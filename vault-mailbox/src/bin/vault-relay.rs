// SPDX-License-Identifier: MIT
//! `vault-relay`: serves only `GET /mailbox/<32hex>` and `GET /healthz`.
//! Plain HTTP by default; terminate TLS in a reverse proxy (and pass `--trust-proxy`
//! behind it), or give `--tls-cert` and `--tls-key` to serve HTTPS/`wss://` directly
//! (rustls, TLS 1.2 or newer, no client certificates).
//!
//! It refuses to start without at least one allowed browser origin
//! (`--allow-origin` or `VAULT_RELAY_ALLOW_ORIGIN`), so a relay is never open to
//! every website by accident. Each origin must follow the app-binding origin rules
//! (`vault_proto::binding::normalize_origin`) and is stored in canonical form.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use vault_mailbox::{MailboxConfig, Mailboxes};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Browser origin allowed to open a mailbox (the app's page origin, e.g.
    /// https://app.example); repeat for several. At least one is required. Must be
    /// https, or http only for localhost, 127.0.0.1 and [::1]; no path, query or
    /// fragment. Requests without an Origin (native vaults) always pass.
    /// The environment variable takes a comma-separated list and is ignored when
    /// the flag is given.
    #[arg(
        long = "allow-origin",
        value_name = "ORIGIN",
        env = ALLOW_ORIGIN_ENV,
        value_delimiter = ',',
        hide_env_values = true
    )]
    allow_origin: Vec<String>,
    /// Maximum simultaneously waiting (unpaired) mailboxes.
    #[arg(long, default_value_t = MailboxConfig::default().max_mailboxes_waiting)]
    max_waiting: usize,
    /// Seconds before an unpaired mailbox is closed.
    #[arg(long, default_value_t = MailboxConfig::default().mailbox_timeout.as_secs())]
    pair_timeout_s: u64,
    /// Seconds without binary traffic before a paired side is closed.
    #[arg(long, default_value_t = MailboxConfig::default().idle_timeout.as_secs())]
    idle_timeout_s: u64,
    /// Maximum bytes of one frame.
    #[arg(long, default_value_t = MailboxConfig::default().mailbox_max_frame_bytes)]
    max_frame_bytes: usize,
    /// Maximum open mailbox connections per client address.
    #[arg(long, default_value_t = MailboxConfig::default().max_per_ip)]
    max_per_ip: usize,
    /// Maximum open mailbox connections in total.
    #[arg(long, default_value_t = MailboxConfig::default().max_total)]
    max_total: usize,
    /// Maximum concurrent HTTP connections (including not yet upgraded ones).
    #[arg(long, default_value_t = MailboxConfig::default().max_connections)]
    max_connections: usize,
    /// Seconds without any read or write progress before a plain HTTP connection is cut.
    #[arg(long, default_value_t = MailboxConfig::default().static_timeout.as_secs())]
    stall_timeout_s: u64,
    /// PEM certificate chain (leaf first) to serve HTTPS/wss directly. Needs
    /// `--tls-key`; without both, the relay speaks plain HTTP.
    #[arg(long, value_name = "PEM", env = TLS_CERT_ENV, value_parser = tls_cert_path)]
    tls_cert: Option<PathBuf>,
    /// PEM private key for `--tls-cert`. Keep it readable by the relay only (0600).
    #[arg(long, value_name = "PEM", env = TLS_KEY_ENV, value_parser = tls_key_path)]
    tls_key: Option<PathBuf>,
    /// Take the client address from the first X-Forwarded-For hop (only behind a
    /// reverse proxy that overwrites that header).
    #[arg(long)]
    trust_proxy: bool,
}

const ALLOW_ORIGIN_ENV: &str = "VAULT_RELAY_ALLOW_ORIGIN";
const TLS_CERT_ENV: &str = "VAULT_RELAY_TLS_CERT";
const TLS_KEY_ENV: &str = "VAULT_RELAY_TLS_KEY";

/// A TLS file path from a flag or its environment variable. An empty value (a set
/// but empty variable, as from `VAULT_RELAY_TLS_CERT=` in a unit or compose file)
/// is an error: falling back to plain HTTP there would be silent.
fn tls_path(value: &str, flag: &str, env: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err(format!(
            "{flag} (or {env}) is set but empty: give the PEM file path, or unset {env} \
             to serve plain HTTP"
        ));
    }
    Ok(PathBuf::from(value))
}

fn tls_cert_path(value: &str) -> Result<PathBuf, String> {
    tls_path(value, "--tls-cert", TLS_CERT_ENV)
}

fn tls_key_path(value: &str) -> Result<PathBuf, String> {
    tls_path(value, "--tls-key", TLS_KEY_ENV)
}

/// Loads the TLS config when `--tls-cert`/`--tls-key` (or their environment
/// variables) are given (empty values never get here: `tls_path`). An error
/// message for the user otherwise; it never contains key material.
fn load_tls(
    cert: &Option<PathBuf>,
    key: &Option<PathBuf>,
) -> Result<Option<std::sync::Arc<rustls::ServerConfig>>, String> {
    let (cert, key) = match (cert.clone(), key.clone()) {
        (None, None) => return Ok(None),
        (Some(c), Some(k)) => (c, k),
        _ => {
            return Err(format!(
                "--tls-cert and --tls-key must be given together (or {TLS_CERT_ENV} and \
                 {TLS_KEY_ENV})"
            ));
        }
    };
    warn_if_key_exposed(&key);
    let read = |what: &str, p: &Path| {
        std::fs::read(p).map_err(|e| format!("cannot read the TLS {what} {}: {e}", p.display()))
    };
    let cert_pem = read("certificate", &cert)?;
    let key_pem = read("key", &key)?;
    vault_mailbox::tls_server_config(&cert_pem, &key_pem)
        .map(Some)
        .map_err(|e| format!("TLS: {e}"))
}

/// Warns (does not fail) when the key file is readable by group or others.
fn warn_if_key_exposed(key: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(key) {
            let mode = m.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(
                    "TLS key file {} is readable by other users (mode {mode:04o}); run chmod 600 on it",
                    key.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = key;
}

/// Validates and normalizes the allowlist; an error message for the user otherwise.
fn allowed_origins(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for v in raw.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
        let o = vault_proto::binding::normalize_origin(v)
            .map_err(|e| format!("invalid allowed origin {v:?}: {e}"))?;
        if !out.contains(&o) {
            out.push(o);
        }
    }
    if out.is_empty() {
        return Err(format!(
            "no browser origin is allowed: pass --allow-origin <ORIGIN> (repeatable) or set \
             {ALLOW_ORIGIN_ENV}=<ORIGIN>[,<ORIGIN>...] with the page origin of every app \
             that uses this relay, e.g. https://app.example"
        ));
    }
    Ok(out)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let allowed = match allowed_origins(&args.allow_origin) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vault-relay: {e}");
            std::process::exit(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let tls = match load_tls(&args.tls_cert, &args.tls_key) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("vault-relay: {e}");
            std::process::exit(2);
        }
    };
    let cfg = MailboxConfig {
        allowed_origins: allowed.clone(),
        max_mailboxes_waiting: args.max_waiting,
        mailbox_timeout: Duration::from_secs(args.pair_timeout_s),
        idle_timeout: Duration::from_secs(args.idle_timeout_s),
        mailbox_max_frame_bytes: args.max_frame_bytes,
        max_per_ip: args.max_per_ip,
        max_total: args.max_total,
        trust_proxy: args.trust_proxy,
        max_connections: args.max_connections,
        static_timeout: Duration::from_secs(args.stall_timeout_s),
    };
    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    let addr = listener.local_addr().unwrap_or(args.listen);
    tracing::info!(%addr, tls = tls.is_some(), allowed_origins = ?allowed, "vault-relay listening");
    vault_mailbox::serve_tls(listener, Mailboxes::new(cfg), tls, shutdown_signal()).await;
    tracing::info!("vault-relay stopped");
}

/// Ctrl-C everywhere; SIGTERM too on Unix (container stop).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
