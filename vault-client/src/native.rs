// SPDX-License-Identifier: MIT
//! Native drivers (feature `native`): a tokio-tungstenite mailbox client and a
//! tokio [`Clock`]. They need a tokio runtime to be entered; the session
//! itself runs on any single-threaded executor. Plain `ws://` only (no TLS):
//! meant for tests and local tools.

use crate::error::ClientError;
use crate::transport::{Clock, MailboxFactory, MailboxTransport};
use async_trait::async_trait;
use futures::channel::mpsc;
use futures::future::{Either, LocalBoxFuture};
use futures::lock::Mutex;
use futures::{FutureExt, StreamExt};
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, SplitStream};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn net(msg: String) -> ClientError {
    ClientError::Transport(msg)
}

/// Real-time [`Clock`] (tokio timers; the runtime must be entered).
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn sleep(&self, ms: u64) -> LocalBoxFuture<'static, ()> {
        // Lazy: the timer starts when first polled (like an `async fn`).
        async move { tokio::time::sleep(Duration::from_millis(ms)).await }.boxed_local()
    }
}

/// Mailbox socket over tokio-tungstenite.
pub struct TokioMailbox {
    sink: Mutex<Option<SplitSink<Ws, Message>>>,
    stream: Mutex<Option<SplitStream<Ws>>>,
    shutdown_tx: mpsc::UnboundedSender<()>,
    shutdown_rx: Mutex<mpsc::UnboundedReceiver<()>>,
}

impl TokioMailbox {
    pub async fn open(url: &str) -> Result<Self, ClientError> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| net(format!("mailbox: {e}")))?;
        let (sink, stream) = ws.split();
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded();
        Ok(Self {
            sink: Mutex::new(Some(sink)),
            stream: Mutex::new(Some(stream)),
            shutdown_tx,
            shutdown_rx: Mutex::new(shutdown_rx),
        })
    }
}

#[async_trait(?Send)]
impl MailboxTransport for TokioMailbox {
    async fn send(&self, frame: &[u8]) -> Result<(), ClientError> {
        let mut g = self.sink.lock().await;
        let sink = g.as_mut().ok_or_else(|| net("mailbox closed".into()))?;
        sink.send(Message::Binary(frame.to_vec().into()))
            .await
            .map_err(|e| net(format!("mailbox send: {e}")))
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, ClientError> {
        let mut g = self.stream.lock().await;
        let Some(stream) = g.as_mut() else {
            return Ok(None);
        };
        let mut shutdown = self.shutdown_rx.lock().await;
        loop {
            let next = stream.next();
            let stop = shutdown.next();
            futures::pin_mut!(next, stop);
            match futures::future::select(next, stop).await {
                Either::Left((Some(Ok(Message::Binary(b))), _)) => {
                    return Ok(Some(b.to_vec()));
                }
                Either::Left((Some(Ok(Message::Close(_))) | None, _))
                | Either::Left((Some(Err(_)), _)) => return Ok(None),
                Either::Left((Some(Ok(_)), _)) => {}
                Either::Right(_) => {
                    // Dropping the halves closes the TCP connection.
                    *g = None;
                    if let Some(mut s) = self.sink.try_lock() {
                        *s = None;
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn close(&self) {
        let _ = self.shutdown_tx.unbounded_send(());
    }
}

/// Opens [`TokioMailbox`]es.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioFactory;

#[async_trait(?Send)]
impl MailboxFactory for TokioFactory {
    type Mailbox = TokioMailbox;
    async fn open_mailbox(&self, url: &str) -> Result<TokioMailbox, ClientError> {
        TokioMailbox::open(url).await
    }
}
