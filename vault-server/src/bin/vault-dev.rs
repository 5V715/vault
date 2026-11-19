// SPDX-License-Identifier: MIT

//! `vault-dev`: a laptop-hosted test vault for the browser app, for use
//! before the Android vault exists.
//!
//! `vault-dev serve --dir D --pair "<qr text>"` opens the container in `D` (created
//! with `vaultctl create`), dials the relay named in the pairing text, runs the vault
//! side of the pairing handshake and asks on the terminal whether to approve the
//! browser (device name, 6-digit code, prompt number; answer `y <number>` or `n` on
//! stdin). One pairing text serves one session; run the command again for the next.
//!
//! The passphrase (the one given to `vaultctl create`) is prompted for without echo.
//! For automation, `--passphrase-file PATH` or `--passphrase-fd N` read it instead;
//! it is never accepted on the command line or in the environment. The vault's
//! long-term Noise key is kept in `D/.vault-dev-identity`, sealed under the same
//! passphrase. Development only; see `docs/vault/README.md`.
//!
//! When the session ends the container is compacted if the server's
//! [`vault_server::CompactionPolicy`] thresholds are crossed, and the high-water
//! mark is saved (also on Ctrl-C).
//!
//! Exit codes: 0 session ended (approved, denied or closed), 1 error.

use std::fmt::Display;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use rand::RngCore;
use vault_container::backend::LocalBackend;
use vault_container::engine::Container;
use vault_container::secrets::{load_secrets, save_sealed, save_secrets, unseal};
use vault_container::{Error as ContainerError, RecoveryKey};
use vault_proto::PairOffer;
use vault_server::{ServerConfig, ServerError, SharedBackend, TerminalApprover, VaultServer};
use zeroize::Zeroizing;

const IDENTITY_FILE: &str = ".vault-dev-identity";
const IDENTITY_MAGIC: &[u8] = b"vault-dev-identity-v1";

#[derive(Parser)]
#[command(
    name = "vault-dev",
    version,
    about = "Laptop-hosted test vault for the web app (development only)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve one browser session from a container directory.
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Container directory created by `vaultctl create`.
    #[arg(long)]
    dir: PathBuf,
    /// Pairing text from the web app (`vault-pair:v1:...`).
    #[arg(long)]
    pair: String,
    /// Read the passphrase from this file (one line) instead of prompting.
    #[arg(long, conflicts_with = "passphrase_fd")]
    passphrase_file: Option<PathBuf>,
    /// Read the passphrase from this open file descriptor instead of prompting.
    #[arg(long, value_name = "FD")]
    passphrase_fd: Option<u32>,
}

type R<T> = Result<T, String>;

fn err<T>(msg: impl Display) -> R<T> {
    Err(msg.to_string())
}

fn read_passphrase(a: &ServeArgs) -> R<Zeroizing<String>> {
    let raw = if let Some(p) = &a.passphrase_file {
        read_secret_file(p)?
    } else if let Some(fd) = a.passphrase_fd {
        // `/dev/fd/N` re-opens the inherited descriptor without unsafe code.
        read_secret_file(Path::new(&format!("/dev/fd/{fd}")))?
    } else {
        Zeroizing::new(
            rpassword::prompt_password("Passphrase: ").map_err(|e| format!("prompt: {e}"))?,
        )
    };
    let line = raw.strip_suffix('\n').unwrap_or(&raw);
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        return err("empty passphrase");
    }
    Ok(Zeroizing::new(line.to_owned()))
}

fn read_secret_file(p: &Path) -> R<Zeroizing<String>> {
    let mut s = Zeroizing::new(String::new());
    std::fs::File::open(p)
        .and_then(|mut f| f.read_to_string(&mut s))
        .map_err(|e| format!("cannot read passphrase: {e}"))?;
    Ok(s)
}

/// The vault's long-term Noise static key, created on first use.
fn load_identity(dir: &Path, pass: &str) -> R<Zeroizing<[u8; 32]>> {
    let path = dir.join(IDENTITY_FILE);
    match std::fs::read(&path) {
        Ok(blob) => {
            let plain = unseal(IDENTITY_MAGIC, &blob, pass).map_err(|e| e.to_string())?;
            let key: [u8; 32] = plain
                .as_slice()
                .try_into()
                .map_err(|_| "corrupted vault-dev identity file".to_string())?;
            Ok(Zeroizing::new(key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut key = Zeroizing::new([0u8; 32]);
            rand::rng().fill_bytes(&mut *key);
            save_sealed(dir, IDENTITY_FILE, IDENTITY_MAGIC, &*key, pass)
                .map_err(|e| format!("cannot write identity file: {e}"))?;
            Ok(key)
        }
        Err(e) => err(format!("cannot read identity file: {e}")),
    }
}

/// Printed when the mailbox was already taken: someone else may hold the pairing text.
const MAILBOX_TAKEN_WARNING: &str = "\
!!! WARNING !!!
This pairing code was already used by another device: the relay mailbox is taken.
Someone else may have read the pairing text and may be posing as your vault.
Do NOT press \"Codes match\" in the browser. Press Cancel, close the tab and
start over with a new code (and do not share the pairing text).";

/// Host (and port) of the relay URL in the pairing text, for the prompt.
fn relay_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest)
}

fn server_error(e: ServerError) -> String {
    match e {
        ServerError::MailboxTaken => MAILBOX_TAKEN_WARNING.into(),
        ServerError::Relay(_) => format!("cannot reach the relay: {e}"),
        ServerError::BadOffer => "pairing text names a relay this vault will not use".into(),
        other => other.to_string(),
    }
}

fn save_high_water(
    dir: &Path,
    secrets: &mut vault_container::secrets::Secrets,
    pass: &str,
    hw: Option<vault_container::engine::HighWater>,
) -> R<()> {
    if let Some(hw) = hw {
        secrets.high_water = Some(hw);
        save_secrets(dir, secrets, pass).map_err(|e| format!("cannot save state: {e}"))?;
    }
    Ok(())
}

async fn serve(a: ServeArgs) -> R<()> {
    let offer = PairOffer::from_qr(a.pair.trim()).map_err(|e| format!("bad pairing text: {e}"))?;
    let pass = read_passphrase(&a)?;
    let mut secrets = load_secrets(&a.dir, &pass).map_err(|e| match e {
        ContainerError::BadSecrets => "wrong passphrase (or not a vaultctl directory)".to_string(),
        other => other.to_string(),
    })?;
    let rk = RecoveryKey::parse(&secrets.recovery_key).map_err(|e| e.to_string())?;
    let backend: SharedBackend = Arc::new(
        LocalBackend::open(&a.dir)
            .await
            .map_err(|e| format!("cannot open container: {e}"))?,
    );
    let (container, _crk) = Container::open_with_recovery(backend, &rk, secrets.high_water)
        .await
        .map_err(|e| format!("cannot open container: {e}"))?;
    let identity = load_identity(&a.dir, &pass)?;

    let server = VaultServer::new(
        container,
        identity,
        Arc::new(TerminalApprover::new().with_relay_host(relay_host(&offer.relay_url))),
        ServerConfig::default(),
    )
    .map_err(server_error)?;

    eprintln!("Connecting to the relay...");
    let handle = server.serve_offer(offer).await.map_err(server_error)?;
    eprintln!(
        "Connected. Waiting for the browser; the code it shows must match the one printed here."
    );
    let reason = tokio::select! {
        r = handle.finished() => r,
        _ = tokio::signal::ctrl_c() => {
            // Committed state stays; only staged writes are dropped by the lock.
            let hw = server.high_water().await;
            server.lock().await;
            save_high_water(&a.dir, &mut secrets, &pass, hw)?;
            println!("session ended: interrupted");
            return Ok(());
        }
    };
    // Clean exit: compact if the policy says so (the server also checks after
    // every session), then persist the high-water mark of the final manifest.
    if let Err(e) = server.compact_if_needed().await {
        eprintln!("vault-dev: compaction failed (data unchanged): {e}");
    }
    let hw = server.high_water().await;
    save_high_water(&a.dir, &mut secrets, &pass, hw)?;
    println!("session ended: {reason}");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("vault-dev: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Cmd::Serve(args) = cli.cmd;
    match rt.block_on(serve(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vault-dev: {e}");
            ExitCode::FAILURE
        }
    }
}
