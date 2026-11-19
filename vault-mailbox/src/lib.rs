// SPDX-License-Identifier: MIT
//! Relay mailbox for the vault protocol: `/mailbox/<32 hex>` pairs exactly two
//! WebSocket clients and pipes binary frames between them. Payloads are Noise
//! ciphertext; the relay never inspects or logs them, and mailbox ids are only
//! logged as a short hash prefix at debug level.
//!
//! [`Mailboxes::handle`] answers one `GET /mailbox/<id>` request (Origin check,
//! WebSocket upgrade, pairing). Mount it in any hyper server; the `server` feature
//! adds [`serve`], the accept loop behind the `vault-relay` binary.

mod pipe;
#[cfg(feature = "server")]
mod serve;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{CONNECTION, HeaderValue, SEC_WEBSOCKET_ACCEPT, UPGRADE};
use hyper::{Request, Response, StatusCode};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

pub use pipe::parse_id;
#[cfg(feature = "server")]
pub use serve::serve;

/// Mailbox limits and policy. The defaults are those of the signal-web relay.
#[derive(Clone, Debug)]
pub struct MailboxConfig {
    /// Browser origins allowed to open a mailbox (exact match). A request without
    /// an `Origin` header (a native vault) is always allowed. Empty: any origin.
    pub allowed_origins: Vec<String>,
    /// Maximum simultaneously waiting (unpaired) mailboxes.
    pub max_mailboxes_waiting: usize,
    /// An unpaired mailbox is closed (4408) after this long.
    pub mailbox_timeout: Duration,
    /// A paired side is closed (1001) after this long without binary progress.
    pub idle_timeout: Duration,
    /// Frame cap (Noise messages are at most 65535 bytes), bounding per-mailbox
    /// memory to roughly 16 frames per direction.
    pub mailbox_max_frame_bytes: usize,
    /// Maximum open mailbox connections per client address.
    pub max_per_ip: usize,
    /// Maximum open mailbox connections in total.
    pub max_total: usize,
    /// Use the first `X-Forwarded-For` hop as the client address for the per-IP
    /// limit. Only enable behind a reverse proxy that OVERWRITES that header.
    pub trust_proxy: bool,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            max_mailboxes_waiting: 32,
            mailbox_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(120),
            mailbox_max_frame_bytes: 64 * 1024,
            max_per_ip: 8,
            max_total: 64,
            trust_proxy: false,
        }
    }
}

/// Counts open connections, per client address and in total.
#[derive(Default)]
struct Limits {
    inner: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

/// Releases one connection from the limits when dropped.
pub(crate) struct Permit {
    limits: Arc<Limits>,
    ip: Option<IpAddr>,
}

impl Limits {
    fn acquire(self: &Arc<Self>, ip: Option<IpAddr>, cfg: &MailboxConfig) -> Option<Permit> {
        let mut g = self.inner.lock().unwrap();
        let per_ip = ip.and_then(|ip| g.1.get(&ip).copied()).unwrap_or(0);
        if g.0 >= cfg.max_total || per_ip >= cfg.max_per_ip {
            return None;
        }
        g.0 += 1;
        if let Some(ip) = ip {
            *g.1.entry(ip).or_default() += 1;
        }
        Some(Permit {
            limits: self.clone(),
            ip,
        })
    }

    fn total(&self) -> usize {
        self.inner.lock().unwrap().0
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut g = self.limits.inner.lock().unwrap();
        g.0 -= 1;
        if let Some(ip) = self.ip
            && let Some(n) = g.1.get_mut(&ip)
        {
            *n -= 1;
            if *n == 0 {
                g.1.remove(&ip);
            }
        }
    }
}

struct Inner {
    cfg: Arc<MailboxConfig>,
    registry: Arc<pipe::Registry>,
    limits: Arc<Limits>,
    stop: watch::Sender<bool>,
}

/// The mailbox registry. Cheap to clone; clones share all state.
#[derive(Clone)]
pub struct Mailboxes {
    inner: Arc<Inner>,
}

type Resp = Response<Full<Bytes>>;

fn text(status: StatusCode, msg: &'static str) -> Resp {
    let mut r = Response::new(Full::new(Bytes::from_static(msg.as_bytes())));
    *r.status_mut() = status;
    r.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    r
}

fn switching_protocols(key: &str) -> Resp {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let hh = r.headers_mut();
    hh.insert(UPGRADE, HeaderValue::from_static("websocket"));
    hh.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    hh.insert(SEC_WEBSOCKET_ACCEPT, HeaderValue::from_str(key).unwrap());
    r
}

impl Mailboxes {
    pub fn new(cfg: MailboxConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg: Arc::new(cfg),
                registry: Arc::default(),
                limits: Arc::default(),
                stop: watch::channel(false).0,
            }),
        }
    }

    /// Answer `GET /mailbox/<id>` (`id` is the path segment after `/mailbox/`).
    /// Without a peer address only the total limit applies, not the per-IP one;
    /// use [`Mailboxes::handle_from`] when the connection's address is known.
    pub async fn handle(&self, id: &str, req: Request<Incoming>) -> Resp {
        self.handle_from(id, req, None).await
    }

    /// Like [`Mailboxes::handle`], with the TCP peer address of the connection
    /// (replaced by the first `X-Forwarded-For` hop when `trust_proxy` is set).
    pub async fn handle_from(
        &self,
        id: &str,
        mut req: Request<Incoming>,
        peer: Option<IpAddr>,
    ) -> Resp {
        let cfg = &self.inner.cfg;
        let Some(id) = pipe::parse_id(id) else {
            return text(StatusCode::BAD_REQUEST, "invalid mailbox id");
        };
        let key = match ws_handshake(cfg, req.headers()) {
            Ok(k) => k,
            Err((st, m)) => return text(st, m),
        };
        let ip = client_ip(cfg, req.headers(), peer);
        let Some(permit) = self.inner.limits.acquire(ip, cfg) else {
            tracing::info!("mailbox refused: limit");
            return text(StatusCode::TOO_MANY_REQUESTS, "too many connections");
        };
        let (join, slot) = match self.inner.registry.join(id, cfg.max_mailboxes_waiting) {
            Ok(v) => v,
            Err(pipe::Refuse::Full) => {
                tracing::info!("mailbox refused: full");
                return text(StatusCode::CONFLICT, "mailbox full");
            }
            Err(pipe::Refuse::TooManyWaiting) => {
                tracing::info!("mailbox refused: too many waiting");
                return text(StatusCode::TOO_MANY_REQUESTS, "too many waiting mailboxes");
            }
        };
        let on_upgrade = hyper::upgrade::on(&mut req);
        tracing::info!("mailbox open");
        tokio::spawn(pipe::run(
            on_upgrade,
            join,
            id.to_string(),
            slot,
            cfg.clone(),
            permit,
            self.inner.stop.subscribe(),
        ));
        switching_protocols(&key)
    }

    /// Close every open mailbox with 1001 and refuse nothing else; call before
    /// dropping the server so clients see a clean close.
    pub fn shutdown(&self) {
        self.inner.stop.send_replace(true);
    }

    /// Open mailbox connections (waiting or paired).
    pub fn open_connections(&self) -> usize {
        self.inner.limits.total()
    }
}

fn client_ip(cfg: &MailboxConfig, h: &hyper::HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> {
    if cfg.trust_proxy
        && let Some(ip) = h
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        return Some(ip);
    }
    peer
}

/// Validate WebSocket upgrade headers and the Origin allowlist; returns the
/// `Sec-WebSocket-Accept` value.
fn ws_handshake(
    cfg: &MailboxConfig,
    h: &hyper::HeaderMap,
) -> Result<String, (StatusCode, &'static str)> {
    let is_ws = h
        .get(UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        && h.get(CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
        && h.get("sec-websocket-version").is_some_and(|v| v == "13");
    let Some(key) = h
        .get("sec-websocket-key")
        .map(|k| derive_accept_key(k.as_bytes()))
    else {
        return Err((StatusCode::BAD_REQUEST, "websocket upgrade required"));
    };
    if !is_ws {
        return Err((StatusCode::BAD_REQUEST, "websocket upgrade required"));
    }
    if !cfg.allowed_origins.is_empty() {
        let origin = h.get("origin");
        // Native clients (the vault) send no Origin; a browser one must match.
        let ok = match origin {
            None => true,
            Some(o) => o
                .to_str()
                .ok()
                .is_some_and(|o| cfg.allowed_origins.iter().any(|a| a == o)),
        };
        if !ok {
            return Err((StatusCode::FORBIDDEN, "origin not allowed"));
        }
    }
    Ok(key)
}
