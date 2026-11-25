// SPDX-License-Identifier: MIT
//! Browser stand-in for the Android app's instrumented pairing tests (the CI
//! emulator jobs): a relay (`vault_mailbox`, plain HTTP) and the browser side of
//! a pairing (`VaultSession` with the native drivers), driven over a small
//! plain-text HTTP control API. Both listen on 127.0.0.1 only; the emulator
//! reaches them through `adb reverse`, so the relay URL in the pairing code is
//! a loopback one (the only kind plain `ws://` is allowed for).
//!
//! Test tooling only: it holds no secrets worth protecting and is never shipped.
//!
//! ```text
//! cargo run -p vault-client --example browser_stand_in -- 47100 47101
//! GET /pair?app=todo&origin=https%3A%2F%2Ftodo.example  -> the QR text
//! GET /sas                 -> the 6-digit code (waits for it)
//! GET /confirm             -> "Codes match" in the browser
//! GET /state?until=active  -> waits for a state: pairing, awaiting, active, closed
//! GET /put?k=..&v=..       -> writes and commits through the vault
//! GET /get?k=..            -> reads through the vault (404 when absent)
//! GET /cancel, GET /lock   -> ends the pairing / the session
//! ```

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vault_client::native::{TokioClock, TokioFactory};
use vault_client::{SessionCallbacks, SessionState, SystemRng, VaultSession};

/// What the browser side reported so far.
#[derive(Clone, Default)]
struct Seen {
    qr: Rc<RefCell<Option<String>>>,
    sas: Rc<RefCell<Option<String>>>,
    state: Rc<RefCell<Option<SessionState>>>,
}

struct Pairing {
    session: Rc<VaultSession>,
    seen: Seen,
}

type Current = Rc<RefCell<Option<Pairing>>>;

fn describe(s: &Option<SessionState>) -> String {
    match s {
        None => "none".into(),
        Some(SessionState::Pairing) => "pairing".into(),
        Some(SessionState::AwaitingApproval) => "awaiting".into(),
        Some(SessionState::Active) => "active".into(),
        Some(SessionState::Closed(r)) => format!("closed:{r}"),
    }
}

/// Decodes `%XX` and `+` in a query value.
fn unescape(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then(|| unescape(v))
    })
}

/// Polls `f` every 50 ms until it returns `Some` or `ms` elapsed.
async fn until<T>(ms: u64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn current(cur: &Current) -> Option<(Rc<VaultSession>, Seen)> {
    cur.borrow()
        .as_ref()
        .map(|p| (p.session.clone(), p.seen.clone()))
}

/// One control request: `(status, body)`.
async fn route(path: &str, query: &str, cur: &Current, relay: &str) -> (u16, String) {
    let timeout = param(query, "timeout_ms")
        .and_then(|t| t.parse().ok())
        .unwrap_or(30_000u64);
    if path == "/healthz" {
        return (200, "ok".into());
    }
    if path == "/pair" {
        // A new pairing replaces the previous one.
        let old = cur.borrow_mut().take();
        if let Some(old) = old {
            old.session.lock().await;
        }
        let app = param(query, "app").unwrap_or_default();
        let origin = param(query, "origin").unwrap_or_default();
        let device = param(query, "device").unwrap_or_else(|| "Stand-in Browser".into());
        let seen = Seen::default();
        let cb = {
            let (qr, sas, state) = (seen.qr.clone(), seen.sas.clone(), seen.state.clone());
            SessionCallbacks::new(
                Rc::new(move |q: &str| *qr.borrow_mut() = Some(q.to_string())),
                Rc::new(move |s: &str| *sas.borrow_mut() = Some(s.to_string())),
                Rc::new(move |s| *state.borrow_mut() = Some(s)),
            )
        };
        let (session, fut) = match VaultSession::pair(
            TokioFactory,
            TokioClock,
            &SystemRng,
            relay,
            &app,
            &origin,
            &device,
            cb,
        ) {
            Ok(x) => x,
            Err(e) => return (400, format!("pair: {e}")),
        };
        tokio::task::spawn_local(fut);
        *cur.borrow_mut() = Some(Pairing {
            session: Rc::new(session),
            seen: seen.clone(),
        });
        let qr = until(10_000, || seen.qr.borrow().clone()).await;
        return match qr {
            Some(q) => (200, q),
            None => (504, "no offer".into()),
        };
    }
    let Some((session, seen)) = current(cur) else {
        return (409, "no pairing".into());
    };
    match path {
        "/sas" => match until(timeout, || seen.sas.borrow().clone()).await {
            Some(s) => (200, s),
            None => (504, describe(&seen.state.borrow())),
        },
        "/confirm" => match session.confirm_sas() {
            Ok(()) => (200, "ok".into()),
            Err(e) => (409, format!("confirm: {e}")),
        },
        "/cancel" => match session.cancel_pairing() {
            Ok(()) => (200, "ok".into()),
            Err(e) => (409, format!("cancel: {e}")),
        },
        "/state" => {
            let want = param(query, "until").unwrap_or_default();
            let reached = until(timeout, || {
                let d = describe(&seen.state.borrow());
                (want.is_empty() || d == want || d.starts_with(&format!("{want}:"))).then_some(d)
            })
            .await;
            match reached {
                Some(d) => (200, d),
                None => (504, describe(&seen.state.borrow())),
            }
        }
        "/put" => {
            let (Some(k), Some(v)) = (param(query, "k"), param(query, "v")) else {
                return (400, "k and v".into());
            };
            let Some(b) = session.backend() else {
                return (409, describe(&seen.state.borrow()));
            };
            if let Err(e) = b.put(k.as_bytes(), v.as_bytes()).await {
                return (500, format!("put: {e}"));
            }
            match b.flush_commit().await {
                Ok(()) => (200, "ok".into()),
                Err(e) => (500, format!("commit: {e}")),
            }
        }
        "/get" => {
            let Some(k) = param(query, "k") else {
                return (400, "k".into());
            };
            let Some(b) = session.backend() else {
                return (409, describe(&seen.state.borrow()));
            };
            match b.get(k.as_bytes()).await {
                Ok(Some(v)) => (200, String::from_utf8_lossy(&v).into_owned()),
                Ok(None) => (404, "absent".into()),
                Err(e) => (500, format!("get: {e}")),
            }
        }
        "/lock" => {
            session.lock().await;
            (200, describe(&seen.state.borrow()))
        }
        _ => (404, "unknown path".into()),
    }
}

async fn serve_one(mut sock: TcpStream, cur: Current, relay: Rc<String>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut chunk).await?;
        if n == 0 || buf.len() > 16 * 1024 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let target = head.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (status, body) = route(path, query, &cur, &relay).await;
    let text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await
}

fn port(arg: Option<String>, what: &str) -> u16 {
    arg.and_then(|p| p.parse().ok()).unwrap_or_else(|| {
        panic!("usage: browser_stand_in <relay port> <control port> ({what} missing)")
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let relay_port = port(args.next(), "relay port");
    let control_port = port(args.next(), "control port");

    let relay_l = TcpListener::bind(("127.0.0.1", relay_port))
        .await
        .expect("bind the relay port");
    // Every connection arrives from 127.0.0.1 (adb reverse): no per-IP cap.
    let mailboxes = vault_mailbox::Mailboxes::new(vault_mailbox::MailboxConfig {
        max_per_ip: 1024,
        ..Default::default()
    });
    tokio::spawn(vault_mailbox::serve(
        relay_l,
        mailboxes,
        std::future::pending::<()>(),
    ));

    let control = TcpListener::bind(("127.0.0.1", control_port))
        .await
        .expect("bind the control port");
    let relay = Rc::new(format!("http://127.0.0.1:{relay_port}"));
    eprintln!("browser stand-in: relay {relay}, control http://127.0.0.1:{control_port}");
    let cur: Current = Rc::default();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            loop {
                let Ok((sock, _)) = control.accept().await else {
                    continue;
                };
                let (cur, relay) = (cur.clone(), relay.clone());
                tokio::task::spawn_local(async move {
                    if let Err(e) = serve_one(sock, cur, relay).await {
                        eprintln!("browser stand-in: control connection: {e}");
                    }
                });
            }
        })
        .await;
}
