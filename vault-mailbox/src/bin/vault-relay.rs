// SPDX-License-Identifier: MIT
//! `vault-relay`: serves only `GET /mailbox/<32hex>` and `GET /healthz`, plus,
//! with `--static-dir` (off by default), a page's files read-only on the same port
//! (see `vault_mailbox::StaticSite`; the `vault-todo-demo` image).
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
    /// Behind one reverse proxy: count the per-IP limit on the rightmost
    /// X-Forwarded-For entry (the one that proxy appended), honoured only when the
    /// connection comes from a loopback or private address (the proxy). Exactly
    /// one proxy hop; see docs/deploy-todo-demo.md.
    #[arg(long)]
    trust_proxy: bool,
    /// Also serve this directory's files (GET/HEAD, read-only, no listings) on
    /// every path other than /mailbox/ and /healthz, with the page's CSP, and
    /// /config.json naming the relay. Needs --public-origin.
    #[arg(long, value_name = "DIR", env = STATIC_DIR_ENV, value_parser = static_dir_path)]
    static_dir: Option<PathBuf>,
    /// The origin the page is served at by the TLS proxy in front (e.g.
    /// https://todo.example). The default for --allow-origin, and the relay's
    /// WebSocket origin on the same host unless --relay-origin is given.
    #[arg(long, value_name = "ORIGIN", env = PUBLIC_ORIGIN_ENV, value_parser = public_origin)]
    public_origin: Option<String>,
    /// The relay's origin as the page dials it (wss://host[:port], or its https
    /// form; ws/http only for loopback) when it is not the --public-origin host.
    #[arg(long, value_name = "ORIGIN", env = RELAY_ORIGIN_ENV, value_parser = relay_origin_arg)]
    relay_origin: Option<String>,
}

const ALLOW_ORIGIN_ENV: &str = "VAULT_RELAY_ALLOW_ORIGIN";
const TLS_CERT_ENV: &str = "VAULT_RELAY_TLS_CERT";
const TLS_KEY_ENV: &str = "VAULT_RELAY_TLS_KEY";
const STATIC_DIR_ENV: &str = "VAULT_RELAY_STATIC_DIR";
const PUBLIC_ORIGIN_ENV: &str = "PUBLIC_ORIGIN";
const RELAY_ORIGIN_ENV: &str = "RELAY_ORIGIN";

/// A set but empty value is an error (as for the TLS paths), never "off".
fn non_empty<'a>(value: &'a str, flag: &str, env: &str) -> Result<&'a str, String> {
    if value.trim().is_empty() {
        return Err(format!(
            "{flag} (or {env}) is set but empty: give a value, or unset {env}"
        ));
    }
    Ok(value.trim())
}

fn static_dir_path(value: &str) -> Result<PathBuf, String> {
    non_empty(value, "--static-dir", STATIC_DIR_ENV).map(PathBuf::from)
}

/// The page's origin, canonical (https, or http for loopback only).
fn public_origin(value: &str) -> Result<String, String> {
    let v = non_empty(value, "--public-origin", PUBLIC_ORIGIN_ENV)?;
    vault_proto::binding::normalize_origin(v).map_err(|e| format!("invalid page origin {v:?}: {e}"))
}

/// The relay's WebSocket origin, canonical: `wss://host[:port]` (or `ws://` for
/// loopback); given as wss/ws or as its https/http form.
fn relay_origin_arg(value: &str) -> Result<String, String> {
    let v = non_empty(value, "--relay-origin", RELAY_ORIGIN_ENV)?;
    let lower = v.to_ascii_lowercase();
    let as_http = if let Some(rest) = lower.strip_prefix("wss://") {
        format!("https://{}", &v[v.len() - rest.len()..])
    } else if let Some(rest) = lower.strip_prefix("ws://") {
        format!("http://{}", &v[v.len() - rest.len()..])
    } else {
        v.to_string()
    };
    let origin = vault_proto::binding::normalize_origin(&as_http)
        .map_err(|e| format!("invalid relay origin {v:?}: {e}"))?;
    Ok(ws_form(&origin))
}

/// `https://h` -> `wss://h`, `http://h` -> `ws://h` (vault-client's mapping).
fn ws_form(origin: &str) -> String {
    match origin.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => format!("ws://{}", origin.strip_prefix("http://").unwrap_or(origin)),
    }
}

/// The static site for `--static-dir`, or None; an error message for the user.
fn static_site(args: &Args) -> Result<Option<vault_mailbox::StaticSite>, String> {
    let Some(dir) = &args.static_dir else {
        return Ok(None);
    };
    let Some(page) = &args.public_origin else {
        return Err(format!(
            "--static-dir needs the page's public origin: pass --public-origin <ORIGIN> or set \
             {PUBLIC_ORIGIN_ENV}=https://<host> (the origin the TLS proxy serves the page at)"
        ));
    };
    let relay = args.relay_origin.clone().unwrap_or_else(|| ws_form(page));
    let page_tls = page.starts_with("https://");
    let relay_tls = relay.starts_with("wss://");
    if page_tls != relay_tls {
        return Err(format!(
            "the page {page} and the relay {relay} must both use TLS or both not (an https \
             page cannot dial ws:, and a wss: relay for an http page is a mistaken setup)"
        ));
    }
    vault_mailbox::StaticSite::new(dir, page, &relay).map(Some)
}

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
    let site = match static_site(&args) {
        Ok(s) => s.map(std::sync::Arc::new),
        Err(e) => {
            eprintln!("vault-relay: {e}");
            std::process::exit(2);
        }
    };
    // The page's origin is the default allowlist.
    let raw_allowed = match (&args.public_origin, args.allow_origin.is_empty()) {
        (Some(page), true) => vec![page.clone()],
        _ => args.allow_origin.clone(),
    };
    let allowed = match allowed_origins(&raw_allowed) {
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
    if let (Some(_), Some(page)) = (&site, &args.public_origin)
        && !allowed.contains(page)
    {
        tracing::warn!(
            "the page origin {page} is not in --allow-origin: the page cannot open mailboxes"
        );
    }
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
    if let Some(site) = &site {
        tracing::info!(dir = %site.root().display(), csp = site.csp(), "serving static files");
    }
    tracing::info!(%addr, tls = tls.is_some(), allowed_origins = ?allowed, "vault-relay listening");
    let opts = vault_mailbox::ServeOptions {
        tls,
        static_site: site,
    };
    vault_mailbox::serve_with(listener, Mailboxes::new(cfg), opts, shutdown_signal()).await;
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
