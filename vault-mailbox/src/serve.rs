// SPDX-License-Identifier: MIT
//! Minimal accept loop: `GET /mailbox/<id>` and `GET /healthz`, nothing else.

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

use crate::{Mailboxes, progress, text};

async fn route(
    mailboxes: Mailboxes,
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
        _ => text(StatusCode::NOT_FOUND, "not found"),
    })
}

/// Serve until `shutdown` resolves, then stop accepting, close every mailbox with
/// 1001 and wait up to 5 s for the connections to drain.
pub async fn serve(
    listener: TcpListener,
    mailboxes: Mailboxes,
    shutdown: impl Future<Output = ()>,
) {
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
        let mut stop = stop.clone();
        tasks.spawn(async move {
            let _slot = slot;
            let svc = service_fn(move |req| route(mailboxes.clone(), req, peer));
            let progress = progress::Progress::new();
            let io = progress::Tracked::new(stream, progress.clone());
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
