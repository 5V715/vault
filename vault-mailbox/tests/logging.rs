// SPDX-License-Identifier: MIT
//! The relay must never log payloads or a full mailbox id.
//!
//! This is its own test binary with a *global* subscriber on purpose. A thread-local
//! `set_default` subscriber inside `mailbox.rs` raced the other tests there: tracing
//! caches each callsite's interest process-wide, so a callsite first hit by a parallel
//! test with no subscriber could stay disabled and leave this capture empty (seen on
//! macOS CI). Here exactly one test runs and the subscriber is installed before any
//! event, so the capture is reliable on every OS.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use vault_mailbox::{MailboxConfig, Mailboxes};

const ID: &str = "0123456789abcdef0123456789abcdef";
const SECRET: &[u8] = b"SECRET-PAYLOAD-0xdeadbeef";

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

async fn next_bin<S>(ws: &mut S) -> Vec<u8>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
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

#[tokio::test]
async fn no_payload_logging() {
    let buf = Buf::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(buf.clone())
            .finish(),
    )
    .expect("no other subscriber in this test binary");

    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (stop, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(vault_mailbox::serve(
        l,
        Mailboxes::new(MailboxConfig::default()),
        async move {
            let _ = rx.await;
        },
    ));
    let url = format!("ws://{addr}/mailbox/{ID}");
    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    a.send(Message::Binary(SECRET.to_vec().into()))
        .await
        .unwrap();
    assert_eq!(next_bin(&mut b).await, SECRET);
    a.close(None).await.unwrap();
    // Let b observe the close and the relay finish logging.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while b.next().await.is_some() {}
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = stop.send(());
    let _ = server.await;

    let logs = String::from_utf8_lossy(&buf.0.lock().unwrap()).to_string();
    assert!(logs.contains("mailbox"), "capture works: {logs}");
    assert!(!logs.contains("SECRET-PAYLOAD"));
    assert!(!logs.contains(&format!("{SECRET:?}")));
    assert!(!logs.contains(ID), "full mailbox id must not be logged");
}
