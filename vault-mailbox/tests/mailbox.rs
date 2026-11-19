// SPDX-License-Identifier: MIT
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::{self, Message};
use vault_mailbox::{MailboxConfig, Mailboxes};

const ID: &str = "0123456789abcdef0123456789abcdef";

struct Relay {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

async fn start(cfg: MailboxConfig) -> Relay {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(vault_mailbox::serve(l, Mailboxes::new(cfg), async move {
        let _ = rx.await;
    }));
    Relay {
        addr,
        stop: Some(tx),
        handle: Some(handle),
    }
}

fn cfg() -> MailboxConfig {
    MailboxConfig::default()
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn join(r: &Relay, id: &str) -> Result<Ws, tungstenite::Error> {
    tokio_tungstenite::connect_async(format!("ws://{}/mailbox/{id}", r.addr))
        .await
        .map(|(ws, _)| ws)
}

fn http_status(e: tungstenite::Error) -> u16 {
    match e {
        tungstenite::Error::Http(resp) => resp.status().as_u16(),
        other => panic!("expected http error, got {other:?}"),
    }
}

fn bin(b: &[u8]) -> Message {
    Message::Binary(b.to_vec().into())
}

async fn next_bin(ws: &mut Ws) -> Vec<u8> {
    match tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    {
        Message::Binary(b) => b.to_vec(),
        other => panic!("unexpected {other:?}"),
    }
}

async fn close_code(ws: &mut Ws, within: Duration) -> Option<u16> {
    tokio::time::timeout(within, async {
        while let Some(m) = ws.next().await {
            match m {
                Ok(Message::Close(Some(f))) => return Some(u16::from(f.code)),
                Ok(Message::Close(None)) | Err(_) => return None,
                _ => {}
            }
        }
        None
    })
    .await
    .expect("no close within deadline")
}

#[tokio::test]
async fn pairs_two_and_pipes_both_ways() {
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    a.send(bin(b"from a 1")).await.unwrap();
    a.send(bin(b"from a 2")).await.unwrap();
    b.send(bin(b"from b")).await.unwrap();
    assert_eq!(next_bin(&mut b).await, b"from a 1");
    assert_eq!(next_bin(&mut b).await, b"from a 2");
    assert_eq!(next_bin(&mut a).await, b"from b");
}

#[tokio::test]
async fn first_may_send_before_peer_arrives() {
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    a.send(bin(b"early")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut b = join(&r, ID).await.unwrap();
    assert_eq!(next_bin(&mut b).await, b"early");
}

#[tokio::test]
async fn third_connection_rejected() {
    let r = start(cfg()).await;
    let _a = join(&r, ID).await.unwrap();
    let _b = join(&r, ID).await.unwrap();
    let e = join(&r, ID).await.err().unwrap();
    assert_eq!(http_status(e), 409);
}

#[tokio::test]
async fn bad_id_rejected() {
    let r = start(cfg()).await;
    for id in [
        "short",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcdef0",
        "",
    ] {
        let e = join(&r, id).await.err().unwrap();
        assert_eq!(http_status(e), 400, "id {id:?}");
    }
}

#[tokio::test]
async fn unpaired_expires_after_60s() {
    let r = start(MailboxConfig {
        mailbox_timeout: Duration::from_millis(300),
        ..cfg()
    })
    .await;
    let mut a = join(&r, ID).await.unwrap();
    // Pings while waiting are fine and do not extend the deadline.
    a.send(Message::Ping(vec![1].into())).await.unwrap();
    assert_eq!(close_code(&mut a, Duration::from_secs(3)).await, Some(4408));
    // The id is free again, immediately, and the new pair works.
    let mut a2 = join(&r, ID).await.unwrap();
    let mut b2 = join(&r, ID).await.unwrap();
    a2.send(bin(b"x")).await.unwrap();
    assert_eq!(next_bin(&mut b2).await, b"x");
}

#[tokio::test]
async fn oversize_frame_closes_pair() {
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    a.send(bin(&vec![0u8; 70 * 1024])).await.unwrap();
    assert_eq!(close_code(&mut b, Duration::from_secs(3)).await, Some(1011));
    let mut ok = bin(&[1u8; 65535]);
    let _ = &mut ok;
    let id2 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let mut c = join(&r, id2).await.unwrap();
    let mut d = join(&r, id2).await.unwrap();
    c.send(ok).await.unwrap();
    assert_eq!(next_bin(&mut d).await.len(), 65535);
}

async fn join_with_origin(
    r: &Relay,
    id: &str,
    origin: Option<&str>,
) -> Result<Ws, tungstenite::Error> {
    use tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{}/mailbox/{id}", r.addr)
        .into_client_request()
        .unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(w, _)| w)
}

#[tokio::test]
async fn origin_rules_for_mailbox() {
    let r = start(MailboxConfig {
        allowed_origins: vec!["https://ok.example".into()],
        ..cfg()
    })
    .await;
    // Missing Origin (native vault) is allowed.
    join_with_origin(&r, ID, None).await.unwrap();
    join_with_origin(&r, ID, Some("https://ok.example"))
        .await
        .unwrap();
    let e = join_with_origin(&r, "11111111111111111111111111111111", Some("https://evil"))
        .await
        .err()
        .unwrap();
    assert_eq!(http_status(e), 403);
    // Without an allowlist any Origin is fine.
    let r2 = start(cfg()).await;
    join_with_origin(&r2, ID, Some("https://anything"))
        .await
        .unwrap();
}

#[tokio::test]
async fn waiting_mailboxes_capped() {
    let r = start(MailboxConfig {
        max_mailboxes_waiting: 2,
        ..cfg()
    })
    .await;
    let _a = join(&r, "11111111111111111111111111111111").await.unwrap();
    let _b = join(&r, "22222222222222222222222222222222").await.unwrap();
    let e = join(&r, "33333333333333333333333333333333")
        .await
        .err()
        .unwrap();
    assert_eq!(http_status(e), 429);
    // Joining an existing waiting mailbox is still allowed.
    let _c = join(&r, "11111111111111111111111111111111").await.unwrap();
    let _d = join(&r, "33333333333333333333333333333333").await.unwrap();
}

#[tokio::test]
async fn waiting_client_close_is_echoed() {
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    a.close(None).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(m) = a.next().await {
            if m.is_err() {
                break;
            }
        }
    })
    .await;
    assert!(end.is_ok(), "close handshake did not complete");
}

#[tokio::test]
async fn text_frames_rejected() {
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    a.send(Message::Text("hi".into())).await.unwrap();
    assert_eq!(close_code(&mut a, Duration::from_secs(3)).await, Some(1003));
    assert_eq!(close_code(&mut b, Duration::from_secs(3)).await, Some(1003));
}

#[tokio::test]
async fn close_propagates() {
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    a.send(bin(b"last")).await.unwrap();
    a.close(None).await.unwrap();
    // Frames sent before the close are still delivered.
    assert_eq!(next_bin(&mut b).await, b"last");
    assert_eq!(close_code(&mut b, Duration::from_secs(3)).await, Some(1000));

    let id2 = "ffffffffffffffffffffffffffffffff";
    let mut a = join(&r, id2).await.unwrap();
    let mut b = join(&r, id2).await.unwrap();
    a.close(Some(tungstenite::protocol::CloseFrame {
        code: 4000.into(),
        reason: "x".into(),
    }))
    .await
    .unwrap();
    assert_eq!(close_code(&mut b, Duration::from_secs(3)).await, Some(1011));
    let _ = (a, b);
}

#[tokio::test]
async fn id_is_free_again_after_both_ended() {
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    a.close(None).await.unwrap();
    assert_eq!(close_code(&mut b, Duration::from_secs(3)).await, Some(1000));
    drop((a, b));
    let mut ok = false;
    for _ in 0..50 {
        if join(&r, ID).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "id never released");
}

#[tokio::test]
async fn per_ip_limit() {
    let r = start(MailboxConfig {
        max_per_ip: 2,
        ..cfg()
    })
    .await;
    let _m1 = join(&r, ID).await.unwrap();
    let _m2 = join(&r, "11111111111111111111111111111111").await.unwrap();
    let e = join(&r, "22222222222222222222222222222222")
        .await
        .err()
        .unwrap();
    assert_eq!(http_status(e), 429);
}

#[tokio::test]
async fn total_limit() {
    let r = start(MailboxConfig {
        max_total: 1,
        ..cfg()
    })
    .await;
    let _m = join(&r, ID).await.unwrap();
    let e = join(&r, "11111111111111111111111111111111")
        .await
        .err()
        .unwrap();
    assert_eq!(http_status(e), 429);
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

// Single-threaded runtime so the thread-local subscriber sees relay tasks too.
#[tokio::test]
async fn no_payload_logging() {
    let buf = Buf::default();
    let sub = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(buf.clone())
        .finish();
    let _g = tracing::subscriber::set_default(sub);
    let r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    a.send(bin(b"SECRET-PAYLOAD-0xdeadbeef")).await.unwrap();
    assert_eq!(next_bin(&mut b).await, b"SECRET-PAYLOAD-0xdeadbeef");
    a.close(None).await.unwrap();
    let _ = close_code(&mut b, Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let logs = String::from_utf8_lossy(&buf.0.lock().unwrap()).to_string();
    assert!(logs.contains("mailbox"), "capture works: {logs}");
    assert!(!logs.contains("SECRET-PAYLOAD"));
    assert!(!logs.contains(&format!("{:?}", b"SECRET-PAYLOAD-0xdeadbeef")));
    assert!(!logs.contains(ID), "full mailbox id must not be logged");
}

#[tokio::test]
async fn slow_peer_backpressure_bounded() {
    let r = start(cfg()).await;
    let a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    let (mut tx, _rx) = a.split();
    let sent = Arc::new(AtomicUsize::new(0));
    let s2 = sent.clone();
    let writer = tokio::spawn(async move {
        let mut i = 0u32;
        loop {
            let mut f = vec![0u8; 64 * 1024];
            f[..4].copy_from_slice(&i.to_be_bytes());
            if tx.send(Message::Binary(f.into())).await.is_err() {
                return;
            }
            i += 1;
            s2.store(i as usize, Ordering::SeqCst);
        }
    });
    // b never reads: the writer must stall after a bounded number of frames.
    let mut prev = usize::MAX;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let n = sent.load(Ordering::SeqCst);
        if n == prev {
            break;
        }
        prev = n;
    }
    let stalled_at = sent.load(Ordering::SeqCst);
    assert_eq!(stalled_at, prev, "writer never stalled");
    assert!(
        stalled_at < 1000,
        "unbounded buffering: {stalled_at} frames"
    );
    // Draining b releases the writer and preserves order.
    for want in 0..(stalled_at as u32 + 5) {
        let f = next_bin(&mut b).await;
        assert_eq!(u32::from_be_bytes(f[..4].try_into().unwrap()), want);
    }
    writer.abort();
}

#[tokio::test]
async fn shutdown_closes_mailboxes_1001() {
    let mut r = start(cfg()).await;
    let mut a = join(&r, ID).await.unwrap();
    let mut b = join(&r, ID).await.unwrap();
    let mut w = join(&r, "11111111111111111111111111111111").await.unwrap();
    r.stop.take().unwrap().send(()).unwrap();
    for ws in [&mut a, &mut b, &mut w] {
        assert_eq!(close_code(ws, Duration::from_secs(5)).await, Some(1001));
    }
    tokio::time::timeout(Duration::from_secs(8), r.handle.take().unwrap())
        .await
        .unwrap()
        .unwrap();
}

async fn plain_get(r: &Relay, path: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = TcpStream::connect(r.addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out[9..12].parse().unwrap()
}

#[tokio::test]
async fn serves_only_healthz_and_mailbox() {
    let r = start(cfg()).await;
    assert_eq!(plain_get(&r, "/healthz").await, 200);
    assert_eq!(plain_get(&r, "/").await, 404);
    assert_eq!(plain_get(&r, "/tunnel?host=a&port=1").await, 404);
    // A plain GET to a valid mailbox id is not a websocket upgrade.
    assert_eq!(plain_get(&r, &format!("/mailbox/{ID}")).await, 400);
}
