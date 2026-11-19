// SPDX-License-Identifier: MIT

//! `vaultctl`: command-line tool to create an encrypted container bound to one app
//! and web origin (`init --app <id> --origin <url>`), add S3 / WebDAV replicas,
//! replicate and restore with the recovery key.
//!
//! The local container lives in `--dir D`. The CLI keeps its secrets (recovery key,
//! replication state, replica credentials) in `D/.vaultctl`, encrypted under a
//! passphrase (Argon2id, 64 MiB, t=3, p=1 -> AES-256-GCM). This is for testing the
//! library; the Android app protects its keys with the Keystore instead.
//!
//! Exit codes: 0 ok, 1 error, 2 diverged replica or rollback detected.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use data_encoding::BASE64;
use rand::RngCore;
use vault_container::backend::{
    ContainerBackend, LocalBackend, S3Backend, S3Config, WebDavBackend, WebDavConfig,
};
use vault_container::engine::{Container, check_format, check_header};
use vault_container::replicate::{
    ObservedMark, ReplicaId, ReplicaOutcome, ReplicaSet, ReplicaState, restore,
};
use vault_container::secrets::{ReplicaCfg, ReplicaEntry, Secrets, load_secrets, save_secrets};
use vault_container::{Error, RecoveryKey};
use zeroize::Zeroizing;

const ENV_PASSPHRASE: &str = "VAULTCTL_PASSPHRASE";
const ENV_RECOVERY_KEY: &str = "VAULTCTL_RECOVERY_KEY";
const ENV_S3_SECRET: &str = "VAULTCTL_S3_SECRET_KEY";
const ENV_DAV_PASSWORD: &str = "VAULTCTL_WEBDAV_PASSWORD";

// ---------------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "vaultctl",
    version,
    about = "Encrypted, replicated container tool (testing CLI)"
)]
struct Cli {
    /// Directory of the local container (and its `.vaultctl` secrets file).
    #[arg(long, global = true)]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new container bound to one app and web origin; prints the recovery
    /// key and a QR code exactly once. (`create` is an alias.)
    #[command(alias = "create")]
    Init {
        /// App id the vault serves, e.g. `signal-web` (`^[a-z0-9][a-z0-9.-]{0,63}$`).
        #[arg(long = "app", value_name = "ID")]
        app: String,
        /// Web origin of the app, e.g. `https://app.example` (https; http only for
        /// localhost, 127.0.0.1 and [::1]; no path). Stored in canonical form.
        #[arg(long, value_name = "URL")]
        origin: String,
    },
    /// Print the container's app binding (app id and origin) and manifest seq.
    Info,
    /// Manage replicas.
    #[command(subcommand)]
    Replica(ReplicaCmd),
    /// Copy the container to every replica.
    Replicate,
    /// Restore into a new, empty directory from a replica using the recovery key
    /// (read from env VAULTCTL_RECOVERY_KEY or a prompt; nothing is created in DIR
    /// unless the restore verifies).
    Restore {
        /// Recovery key. Hidden on purpose: command lines are visible to other local
        /// users and end up in shell history; prefer the prompt or the environment.
        #[arg(long, hide = true)]
        recovery_key: Option<String>,
        #[command(subcommand)]
        from: Kind,
    },
    /// Debug: store VALUE under KEY and commit.
    Put {
        key: String,
        value: String,
        /// VALUE is base64.
        #[arg(long)]
        base64: bool,
    },
    /// Debug: print the value of KEY.
    Get {
        key: String,
        /// Print the value as base64.
        #[arg(long)]
        base64: bool,
    },
    /// Debug: print `key<TAB>value` for all keys starting with PREFIX.
    Scan {
        prefix: String,
        /// Print values as base64.
        #[arg(long)]
        base64: bool,
    },
    /// Commit pending changes (each `put` already commits) and print the sequence number.
    Commit,
}

#[derive(Subcommand)]
enum ReplicaCmd {
    /// Add a replica (a conditional-write probe runs first).
    #[command(subcommand)]
    Add(Kind),
    /// List replicas (no secrets).
    List,
    /// Allow overwriting a diverged replica with this container.
    Adopt { id: String },
}

#[derive(Subcommand)]
enum Kind {
    /// S3-compatible object storage (secret key from VAULTCTL_S3_SECRET_KEY or prompt).
    S3(S3Args),
    /// WebDAV collection, e.g. Nextcloud (password from VAULTCTL_WEBDAV_PASSWORD or prompt).
    #[command(name = "webdav")]
    WebDav(WebDavArgs),
}

#[derive(Args)]
struct S3Args {
    /// Replica id (default: generated).
    #[arg(long)]
    id: Option<String>,
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    region: String,
    #[arg(long)]
    bucket: String,
    #[arg(long, default_value = "")]
    prefix: String,
    #[arg(long)]
    access_key: String,
    #[arg(long)]
    path_style: bool,
    #[arg(long)]
    allow_insecure_http: bool,
    /// Extra PEM root CA file to trust.
    #[arg(long)]
    ca_pem: Option<PathBuf>,
}

#[derive(Args)]
struct WebDavArgs {
    /// Replica id (default: generated).
    #[arg(long)]
    id: Option<String>,
    /// URL of the existing collection holding the container.
    #[arg(long)]
    url: String,
    #[arg(long)]
    username: String,
    #[arg(long)]
    allow_insecure_http: bool,
    /// Extra PEM root CA file to trust.
    #[arg(long)]
    ca_pem: Option<PathBuf>,
}

// ---------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------

#[derive(Debug)]
struct CliError {
    code: u8,
    msg: String,
}

type R<T> = Result<T, CliError>;

fn fail<T>(msg: impl Display) -> R<T> {
    Err(CliError {
        code: 1,
        msg: msg.to_string(),
    })
}

impl From<Error> for CliError {
    fn from(e: Error) -> Self {
        let code = match e {
            Error::Rollback | Error::DivergedReplicas(_) => 2,
            _ => 1,
        };
        CliError {
            code,
            msg: e.to_string(),
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError {
            code: 1,
            msg: format!("i/o error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------------
// Input helpers
// ---------------------------------------------------------------------------------

fn env_or_prompt(env: &str, prompt: &str) -> R<Zeroizing<String>> {
    if let Ok(v) = std::env::var(env) {
        return Ok(Zeroizing::new(v));
    }
    Ok(Zeroizing::new(rpassword::prompt_password(prompt)?))
}

fn passphrase(confirm: bool) -> R<Zeroizing<String>> {
    if let Ok(v) = std::env::var(ENV_PASSPHRASE) {
        if v.is_empty() {
            return fail(format!("{ENV_PASSPHRASE} is empty"));
        }
        return Ok(Zeroizing::new(v));
    }
    let p = Zeroizing::new(rpassword::prompt_password("Passphrase: ")?);
    if p.is_empty() {
        return fail("empty passphrase");
    }
    if confirm {
        let again = Zeroizing::new(rpassword::prompt_password("Repeat passphrase: ")?);
        if *p != *again {
            return fail("passphrases do not match");
        }
    }
    Ok(p)
}

fn ensure_empty_dir(dir: &Path) -> R<()> {
    if dir.exists() {
        if !dir.is_dir() {
            return fail(format!("{} is not a directory", dir.display()));
        }
        if std::fs::read_dir(dir)?.next().is_some() {
            return fail(format!("{} is not empty", dir.display()));
        }
    }
    Ok(())
}

/// Best-effort removal of everything inside `dir` (which was empty or new), and of
/// `dir` itself if it did not exist before.
fn cleanup_dir(dir: &Path, existed: bool) {
    if existed {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                let _ = if p.is_dir() {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                };
            }
        }
    } else {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// True if the URL's authority carries `user[:password]@`.
fn has_userinfo(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#'])
        .next()
        .is_some_and(|a| a.contains('@'))
}

/// The URL without userinfo, for display.
fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let (auth, tail) = rest.split_at(end);
            let host = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
            format!("{scheme}://{host}{tail}")
        }
        None => url.to_string(),
    }
}

fn read_pem(path: &Option<PathBuf>) -> R<Option<String>> {
    path.as_ref()
        .map(|p| std::fs::read_to_string(p).map_err(Into::into))
        .transpose()
}

fn encode(v: &[u8], b64: bool) -> R<String> {
    if b64 {
        Ok(BASE64.encode(v))
    } else {
        String::from_utf8(v.to_vec()).or_else(|_| fail("value is not UTF-8; use --base64"))
    }
}

// ---------------------------------------------------------------------------------
// Replica plumbing
// ---------------------------------------------------------------------------------

fn kind_to_entry(kind: &Kind, existing: &[ReplicaEntry]) -> R<ReplicaEntry> {
    let (id, cfg) = match kind {
        Kind::S3(a) => {
            let secret = env_or_prompt(ENV_S3_SECRET, "S3 secret key: ")?;
            (
                a.id.clone(),
                ReplicaCfg::S3 {
                    endpoint: a.endpoint.clone(),
                    region: a.region.clone(),
                    bucket: a.bucket.clone(),
                    prefix: a.prefix.clone(),
                    access_key: a.access_key.clone(),
                    secret_key: (*secret).clone(),
                    path_style: a.path_style,
                    allow_insecure_http: a.allow_insecure_http,
                    ca_pem: read_pem(&a.ca_pem)?,
                },
            )
        }
        Kind::WebDav(a) => {
            if has_userinfo(&a.url) {
                return fail(
                    "the WebDAV URL must not contain credentials (user:password@); \
                     use --username and VAULTCTL_WEBDAV_PASSWORD",
                );
            }
            let password = env_or_prompt(ENV_DAV_PASSWORD, "WebDAV password: ")?;
            (
                a.id.clone(),
                ReplicaCfg::WebDav {
                    url: a.url.clone(),
                    username: a.username.clone(),
                    password: (*password).clone(),
                    allow_insecure_http: a.allow_insecure_http,
                    ca_pem: read_pem(&a.ca_pem)?,
                },
            )
        }
    };
    let id = id.unwrap_or_else(|| {
        let prefix = match cfg {
            ReplicaCfg::S3 { .. } => "s3",
            ReplicaCfg::WebDav { .. } => "webdav",
        };
        (1..)
            .map(|n| format!("{prefix}-{n}"))
            .find(|c| !existing.iter().any(|e| &e.id == c))
            .expect("unbounded range")
    });
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        return fail("replica id must be non-empty and use only [A-Za-z0-9._-]");
    }
    Ok(ReplicaEntry { id, cfg })
}

/// Builds the backend of a replica; with `probe`, first checks conditional writes.
async fn connect(cfg: &ReplicaCfg, probe: bool) -> R<Arc<dyn ContainerBackend>> {
    match cfg {
        ReplicaCfg::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key,
            secret_key,
            path_style,
            allow_insecure_http,
            ca_pem,
        } => {
            let b = S3Backend::new(S3Config {
                endpoint: endpoint.clone(),
                region: region.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
                access_key: access_key.clone(),
                secret_key: Zeroizing::new(secret_key.clone()),
                path_style: *path_style,
                allow_insecure_http: *allow_insecure_http,
                extra_root_ca_pem: ca_pem.clone(),
            })?;
            if probe {
                b.probe_conditional_writes().await?;
            }
            Ok(Arc::new(b))
        }
        ReplicaCfg::WebDav {
            url,
            username,
            password,
            allow_insecure_http,
            ca_pem,
        } => {
            let b = WebDavBackend::new(WebDavConfig {
                base_url: url.clone(),
                username: username.clone(),
                password: Zeroizing::new(password.clone()),
                allow_insecure_http: *allow_insecure_http,
                extra_root_ca_pem: ca_pem.clone(),
            })?;
            if probe {
                b.probe_conditional_writes().await?;
            }
            Ok(Arc::new(b))
        }
    }
}

/// Checks the format of `dir/header` if there is one (read as a plain file, so
/// nothing is created in `dir`); a missing header is left to the open.
fn check_local_format(dir: &Path) -> R<()> {
    match std::fs::read(dir.join("header")) {
        Ok(bytes) => Ok(check_header(&bytes)?),
        Err(_) => Ok(()),
    }
}

struct Opened {
    c: Container<LocalBackend>,
    secrets: Secrets,
    pass: Zeroizing<String>,
    dir: PathBuf,
}

async fn open(dir: &Path) -> R<Opened> {
    // The header first: a container from before the split must say so, not fail
    // later on its (old-format) recovery key or a passphrase prompt.
    check_local_format(dir)?;
    let pass = passphrase(false)?;
    let secrets = load_secrets(dir, &pass)?;
    let rk = RecoveryKey::parse(&secrets.recovery_key)?;
    let backend = LocalBackend::open(dir).await?;
    let (c, _crk) = Container::open_with_recovery(backend, &rk, secrets.high_water).await?;
    Ok(Opened {
        c,
        secrets,
        pass,
        dir: dir.to_path_buf(),
    })
}

impl Opened {
    fn save(&mut self) -> R<()> {
        self.secrets.high_water = Some(self.c.high_water());
        Ok(save_secrets(&self.dir, &self.secrets, &self.pass)?)
    }
}

// ---------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------

async fn cmd_init(dir: &Path, app: &str, origin: &str) -> R<u8> {
    // Checked before anything is prompted for or written.
    vault_container::Binding::new(app, origin)?;
    ensure_empty_dir(dir)?;
    let pass = passphrase(true)?;
    let existed = dir.exists();
    let backend = LocalBackend::open(dir).await?;
    let (c, rk) = match Container::create(backend, app, origin).await {
        Ok(v) => v,
        Err(e) => {
            cleanup_dir(dir, existed);
            return Err(e.into());
        }
    };
    let secrets = Secrets {
        recovery_key: (*rk.to_display()).clone(),
        high_water: Some(c.high_water()),
        state: ReplicaState::default(),
        replicas: Vec::new(),
        last_observed: Default::default(),
    };
    if let Err(e) = save_secrets(dir, &secrets, &pass) {
        // The recovery key was never shown: leave nothing half-created behind.
        cleanup_dir(dir, existed);
        return fail(format!(
            "could not write the secrets file ({}); nothing was created",
            e
        ));
    }

    let out = std::io::stdout();
    let mut out = out.lock();
    writeln!(out, "Container created in {}", dir.display())?;
    let (app_id, origin) = c.binding();
    writeln!(out, "Bound to app {app_id} at origin {origin}")?;
    writeln!(out)?;
    writeln!(out, "Recovery key: {}", *rk.to_display())?;
    writeln!(out)?;
    if let Ok(qr) = qrcode::QrCode::new(rk.qr_payload().as_bytes()) {
        let art = qr
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        writeln!(out, "{art}")?;
    }
    writeln!(
        out,
        "WARNING: the recovery key and QR code are shown only once. The recovery key alone \
         is enough to restore (and read) your data from any replica, so keep it offline and \
         private, e.g. on paper. Anyone holding it can read your data; if it is lost and \
         this directory is lost too, your data cannot be recovered."
    )?;
    Ok(0)
}

fn show_mark(m: &ObservedMark) -> String {
    let short = |h: &[u8; 32]| data_encoding::HEXLOWER.encode(&h[..6]);
    match m {
        ObservedMark::Manifest { seq, hash } => format!(
            "manifest seq {} hash {}",
            seq.map_or("unreadable".to_string(), |s| s.to_string()),
            short(hash)
        ),
        ObservedMark::Header { hash } => format!("foreign container header hash {}", short(hash)),
    }
}

async fn cmd_replica(dir: &Path, cmd: ReplicaCmd) -> R<u8> {
    let mut o = open(dir).await?;
    match cmd {
        ReplicaCmd::Add(kind) => {
            let entry = kind_to_entry(&kind, &o.secrets.replicas)?;
            if o.secrets.replicas.iter().any(|e| e.id == entry.id) {
                return fail(format!("replica {:?} already exists", entry.id));
            }
            connect(&entry.cfg, true).await?;
            println!(
                "Replica {:?} added (conditional writes verified).",
                entry.id
            );
            o.secrets.replicas.push(entry);
            o.save()?;
        }
        ReplicaCmd::List => {
            if o.secrets.replicas.is_empty() {
                println!("No replicas configured.");
            }
            for e in &o.secrets.replicas {
                let rec = o.secrets.state.replicas.get(&ReplicaId(e.id.clone()));
                let (kind, target) = match &e.cfg {
                    ReplicaCfg::S3 {
                        endpoint,
                        bucket,
                        prefix,
                        ..
                    } => (
                        "s3",
                        format!("{endpoint} bucket={bucket} prefix={prefix:?}"),
                    ),
                    ReplicaCfg::WebDav { url, .. } => ("webdav", redact_url(url)),
                };
                let seen = rec
                    .and_then(|r| r.last_seen)
                    .map_or("never".to_string(), |h| format!("seq {}", h.seq));
                let err = rec
                    .and_then(|r| r.last_error.clone())
                    .map(|e| format!(" last error: {e}"))
                    .unwrap_or_default();
                println!("{}\t{kind}\t{target}\tlast synced: {seen}{err}", e.id);
            }
        }
        ReplicaCmd::Adopt { id } => {
            if !o.secrets.replicas.iter().any(|e| e.id == id) {
                return fail(format!("unknown replica {id:?}"));
            }
            let Some(observed) = o.secrets.last_observed.get(&id).copied() else {
                return fail(format!(
                    "replica {id:?} was not reported as diverged by the last `vaultctl replicate`"
                ));
            };
            println!(
                "This allows the next `vaultctl replicate` to OVERWRITE replica {id:?} with \
                 this container, but only while it still holds: {}. Its current content may \
                 be lost.",
                show_mark(&observed)
            );
            print!("Type the replica id again to confirm: ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            if line.trim() != id {
                return fail("confirmation does not match; nothing changed");
            }
            let rid = ReplicaId(id.clone());
            let cfg = &o
                .secrets
                .replicas
                .iter()
                .find(|e| e.id == id)
                .expect("checked")
                .cfg;
            let backend = connect(cfg, false).await?;
            // `ReplicaSet::new` keeps the state of replicas it is not given.
            let mut set = ReplicaSet::new(
                Arc::new(o.c.backend().clone()),
                vec![(rid.clone(), backend)],
                std::mem::take(&mut o.secrets.state),
            );
            set.adopt_diverged(&rid, observed)?;
            o.secrets.state = set.state().clone();
            o.save()?;
            println!("Confirmed. Run `vaultctl replicate` to overwrite {id:?}.");
        }
    }
    Ok(0)
}

async fn cmd_replicate(dir: &Path) -> R<u8> {
    let mut o = open(dir).await?;
    if o.secrets.replicas.is_empty() {
        return fail("no replicas configured (see `vaultctl replica add`)");
    }
    let mut replicas = Vec::new();
    for e in &o.secrets.replicas {
        replicas.push((ReplicaId(e.id.clone()), connect(&e.cfg, false).await?));
    }
    let manifest_key = o.c.crk().subkeys().manifest;
    let mut set = ReplicaSet::new(
        Arc::new(o.c.backend().clone()),
        replicas,
        std::mem::take(&mut o.secrets.state),
    );
    let (dir_p, pass) = (o.dir.clone(), o.pass.clone());
    let hw = o.c.high_water();
    let mut persist_secrets = |state: &ReplicaState| -> vault_container::Result<()> {
        o.secrets.state = state.clone();
        o.secrets.high_water = Some(hw);
        save_secrets(&dir_p, &o.secrets, &pass)
    };
    let statuses = set
        .replicate_once(&manifest_key, &mut persist_secrets)
        .await;
    o.secrets.state = set.state().clone();
    for s in &statuses {
        match &s.outcome {
            ReplicaOutcome::Diverged { observed, .. } => {
                o.secrets.last_observed.insert(s.id.0.clone(), *observed);
            }
            _ => {
                o.secrets.last_observed.remove(&s.id.0);
            }
        }
    }
    o.save()?;

    let mut code = 0;
    for s in &statuses {
        let id = &s.id.0;
        match &s.outcome {
            ReplicaOutcome::Synced {
                seq,
                uploaded,
                deleted,
            } => {
                println!("{id}: synced at seq {seq} ({uploaded} uploaded, {deleted} deleted)");
            }
            ReplicaOutcome::Rollback {
                found_seq,
                last_seen_seq,
                seq,
            } => {
                let found = found_seq.map_or("no manifest".to_string(), |s| format!("seq {s}"));
                println!(
                    "{id}: ROLLBACK detected: replica served {found} but had seq {last_seen_seq}; \
                     repaired to seq {seq}. The provider may have restored an old snapshot."
                );
                code = code.max(2);
            }
            ReplicaOutcome::Diverged {
                replica_seq,
                primary_seq,
                observed,
            } => {
                let rs = replica_seq.map_or("unreadable".to_string(), |s| format!("seq {s}"));
                println!(
                    "{id}: DIVERGED: replica holds {rs} [{}], this container is at seq {primary_seq}; \
                     nothing was overwritten. If this container is the one to keep, run \
                     `vaultctl replica adopt {id}` (you must type the id to confirm) and \
                     replicate again; otherwise restore from that replica.",
                    show_mark(observed)
                );
                code = code.max(2);
            }
            ReplicaOutcome::Adopted {
                seq,
                replaced,
                rollback,
            } => {
                println!(
                    "{id}: ADOPTED: overwrote [{}] with seq {seq}.",
                    show_mark(replaced)
                );
                if *rollback {
                    println!(
                        "{id}: ROLLBACK detected: the replaced manifest was older than the \
                         one last written there."
                    );
                    code = code.max(2);
                }
            }
            ReplicaOutcome::Failed { error } => {
                println!("{id}: FAILED: {error}");
                code = code.max(1);
            }
        }
    }
    Ok(code)
}

async fn cmd_restore(dir: &Path, recovery_key: Option<String>, from: Kind) -> R<u8> {
    ensure_empty_dir(dir)?;
    let rk_text = match recovery_key {
        Some(k) => Zeroizing::new(k),
        None => env_or_prompt(ENV_RECOVERY_KEY, "Recovery key: ")?,
    };
    let entry = kind_to_entry(&from, &[])?;
    let pass = passphrase(true)?;
    // The source stays configured as a replica: probe it as `replica add` does.
    let source = connect(&entry.cfg, true).await?;
    // A source from before the split reports the format before its (old-format)
    // recovery key is parsed; an unreadable or missing header is left to `restore`.
    if let Err(e @ (Error::PreV1Container | Error::UnsupportedFormatVersion(_))) =
        check_format(source.as_ref()).await
    {
        return Err(e.into());
    }
    let rk = RecoveryKey::parse(&rk_text)?;
    let rid = ReplicaId(entry.id.clone());

    // Everything is built in a sibling staging directory and moved into place only after
    // the restore verified and the secrets file was written; on any failure it is removed
    // and DIR is left untouched.
    let parent = dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tag = [0u8; 6];
    rand::rng().fill_bytes(&mut tag);
    let name = dir
        .file_name()
        .map_or("vault".into(), |n| n.to_string_lossy().into_owned());
    let staging = parent.join(format!(
        ".{name}.restoring-{}",
        data_encoding::HEXLOWER.encode(&tag)
    ));
    let result = restore_into(&staging, &rk, entry, source, rid, &pass).await;
    let finish = result.and_then(|()| {
        if dir.exists() {
            std::fs::remove_dir(dir)?; // verified empty above
        }
        std::fs::rename(&staging, dir)?;
        Ok(())
    });
    if let Err(e) = finish {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(CliError {
            code: e.code,
            msg: format!(
                "{}; restore failed and nothing was created in {}",
                e.msg,
                dir.display()
            ),
        });
    }
    // Make the rename itself durable (DIR now holds the restored vault either way).
    if let Err(e) = vault_container::secrets::sync_dir(parent) {
        return fail(format!(
            "restored into {}, but syncing its parent directory failed: {e}",
            dir.display()
        ));
    }
    println!("Secrets file written; the source is kept as a replica.");
    Ok(0)
}

async fn restore_into(
    staging: &Path,
    rk: &RecoveryKey,
    entry: ReplicaEntry,
    source: Arc<dyn ContainerBackend>,
    rid: ReplicaId,
    pass: &str,
) -> R<()> {
    let target = LocalBackend::open(staging).await?;
    let report = restore(
        &[(rid, source)],
        rk,
        &ReplicaId("local-target".into()),
        &target,
        None,
    )
    .await?;
    let ids = |v: &[ReplicaId]| {
        v.iter()
            .map(|i| i.0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "Restored seq {} from replica {:?}.",
        report.seq, report.chosen.0
    );
    println!("  stale/rolled back: [{}]", ids(&report.rollbacks));
    println!("  diverged:          [{}]", ids(&report.diverged));
    println!("  unreachable:       [{}]", ids(&report.unreachable));
    println!("  broken:            [{}]", ids(&report.broken));

    let (c, _crk) = Container::open_with_recovery(target, rk, None).await?;
    let secrets = Secrets {
        recovery_key: (*rk.to_display()).clone(),
        high_water: Some(c.high_water()),
        state: report.state,
        replicas: vec![entry],
        last_observed: Default::default(),
    };
    Ok(save_secrets(staging, &secrets, pass)?)
}

async fn cmd_data(dir: &Path, cmd: Cmd) -> R<u8> {
    let mut o = open(dir).await?;
    match cmd {
        Cmd::Put { key, value, base64 } => {
            let v = if base64 {
                Zeroizing::new(
                    BASE64
                        .decode(value.as_bytes())
                        .or_else(|_| fail("VALUE is not valid base64"))?,
                )
            } else {
                Zeroizing::new(value.into_bytes())
            };
            o.c.put(key.as_bytes(), &v)?;
            let seq = o.c.commit().await?;
            o.save()?;
            println!("committed seq {seq}");
        }
        Cmd::Get { key, base64 } => match o.c.get(key.as_bytes()) {
            Some(v) => {
                let v = Zeroizing::new(v);
                println!("{}", encode(&v, base64)?);
            }
            None => return fail(format!("key {key:?} not found")),
        },
        Cmd::Scan { prefix, base64 } => {
            for (k, v) in o.c.scan_prefix(prefix.as_bytes()) {
                let v = Zeroizing::new(v);
                println!("{}\t{}", encode(&k, false)?, encode(&v, base64)?);
            }
        }
        Cmd::Commit => {
            let seq = o.c.commit().await?;
            o.save()?;
            println!("committed seq {seq}");
        }
        Cmd::Info => {
            let (app_id, origin) = o.c.binding();
            println!("app: {app_id}");
            println!("origin: {origin}");
            println!("seq: {}", o.c.seq());
        }
        _ => unreachable!("handled by caller"),
    }
    Ok(0)
}

async fn dispatch(cli: Cli) -> R<u8> {
    let Some(dir) = cli.dir else {
        return fail("--dir <DIR> is required");
    };
    match cli.cmd {
        Cmd::Init { app, origin } => cmd_init(&dir, &app, &origin).await,
        Cmd::Replica(r) => cmd_replica(&dir, r).await,
        Cmd::Replicate => cmd_replicate(&dir).await,
        Cmd::Restore { recovery_key, from } => cmd_restore(&dir, recovery_key, from).await,
        other => cmd_data(&dir, other).await,
    }
}

/// Runs the CLI with the given arguments (including the program name).
fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { 1 } else { 0 });
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(dispatch(cli)) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {}", e.msg);
            ExitCode::from(e.code)
        }
    }
}

fn main() -> ExitCode {
    run(std::env::args_os())
}
