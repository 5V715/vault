// SPDX-License-Identifier: MIT

//! `vault-dev`: a laptop-hosted test vault for the browser app, for use
//! before the Android vault exists.
//!
//! `vault-dev serve --dir D --pair "<qr text>"` serves the container in `D` (created
//! with `vaultctl init --app <id> --origin <url>`), dials the relay named in the
//! pairing text, runs the vault side of the pairing handshake and asks on the
//! terminal whether to approve the browser (app id, origin, relay host, device
//! name, 6-digit code, prompt number; answer `y <number>` or `n` on stdin). A
//! pairing text for another app or origin than the container's binding is refused
//! before any prompt (the browser is told `WrongApp`). One pairing text serves one
//! session; run the command again for the next.
//!
//! The passphrase (the one given to `vaultctl init`) is prompted for without echo.
//! For automation, `--passphrase-file PATH` (or, on Unix only, `--passphrase-fd N`)
//! read it instead; it is never accepted on the command line or in the environment.
//! The vault is served locked: the pairing handshake uses a fresh random Noise key
//! per run and the container is opened (with the recovery key from the
//! passphrase-sealed `D/.vaultctl`) only when the prompt is approved. A
//! `D/.vault-dev-identity` file left by earlier versions is obsolete: it is ignored
//! and may be deleted (no migration is needed). Development only; see
//! `docs/vault/README.md`.
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
use vault_container::backend::LocalBackend;
use vault_container::engine::check_header;
use vault_container::secrets::{load_secrets, save_secrets};
use vault_container::{Error as ContainerError, RecoveryKey};
use vault_proto::PairOffer;
use vault_server::{
    ServerConfig, ServerError, SharedBackend, TerminalApprover, UnlockMaterial, VaultServer,
};
use zeroize::Zeroizing;

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
    /// Container directory created by `vaultctl init`.
    #[arg(long)]
    dir: PathBuf,
    /// Pairing text from the web app (`vault-pair:v1:...`).
    #[arg(long)]
    pair: String,
    /// Read the passphrase from this file (one line) instead of prompting.
    #[cfg_attr(unix, arg(long, conflicts_with = "passphrase_fd"))]
    #[cfg_attr(not(unix), arg(long))]
    passphrase_file: Option<PathBuf>,
    /// Read the passphrase from this open file descriptor instead of prompting
    /// (Unix only: it re-opens `/dev/fd/N`).
    #[cfg(unix)]
    #[arg(long, value_name = "FD")]
    passphrase_fd: Option<u32>,
    /// Also trust the CA certificate(s) in this PEM file when dialing a `wss://`
    /// relay (on top of the public roots), e.g. the local CA of a LAN relay.
    #[arg(long, value_name = "PEM")]
    ca_file: Option<PathBuf>,
}

type R<T> = Result<T, String>;

fn err<T>(msg: impl Display) -> R<T> {
    Err(msg.to_string())
}

fn read_passphrase(a: &ServeArgs) -> R<Zeroizing<String>> {
    let raw = if let Some(p) = &a.passphrase_file {
        read_secret_file(p)?
    } else if let Some(raw) = read_passphrase_fd(a) {
        raw?
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

/// `--passphrase-fd N`: `/dev/fd/N` re-opens the inherited descriptor without unsafe code.
#[cfg(unix)]
fn read_passphrase_fd(a: &ServeArgs) -> Option<R<Zeroizing<String>>> {
    a.passphrase_fd
        .map(|fd| read_secret_file(Path::new(&format!("/dev/fd/{fd}"))))
}

/// The flag does not exist on Windows.
#[cfg(not(unix))]
fn read_passphrase_fd(_: &ServeArgs) -> Option<R<Zeroizing<String>>> {
    None
}

fn read_secret_file(p: &Path) -> R<Zeroizing<String>> {
    let mut s = Zeroizing::new(String::new());
    std::fs::File::open(p)
        .and_then(|mut f| f.read_to_string(&mut s))
        .map_err(|e| format!("cannot read passphrase: {e}"))?;
    Ok(s)
}

fn server_error(e: ServerError) -> String {
    match e {
        // The approver has printed the warning (`MAILBOX_TAKEN_WARNING`).
        ServerError::MailboxTaken => {
            "the relay mailbox is already taken (see the warning above)".into()
        }
        ServerError::Relay(_) => format!("cannot reach the relay: {e}"),
        ServerError::Tls(m) => format!("--ca-file: {m}"),
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
    // The header first: a container from before the split must say so, not fail
    // on its (old-format) recovery key, a passphrase or the relay.
    // Read as a plain file (nothing is created in the directory); a missing header
    // is left to the open below.
    if let Ok(bytes) = std::fs::read(a.dir.join("header")) {
        check_header(&bytes).map_err(|e| format!("cannot open container: {e}"))?;
    }
    let extra_ca = match &a.ca_file {
        Some(p) => Some(
            std::fs::read(p).map_err(|e| format!("cannot read --ca-file {}: {e}", p.display()))?,
        ),
        None => None,
    };
    let pass = read_passphrase(&a)?;
    let mut secrets = load_secrets(&a.dir, &pass).map_err(|e| match e {
        ContainerError::BadSecrets => "wrong passphrase (or not a vaultctl directory)".to_string(),
        other => other.to_string(),
    })?;
    // Checked now so that a damaged key is reported before anything is dialed;
    // parsed again (and dropped) at each approval.
    RecoveryKey::parse(&secrets.recovery_key).map_err(|e| e.to_string())?;
    let backend: SharedBackend = Arc::new(
        LocalBackend::open(&a.dir)
            .await
            .map_err(|e| format!("cannot open container: {e}"))?,
    );
    // The container is opened only at approval, with these passphrase-derived secrets.
    let unlock = {
        let rk_text = Zeroizing::new(secrets.recovery_key.clone());
        let high_water = secrets.high_water;
        move || match RecoveryKey::parse(&rk_text) {
            Ok(key) => UnlockMaterial::RecoveryKey { key, high_water },
            Err(_) => UnlockMaterial::Unlocked,
        }
    };
    let approver = Arc::new(TerminalApprover::new().with_unlock(unlock));

    let config = ServerConfig {
        extra_ca_pem: extra_ca,
        ..ServerConfig::default()
    };
    let server = VaultServer::new_locked(backend, approver, config)
        .await
        .map_err(|e| match e {
            ServerError::Container(e) => format!("cannot open container: {e}"),
            other => server_error(other),
        })?;

    let (app_id, origin) = server.binding();
    let bound = offer.app_id == app_id && offer.origin == origin;
    if !bound {
        eprintln!(
            "WARNING: this vault is bound to app {} at {}, but the pairing text is for app {} at {}. \
             The session will be refused (the browser is told: wrong app).",
            app_id.escape_debug(),
            origin.escape_debug(),
            offer.app_id.escape_debug(),
            offer.origin.escape_debug(),
        );
    }

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
