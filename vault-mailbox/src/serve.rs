// SPDX-License-Identifier: MIT
//! Minimal accept loop: `GET /mailbox/<id>` and `GET /healthz`, plus read-only
//! static files when a [`StaticSite`] is given (off by default).

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::{Mailboxes, StaticSite, progress, text};

/// What [`serve_with`] serves besides the relay. The default is the plain relay.
#[derive(Clone, Default)]
pub struct ServeOptions {
    /// Terminate TLS here (see [`crate::tls_server_config`]).
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Serve these files on every path that is not `/healthz` or `/mailbox/...`.
    pub static_site: Option<Arc<StaticSite>>,
}

async fn route(
    mailboxes: Mailboxes,
    static_site: Option<Arc<StaticSite>>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    Ok(match (req.method(), path.as_str()) {
        (&Method::GET, "/healthz") => text(StatusCode::OK, "ok"),
        (&Method::GET, p) if p.starts_with("/mailbox/") => {
            mailboxes
                .handle_from(&p["/mailbox/".len()..], req, Some(peer.ip()))
                .await
        }
        (_, p) if p == "/healthz" || p.starts_with("/mailbox/") => {
            text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        }
        (m, p) => match &static_site {
            Some(site) => site.respond(m, p).await,
            None => text(StatusCode::NOT_FOUND, "not found"),
        },
    })
}

/// Serve until `shutdown` resolves, then stop accepting, close every mailbox with
/// 1001 and wait up to 5 s for the connections to drain. Plain HTTP; see
/// [`serve_tls`] to terminate TLS here.
pub async fn serve(
    listener: TcpListener,
    mailboxes: Mailboxes,
    shutdown: impl Future<Output = ()>,
) {
    serve_tls(listener, mailboxes, None, shutdown).await
}

/// [`serve`] with optional TLS termination: with `Some(config)` (see
/// [`crate::tls_server_config`]) every accepted connection first completes a TLS
/// handshake, inside the same connection cap and under the same stall and
/// handshake time limits as the HTTP that follows; a failed handshake just drops
/// the connection.
pub async fn serve_tls(
    listener: TcpListener,
    mailboxes: Mailboxes,
    tls: Option<Arc<rustls::ServerConfig>>,
    shutdown: impl Future<Output = ()>,
) {
    let opts = ServeOptions {
        tls,
        ..Default::default()
    };
    serve_with(listener, mailboxes, opts, shutdown).await
}

/// [`serve_tls`] plus, with [`ServeOptions::static_site`], read-only static files
/// on the same port (the relay's own paths win).
pub async fn serve_with(
    listener: TcpListener,
    mailboxes: Mailboxes,
    opts: ServeOptions,
    shutdown: impl Future<Output = ()>,
) {
    let acceptor = opts.tls.map(tokio_rustls::TlsAcceptor::from);
    let static_site = opts.static_site;
    let stop = mailboxes.inner.stop.subscribe();
    let mut tasks = tokio::task::JoinSet::new();
    let static_timeout = mailboxes.inner.cfg.static_timeout;
    let slots = Arc::new(tokio::sync::Semaphore::new(
        mailboxes.inner.cfg.max_connections,
    ));
    tokio::pin!(shutdown);
    loop {
        // Wait for a connection slot before accepting (backpressure via the kernel backlog).
        let slot = tokio::select! {
            _ = &mut shutdown => break,
            p = slots.clone().acquire_owned() => p.expect("semaphore never closed"),
        };
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => break,
            acc = listener.accept() => match acc {
                Ok(v) => v,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        while tasks.try_join_next().is_some() {}
        let mailboxes = mailboxes.clone();
        let static_site = static_site.clone();
        let acceptor = acceptor.clone();
        let mut stop = stop.clone();
        tasks.spawn(async move {
            let _slot = slot;
            let progress = progress::Progress::new();
            // Progress is measured on the raw socket, below TLS, so the handshake
            // counts too.
            let io = progress::Tracked::new(stream, progress.clone());
            match acceptor {
                None => {
                    drive(
                        io,
                        mailboxes,
                        static_site,
                        peer,
                        progress,
                        static_timeout,
                        stop,
                    )
                    .await
                }
                Some(acceptor) => {
                    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(io));
                    let tls = tokio::select! {
                        r = handshake => match r {
                            Ok(Ok(tls)) => tls,
                            _ => return,
                        },
                        _ = progress.stalled(static_timeout) => return,
                        _ = stopped(&mut stop) => return,
                    };
                    drive(
                        tls,
                        mailboxes,
                        static_site,
                        peer,
                        progress,
                        static_timeout,
                        stop,
                    )
                    .await
                }
            }
        });
    }
    mailboxes.shutdown();
    let drain = async {
        while tasks.join_next().await.is_some() {}
        while mailboxes.open_connections() > 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        tracing::warn!("shutdown drain timed out");
    }
}

async fn stopped(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

/// Upper bound for a TLS handshake (the stall timeout alone would let a client
/// that sends a byte at a time hold a slot).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// One HTTP connection (plain or already TLS) until it ends, stalls or the relay stops.
async fn drive<I>(
    io: I,
    mailboxes: Mailboxes,
    static_site: Option<Arc<StaticSite>>,
    peer: SocketAddr,
    progress: progress::Progress,
    static_timeout: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = service_fn(move |req| route(mailboxes.clone(), static_site.clone(), req, peer));
    // header_read_timeout also closes keep-alive connections idle between requests.
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .serve_connection(TokioIo::new(io), svc)
        .with_upgrades();
    tokio::pin!(conn);
    tokio::select! {
        _ = &mut conn => {}
        // No I/O progress for static_timeout cuts the connection (an upgraded
        // mailbox has left `conn` by then and has its own idle timeout).
        _ = progress.stalled(static_timeout) => {}
        _ = stopped(&mut stop) => {
            conn.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn).await;
        }
    }
}
