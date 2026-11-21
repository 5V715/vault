// SPDX-License-Identifier: MIT
//! Pairing registry and the per-participant frame pipe.

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};

use crate::{MailboxConfig, Permit};

/// Frames buffered per direction; a slow peer backpressures the sender.
const CHANNEL_FRAMES: usize = 16;
/// Frames (and bytes) a first connection may send before its peer arrives
/// (a browser initiator may send Noise message 1 right after connecting).
const PREPAIR_MAX_FRAMES: usize = 16;
const PREPAIR_MAX_BYTES: usize = 64 * 1024;
const CLOSE_SEND_TIMEOUT: Duration = Duration::from_secs(2);
const CODE_UNPAIRED_EXPIRED: u16 = 4408;

/// Lowercase 32 hex characters.
pub fn parse_id(s: &str) -> Option<&str> {
    (s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(s)
}

fn short(id: &str) -> String {
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    format!("{:08x}", h.finish() >> 32)
}

enum Item {
    Frame(Bytes),
    Close(CloseCode, &'static str),
}

pub(crate) struct Link {
    tx: mpsc::Sender<Item>,
    rx: mpsc::Receiver<Item>,
}

enum Entry {
    Waiting(oneshot::Sender<Link>),
    /// Both sides connected; removed when the last one has ended.
    Paired {
        live: u8,
    },
}

pub(crate) enum Refuse {
    /// Two participants already.
    Full,
    TooManyWaiting,
}

pub(crate) enum Join {
    First(oneshot::Receiver<Link>),
    Second(Link),
}

#[derive(Default)]
pub(crate) struct Registry {
    map: Mutex<HashMap<String, Entry>>,
}

/// Releases this connection's claim on the mailbox id when dropped.
pub(crate) struct Slot {
    reg: Arc<Registry>,
    id: String,
}

impl Registry {
    pub(crate) fn join(
        self: &Arc<Self>,
        id: &str,
        max_waiting: usize,
    ) -> Result<(Join, Slot), Refuse> {
        let mut g = self.map.lock().unwrap();
        let join = match g.remove(id) {
            None => {
                let waiting = g
                    .values()
                    .filter(|e| matches!(e, Entry::Waiting(_)))
                    .count();
                if waiting >= max_waiting {
                    return Err(Refuse::TooManyWaiting);
                }
                let (tx, rx) = oneshot::channel();
                g.insert(id.to_string(), Entry::Waiting(tx));
                Join::First(rx)
            }
            Some(Entry::Waiting(first)) => {
                let (tx_ab, rx_ab) = mpsc::channel(CHANNEL_FRAMES);
                let (tx_ba, rx_ba) = mpsc::channel(CHANNEL_FRAMES);
                let _ = first.send(Link {
                    tx: tx_ab,
                    rx: rx_ba,
                });
                g.insert(id.to_string(), Entry::Paired { live: 2 });
                Join::Second(Link {
                    tx: tx_ba,
                    rx: rx_ab,
                })
            }
            Some(paired @ Entry::Paired { .. }) => {
                g.insert(id.to_string(), paired);
                return Err(Refuse::Full);
            }
        };
        drop(g);
        Ok((
            join,
            Slot {
                reg: self.clone(),
                id: id.to_string(),
            },
        ))
    }

    fn leave(&self, id: &str) {
        let mut g = self.map.lock().unwrap();
        match g.get_mut(id) {
            Some(Entry::Paired { live }) if *live > 1 => *live -= 1,
            Some(_) => {
                g.remove(id);
            }
            None => {}
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.reg.leave(&self.id);
    }
}

enum Fin {
    /// Close our client and tell the peer.
    Local(CloseCode, &'static str),
    /// The peer already ended with this code; mirror it to our client.
    Peer(CloseCode, &'static str),
}

fn client_close(code: Option<CloseCode>) -> Fin {
    match code {
        None | Some(CloseCode::Normal) => Fin::Local(CloseCode::Normal, ""),
        Some(_) => Fin::Local(CloseCode::Error, "peer error"),
    }
}

enum Waited {
    Paired(Link),
    End(CloseCode, &'static str),
    Gone,
}

/// Run one mailbox participant after the HTTP upgrade. Holds `permit` and `slot`
/// for its lifetime.
pub(crate) async fn run(
    on_upgrade: OnUpgrade,
    join: Join,
    id: String,
    slot: Slot,
    cfg: Arc<MailboxConfig>,
    permit: Permit,
    mut stop: watch::Receiver<bool>,
) {
    let _permit = permit;
    let tag = short(&id);
    let Ok(upgraded) = on_upgrade.await else {
        return;
    };
    let ws_cfg = WebSocketConfig::default()
        .max_frame_size(Some(cfg.mailbox_max_frame_bytes))
        .max_message_size(Some(cfg.mailbox_max_frame_bytes));
    let mut ws =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(ws_cfg)).await;

    let mut prepaired: VecDeque<Bytes> = VecDeque::new();
    let link = match join {
        Join::Second(link) => link,
        Join::First(mut rx) => {
            let deadline = tokio::time::sleep(cfg.mailbox_timeout);
            tokio::pin!(deadline);
            let mut pre_bytes = 0usize;
            let waited = loop {
                tokio::select! {
                    l = &mut rx => break match l {
                        Ok(l) => Waited::Paired(l),
                        Err(_) => Waited::Gone,
                    },
                    _ = &mut deadline => {
                        break Waited::End(CloseCode::from(CODE_UNPAIRED_EXPIRED), "unpaired");
                    }
                    _ = stop.wait_for(|v| *v) => break Waited::End(CloseCode::Away, "shutdown"),
                    m = ws.next() => match m {
                        Some(Ok(Message::Binary(b))) => {
                            pre_bytes += b.len();
                            if prepaired.len() >= PREPAIR_MAX_FRAMES || pre_bytes > PREPAIR_MAX_BYTES {
                                break Waited::End(CloseCode::Policy, "too much before pairing");
                            }
                            prepaired.push_back(b);
                        }
                        Some(Ok(Message::Text(_))) => {
                            break Waited::End(CloseCode::Unsupported, "binary only");
                        }
                        Some(Ok(Message::Close(f))) => {
                            // Echo the client's close code.
                            break Waited::End(f.map_or(CloseCode::Normal, |f| f.code), "");
                        }
                        Some(Err(_)) | None => break Waited::Gone,
                        Some(Ok(_)) => {} // ping/pong: allowed while waiting
                    },
                }
            };
            match waited {
                Waited::Paired(l) => l,
                Waited::End(code, reason) => {
                    // Release the id first so a joiner cannot pair with a dead waiter.
                    drop(slot);
                    let deadline = tokio::time::Instant::now() + CLOSE_SEND_TIMEOUT;
                    close_ws(&mut ws, code, reason, deadline).await;
                    drain(&mut ws, deadline).await;
                    tracing::debug!(mailbox = %tag, "mailbox ended unpaired");
                    return;
                }
                Waited::Gone => {
                    drop(slot);
                    tracing::debug!(mailbox = %tag, "mailbox waiter left");
                    return;
                }
            }
        }
    };
    tracing::debug!(mailbox = %tag, "mailbox paired side");
    let Link { tx, mut rx } = link;
    let (mut sink, mut stream) = ws.split();

    let start = Instant::now();
    let last = AtomicU64::new(0);
    let now_ms = || start.elapsed().as_millis() as u64;
    let touch = || last.store(now_ms(), Ordering::Relaxed);
    let (bytes_in, bytes_out) = (AtomicU64::new(0), AtomicU64::new(0));

    // Own client -> peer. A full channel stops reading from the client
    // (backpressure) until the peer drains it.
    let up = async {
        for b in prepaired.drain(..) {
            bytes_in.fetch_add(b.len() as u64, Ordering::Relaxed);
            if tx.send(Item::Frame(b)).await.is_err() {
                return std::future::pending().await;
            }
        }
        touch();
        while let Some(msg) = stream.next().await {
            // Only binary payload progress counts as activity.
            match msg {
                Ok(Message::Binary(b)) => {
                    touch();
                    bytes_in.fetch_add(b.len() as u64, Ordering::Relaxed);
                    if tx.send(Item::Frame(b)).await.is_err() {
                        return std::future::pending().await;
                    }
                    touch();
                }
                Ok(Message::Text(_)) => return Fin::Local(CloseCode::Unsupported, "binary only"),
                Ok(Message::Close(f)) => return client_close(f.map(|f| f.code)),
                Err(_) => return Fin::Local(CloseCode::Error, "peer error"),
                Ok(_) => {}
            }
        }
        Fin::Local(CloseCode::Normal, "")
    };
    // Peer -> own client.
    let down = async {
        loop {
            match rx.recv().await {
                Some(Item::Frame(b)) => {
                    touch();
                    bytes_out.fetch_add(b.len() as u64, Ordering::Relaxed);
                    if sink.send(Message::Binary(b)).await.is_err() {
                        return Fin::Local(CloseCode::Error, "peer error");
                    }
                    touch();
                }
                Some(Item::Close(code, reason)) => return Fin::Peer(code, reason),
                None => return Fin::Peer(CloseCode::Error, "peer error"),
            }
        }
    };
    let idle = async {
        loop {
            let idle_for =
                Duration::from_millis(now_ms().saturating_sub(last.load(Ordering::Relaxed)));
            if idle_for >= cfg.idle_timeout {
                return Fin::Local(CloseCode::Away, "idle");
            }
            tokio::time::sleep(cfg.idle_timeout - idle_for).await;
        }
    };
    let shutdown = async {
        let _ = stop.wait_for(|v| *v).await;
        Fin::Local(CloseCode::Away, "shutdown")
    };
    let fin = tokio::select! {
        r = up => r,
        r = down => r,
        r = idle => r,
        r = shutdown => r,
    };
    let (code, reason) = match fin {
        Fin::Local(code, reason) => {
            // Queue the close behind in-flight frames; a stuck peer is dropped
            // after the timeout (its recv then ends with an error close).
            let _ =
                tokio::time::timeout(CLOSE_SEND_TIMEOUT, tx.send(Item::Close(code, reason))).await;
            (code, reason)
        }
        Fin::Peer(code, reason) => (code, reason),
    };
    drop(tx);
    // One cap for the whole shutdown: close frame, then drain the client.
    let deadline = tokio::time::Instant::now() + CLOSE_SEND_TIMEOUT;
    close_ws(&mut sink, code, reason, deadline).await;
    drain(&mut stream, deadline).await;
    tracing::debug!(
        mailbox = %tag,
        bytes_in = bytes_in.load(Ordering::Relaxed),
        bytes_out = bytes_out.load(Ordering::Relaxed),
        "mailbox side closed"
    );
}

async fn close_ws<S>(
    ws: &mut S,
    code: CloseCode,
    reason: &'static str,
    deadline: tokio::time::Instant,
) where
    S: futures_util::Sink<Message> + Unpin,
{
    let frame = Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }));
    let _ = tokio::time::timeout_at(deadline, ws.send(frame)).await;
}

/// Read and discard what the client still sends until its close, EOF or the
/// deadline. Dropping a socket with unread data makes Windows send an RST, which
/// can discard frames (the final notice) still in flight to the client.
async fn drain<S>(ws: &mut S, deadline: tokio::time::Instant)
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let _ = tokio::time::timeout_at(deadline, async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;
}
