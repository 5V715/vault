// SPDX-License-Identifier: MIT

//! Request/response channel over an established Noise session.

use crate::error::{ClientError, CloseReason, perr};
use crate::transport::MailboxTransport;
use futures::channel::oneshot;
use futures::lock::Mutex;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use vault_proto::msg::VaultMessage;
use vault_proto::noise::{PairKeys, Session};
use vault_proto::{Chunk, Chunker, Notice, Reassembler, Request, RequestBody, ResponseBody};
use zeroize::{Zeroize, Zeroizing};

/// Run the Noise XX initiator over `t`: send message 1, read message 2, send
/// message 3. `recv_timeout` bounds the wait for the vault (it is only
/// scanning the QR code, so this is generous).
pub(crate) async fn handshake(
    t: &dyn MailboxTransport,
    keys: PairKeys,
    recv_timeout: impl std::future::Future<Output = ()>,
) -> Result<Session, ClientError> {
    let (init, m1) = keys
        .into_initiator()
        .map_err(|_| perr("handshake setup failed"))?;
    t.send(&m1).await?;
    let m2 = {
        let recv = t.recv();
        futures::pin_mut!(recv, recv_timeout);
        match futures::future::select(recv, recv_timeout).await {
            futures::future::Either::Left((r, _)) => r?,
            futures::future::Either::Right(_) => {
                return Err(ClientError::Transport("pairing timed out".into()));
            }
        }
    }
    .ok_or_else(|| ClientError::Transport("pairing offer expired or was closed".into()))?;
    let (m3, session) = init
        .handle_msg2(&m2)
        .map_err(|_| perr("handshake failed"))?;
    t.send(&m3).await?;
    Ok(session)
}

/// An established, not necessarily approved, vault channel.
pub(crate) struct Rpc {
    transport: Box<dyn MailboxTransport>,
    noise: RefCell<Session>,
    /// Held while one message's frames are sent, so chunks of two messages
    /// never interleave (the protocol allows one message in flight).
    chunker: Mutex<Chunker>,
    reasm: RefCell<Reassembler>,
    next_id: Cell<u64>,
    pending: RefCell<HashMap<u64, oneshot::Sender<ResponseBody>>>,
    approved: Cell<bool>,
    closed: RefCell<Option<CloseReason>>,
}

impl Rpc {
    pub(crate) fn new(transport: Box<dyn MailboxTransport>, noise: Session) -> Self {
        Self {
            transport,
            noise: RefCell::new(noise),
            chunker: Mutex::new(Chunker::new()),
            reasm: RefCell::new(Reassembler::new()),
            next_id: Cell::new(1),
            pending: RefCell::default(),
            approved: Cell::new(false),
            closed: RefCell::new(None),
        }
    }

    pub(crate) fn sas(&self) -> String {
        self.noise.borrow().sas().to_string()
    }

    pub(crate) fn set_approved(&self) {
        self.approved.set(true);
    }

    pub(crate) fn close_reason(&self) -> Option<CloseReason> {
        self.closed.borrow().clone()
    }

    /// Mark the channel closed, fail every waiting call and close the socket.
    pub(crate) fn fail(&self, reason: CloseReason) {
        self.closed.borrow_mut().get_or_insert(reason);
        // Dropping the senders wakes the waiters with an error.
        self.pending.borrow_mut().clear();
        self.transport.close();
    }

    /// Send `bodies` back to back (one message each, in order) and collect the
    /// answers. Only `OpenSession` and `Ping` may go out before approval.
    pub(crate) async fn call_many(
        &self,
        bodies: Vec<RequestBody>,
    ) -> Result<Vec<ResponseBody>, ClientError> {
        if let Some(r) = self.close_reason() {
            return Err(ClientError::Closed(r));
        }
        if !self.approved.get()
            && bodies
                .iter()
                .any(|b| !matches!(b, RequestBody::OpenSession { .. } | RequestBody::Ping))
        {
            return Err(perr("vault session not approved yet"));
        }
        let mut waiting = Vec::with_capacity(bodies.len());
        {
            let mut chunker = self.chunker.lock().await;
            for body in bodies {
                if let Some(r) = self.close_reason() {
                    return Err(ClientError::Closed(r));
                }
                let id = self.next_id.get();
                self.next_id.set(id + 1);
                let (tx, rx) = oneshot::channel();
                self.pending.borrow_mut().insert(id, tx);
                waiting.push(rx);
                let msg = Zeroizing::new(Request { id, body }.encode());
                let chunks = chunker.split(&msg).map_err(|_| perr("request too large"))?;
                for mut c in chunks {
                    let plain = Zeroizing::new(c.encode());
                    c.payload.zeroize();
                    let frame = self
                        .noise
                        .borrow_mut()
                        .encrypt(&plain)
                        .map_err(|_| perr("encrypt failed"))?;
                    if let Err(e) = self.transport.send(&frame).await {
                        self.fail(CloseReason::SendFailed);
                        return Err(e);
                    }
                }
            }
        }
        let mut out = Vec::with_capacity(waiting.len());
        for rx in waiting {
            match rx.await {
                Ok(ResponseBody::Error { code, msg }) => {
                    return Err(ClientError::Vault { code, msg });
                }
                Ok(b) => out.push(b),
                Err(_) => {
                    // Only `fail` drops the senders, and it records a reason first.
                    let r = self.close_reason().unwrap_or(CloseReason::Closed);
                    return Err(ClientError::Closed(r));
                }
            }
        }
        Ok(out)
    }

    pub(crate) async fn call(&self, body: RequestBody) -> Result<ResponseBody, ClientError> {
        let mut v = self.call_many(vec![body]).await?;
        v.pop().ok_or_else(|| perr("no response"))
    }

    /// Read the vault's messages until the session ends; returns why.
    pub(crate) async fn drive(&self) -> CloseReason {
        let reason = loop {
            match self.transport.recv().await {
                Ok(Some(frame)) => match self.on_frame(&frame) {
                    Ok(None) => {
                        // Let the waiter that just got its answer run before we
                        // look at the next frame (an answer followed by a close
                        // must be seen as the answer).
                        yield_once().await;
                    }
                    Ok(Some(reason)) | Err(reason) => break reason,
                },
                Ok(None) | Err(_) => break CloseReason::Disconnected,
            }
        };
        self.fail(reason.clone());
        self.close_reason().unwrap_or(reason)
    }

    /// `Ok(Some(reason))`: the vault ended the session; `Err(reason)`: protocol error.
    fn on_frame(&self, frame: &[u8]) -> Result<Option<CloseReason>, CloseReason> {
        let plain = Zeroizing::new(
            self.noise
                .borrow_mut()
                .decrypt(frame)
                .map_err(|_| CloseReason::ProtocolError)?,
        );
        let chunk = Chunk::decode(&plain).map_err(|_| CloseReason::ProtocolError)?;
        let Some(msg) = self
            .reasm
            .borrow_mut()
            .push(chunk)
            .map_err(|_| CloseReason::ProtocolError)?
            .map(Zeroizing::new)
        else {
            return Ok(None);
        };
        match VaultMessage::decode(&msg).map_err(|_| CloseReason::ProtocolError)? {
            VaultMessage::Response(r) => {
                if let Some(tx) = self.pending.borrow_mut().remove(&r.id) {
                    let _ = tx.send(r.body);
                }
                Ok(None)
            }
            VaultMessage::Notice(Notice::SessionTerminated { reason }) => {
                Ok(Some(CloseReason::Terminated(reason)))
            }
            VaultMessage::Notice(Notice::Locked) => Ok(Some(CloseReason::Locked)),
        }
    }

    /// Best effort goodbye: send `Close` without waiting for the answer
    /// (nobody is reading any more), then drop the socket.
    pub(crate) async fn close(&self) {
        if self.close_reason().is_none() && self.approved.get() {
            let mut chunker = self.chunker.lock().await;
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            let msg = Request {
                id,
                body: RequestBody::Close,
            }
            .encode();
            if let Ok(chunks) = chunker.split(&msg) {
                for c in chunks {
                    let frame = self.noise.borrow_mut().encrypt(&c.encode());
                    match frame {
                        Ok(f) if self.transport.send(&f).await.is_ok() => {}
                        _ => break,
                    }
                }
            }
        }
        self.fail(CloseReason::Closed);
    }
}

/// Return `Pending` once (waking immediately), giving other tasks a turn.
async fn yield_once() {
    let mut yielded = false;
    futures::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

/// Send `OpenSession` and wait for the user's decision on the vault. `Ok` means
/// the vault answered `Approved`; the caller opens the channel for store
/// requests ([`Rpc::set_approved`]) only once the user also confirmed the
/// code in the browser.
pub(crate) async fn open_session(rpc: &Rpc, device_name: &str) -> Result<(), ClientError> {
    match rpc
        .call(RequestBody::OpenSession {
            device_name: device_name.to_string(),
        })
        .await?
    {
        ResponseBody::Approved => Ok(()),
        ResponseBody::Denied => Err(ClientError::Denied),
        _ => Err(perr("unexpected answer to OpenSession")),
    }
}
