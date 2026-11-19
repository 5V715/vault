// SPDX-License-Identifier: MIT
//! One session after the Noise handshake: AwaitApproval -> Active -> Closed.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::{SemaphorePermit, mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};
use vault_proto::frame::MAX_MESSAGE;
use vault_proto::msg::{MAX_ENTRIES_TOTAL, MAX_VALUE, VaultMessage};
use vault_proto::{ErrorCode, Notice, ProtoError, Request, RequestBody, Response, ResponseBody};
use zeroize::{Zeroize, Zeroizing};

use crate::transport::{Channel, ChannelError};
use crate::{
    EndReason, Inner, MAX_AWAITING_APPROVAL, SessionRecord, Terminate, VaultContainer, now_ms,
};

/// How long a newly approved session waits for the replaced one to send its notice.
const REPLACE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Clears the active slot (discarding uncommitted writes) and writes the log
/// record. The normal path calls [`Cleanup::release_active`] and
/// [`Cleanup::record`]; if the session task panics, `Drop` does both.
struct Cleanup {
    inner: Arc<Inner>,
    id: u64,
    started_at_ms: u64,
    device_name: String,
    armed: bool,
}

fn release(inner: &Inner, id: u64, c: Option<&mut VaultContainer>) {
    let mut st = inner.state();
    if st.active == Some(id) {
        st.active = None;
        if let Some(c) = c {
            c.discard_pending();
        }
    }
}

impl Cleanup {
    async fn release_active(&self) {
        let mut c = self.inner.container.lock().await;
        release(&self.inner, self.id, c.as_mut());
    }

    fn record(&mut self, reason: EndReason) {
        self.armed = false;
        let mut st = self.inner.state();
        st.sessions.remove(&self.id);
        st.log.push(SessionRecord {
            id: self.id,
            device_name: std::mem::take(&mut self.device_name),
            started_at_ms: self.started_at_ms,
            ended_at_ms: now_ms(),
            reason,
        });
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::warn!(session = self.id, "vault session task failed");
        // Unwinding has released any container guard this task held.
        match self.inner.container.try_lock() {
            Ok(mut c) => release(&self.inner, self.id, c.as_mut()),
            Err(_) => {
                // Stay active until the container can be taken, so nobody else
                // inherits our batch (a new activation discards it anyway).
                if let Ok(h) = tokio::runtime::Handle::try_current() {
                    let (inner, id) = (self.inner.clone(), self.id);
                    h.spawn(async move {
                        let mut c = inner.container.lock().await;
                        release(&inner, id, c.as_mut());
                    });
                } else {
                    release(&self.inner, self.id, None);
                }
            }
        }
        self.record(EndReason::Internal);
    }
}

/// A slot among the sessions waiting for approval.
struct AwaitSlot<'a>(&'a Inner);

impl<'a> AwaitSlot<'a> {
    fn acquire(inner: &'a Inner) -> Option<Self> {
        // Increment-if-below-limit (no `fetch_update`: deprecated on newer stable).
        let mut cur = inner.awaiting.load(Ordering::SeqCst);
        while cur < MAX_AWAITING_APPROVAL {
            match inner
                .awaiting
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(Self(inner)),
                Err(now) => cur = now,
            }
        }
        None
    }
}

impl Drop for AwaitSlot<'_> {
    fn drop(&mut self) {
        self.0.awaiting.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) async fn run(
    inner: Arc<Inner>,
    id: u64,
    mut ch: Channel,
    sas: String,
    mut ctl: mpsc::UnboundedReceiver<Terminate>,
) -> EndReason {
    let mut cleanup = Cleanup {
        inner: inner.clone(),
        id,
        started_at_ms: now_ms(),
        device_name: String::new(),
        armed: true,
    };
    let mut ack = None;
    let reason = drive(
        &inner,
        id,
        &mut ch,
        &sas,
        &mut ctl,
        &mut cleanup.device_name,
        &mut ack,
    )
    .await;
    cleanup.release_active().await;
    ch.close().await;
    if let Some(ack) = ack {
        let _ = ack.send(());
    }
    cleanup.record(reason);
    tracing::info!(
        session = id,
        reason = reason.as_str(),
        "vault session ended"
    );
    // Between sessions: compact if the policy says so (skipped if another
    // session is already active). A failure leaves the container as it was.
    if let Err(e) = crate::compact_if_needed(&inner).await {
        tracing::warn!(error = %e, "vault compaction failed");
    }
    reason
}

enum Ev<'a> {
    Ctl(Option<Terminate>),
    Msg(Result<Zeroizing<Vec<u8>>, ChannelError>),
    Permit(SemaphorePermit<'a>),
    Approval(Option<bool>),
    Timeout,
    Idle,
}

/// Outcome of a message received before approval.
enum Pre {
    /// `Ping`, answered.
    Continue,
    Open(u64, String),
    End(EndReason),
}

async fn pre_approval(ch: &mut Channel, m: Result<Zeroizing<Vec<u8>>, ChannelError>) -> Pre {
    let bytes = match m {
        Ok(b) => b,
        Err(e) => return Pre::End(fail(ch, e).await),
    };
    let req = match decode(ch, &bytes).await {
        Ok(r) => r,
        Err(r) => return Pre::End(r),
    };
    match req.body {
        RequestBody::Ping => match respond(ch, req.id, ResponseBody::Ok).await {
            Ok(()) => Pre::Continue,
            Err(e) => Pre::End(e.into()),
        },
        RequestBody::OpenSession { device_name } => Pre::Open(req.id, device_name),
        other => {
            let _ = respond(
                ch,
                req.id,
                error(ErrorCode::NotApproved, "session not approved"),
            )
            .await;
            wipe_request(other);
            Pre::End(EndReason::NotApproved)
        }
    }
}

/// A second `OpenSession` while one is pending approval.
async fn duplicate_open(ch: &mut Channel, id: u64) -> EndReason {
    let _ = respond(
        ch,
        id,
        error(ErrorCode::BadRequest, "duplicate OpenSession"),
    )
    .await;
    EndReason::ProtocolError
}

/// Runs the session until it ends; sends the final response or notice itself.
#[allow(clippy::too_many_arguments)]
async fn drive(
    inner: &Inner,
    id: u64,
    ch: &mut Channel,
    sas: &str,
    ctl: &mut mpsc::UnboundedReceiver<Terminate>,
    device_name: &mut String,
    ack: &mut Option<oneshot::Sender<()>>,
) -> EndReason {
    let cfg = &inner.config;

    // AwaitApproval 1: wait for OpenSession (Pings are answered).
    let deadline = Instant::now() + cfg.approval_timeout;
    let open_id = loop {
        let ev = tokio::select! {
            biased;
            c = ctl.recv() => Ev::Ctl(c),
            _ = sleep_until(deadline) => Ev::Timeout,
            m = ch.recv() => Ev::Msg(m),
        };
        match ev {
            Ev::Ctl(c) => return terminated(ch, c, ack).await,
            Ev::Timeout => return EndReason::ApprovalTimeout,
            Ev::Msg(m) => match pre_approval(ch, m).await {
                Pre::Continue => {}
                Pre::Open(i, name) => {
                    *device_name = name;
                    break i;
                }
                Pre::End(r) => return r,
            },
            _ => unreachable!(),
        }
    };

    let Some(slot) = AwaitSlot::acquire(inner) else {
        let _ = respond(ch, open_id, ResponseBody::Denied).await;
        return EndReason::Busy;
    };

    // AwaitApproval 2: wait for the (server-wide, one at a time) approval prompt.
    let deadline = Instant::now() + cfg.approval_timeout;
    let acquire = inner.approval.acquire();
    tokio::pin!(acquire);
    let permit = loop {
        let ev = tokio::select! {
            biased;
            c = ctl.recv() => Ev::Ctl(c),
            _ = sleep_until(deadline) => Ev::Timeout,
            p = &mut acquire => match p {
                Ok(p) => Ev::Permit(p),
                Err(_) => Ev::Timeout,
            },
            m = ch.recv() => Ev::Msg(m),
        };
        match ev {
            Ev::Ctl(c) => return terminated(ch, c, ack).await,
            Ev::Timeout => {
                let _ = respond(ch, open_id, ResponseBody::Denied).await;
                return EndReason::ApprovalTimeout;
            }
            Ev::Permit(p) => break p,
            Ev::Msg(m) => match pre_approval(ch, m).await {
                Pre::Continue => {}
                Pre::Open(i, _) => return duplicate_open(ch, i).await,
                Pre::End(r) => return r,
            },
            _ => unreachable!(),
        }
    };

    // AwaitApproval 3: the prompt is shown; its timeout starts now.
    tracing::info!(session = id, "vault session: awaiting user approval");
    let approve = timeout(
        cfg.approval_timeout,
        inner.approver.approve(device_name, sas),
    );
    tokio::pin!(approve);
    let approved = loop {
        let ev = tokio::select! {
            biased;
            c = ctl.recv() => Ev::Ctl(c),
            a = &mut approve => Ev::Approval(a.ok()),
            m = ch.recv() => Ev::Msg(m),
        };
        match ev {
            Ev::Ctl(c) => return terminated(ch, c, ack).await,
            Ev::Approval(a) => break a,
            Ev::Msg(m) => match pre_approval(ch, m).await {
                Pre::Continue => {}
                Pre::Open(i, _) => return duplicate_open(ch, i).await,
                Pre::End(r) => return r,
            },
            _ => unreachable!(),
        }
    };
    drop(permit);
    drop(slot);
    match approved {
        None => {
            let _ = respond(ch, open_id, ResponseBody::Denied).await;
            return EndReason::ApprovalTimeout;
        }
        Some(false) => {
            let _ = respond(ch, open_id, ResponseBody::Denied).await;
            return EndReason::Denied;
        }
        Some(true) => {}
    }

    // Become the active session: atomically (under the container lock) take over
    // and discard the old session's uncommitted writes, then end the old session.
    let old = {
        let mut c = inner.container.lock().await;
        let Some(c) = c.as_mut() else {
            let _ = send_notice(ch, Notice::Locked).await;
            return EndReason::Locked;
        };
        c.discard_pending();
        let mut st = inner.state();
        let old = st.active.replace(id);
        old.and_then(|o| st.sessions.get(&o).cloned())
    };
    if let Some(tx) = old {
        let (atx, arx) = oneshot::channel();
        let sent = tx.send(Terminate {
            reason: EndReason::Replaced,
            ack: Some(atx),
        });
        if sent.is_ok() {
            let _ = timeout(REPLACE_ACK_TIMEOUT, arx).await;
        }
    }
    if let Err(e) = respond(ch, open_id, ResponseBody::Approved).await {
        return e.into();
    }
    tracing::info!(session = id, "vault session: approved");

    // Active.
    let deadline = Instant::now() + cfg.session_timeout;
    let mut idle_at = cfg.idle_timeout.map(|d| Instant::now() + d);
    loop {
        let idle = async {
            match idle_at {
                Some(t) => sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        let ev = tokio::select! {
            biased;
            c = ctl.recv() => Ev::Ctl(c),
            _ = sleep_until(deadline) => Ev::Timeout,
            _ = idle => Ev::Idle,
            m = ch.recv() => Ev::Msg(m),
        };
        let req = match ev {
            Ev::Ctl(c) => return terminated(ch, c, ack).await,
            Ev::Timeout => {
                let _ = send_notice(ch, terminated_notice("timeout")).await;
                return EndReason::Timeout;
            }
            Ev::Idle => {
                let _ = send_notice(ch, terminated_notice("idle")).await;
                return EndReason::IdleTimeout;
            }
            Ev::Msg(Err(e)) => return fail(ch, e).await,
            Ev::Msg(Ok(bytes)) => match decode(ch, &bytes).await {
                Ok(r) => r,
                Err(r) => return r,
            },
            _ => unreachable!(),
        };
        if matches!(req.body, RequestBody::Ping) {
            // Keepalive: answered, but not activity for the idle timeout.
            if let Err(e) = respond(ch, req.id, ResponseBody::Ok).await {
                return e.into();
            }
            continue;
        }
        idle_at = cfg.idle_timeout.map(|d| Instant::now() + d);
        let close = matches!(req.body, RequestBody::Close);
        let body = handle(inner, id, req.body).await;
        if let Err(e) = respond(ch, req.id, body).await {
            return e.into();
        }
        if close {
            return EndReason::Closed;
        }
    }
}

/// Decode a request; on failure answer `Error` with id 0 (the real id is
/// unknown) and end the session.
async fn decode(ch: &mut Channel, bytes: &[u8]) -> Result<Request, EndReason> {
    match Request::decode(bytes) {
        Ok(r) => Ok(r),
        Err(e) => {
            let code = match e {
                ProtoError::TooLarge => ErrorCode::TooLarge,
                _ => ErrorCode::BadRequest,
            };
            let _ = respond(ch, 0, error(code, "malformed request")).await;
            Err(EndReason::ProtocolError)
        }
    }
}

/// Map a channel failure to an end reason, answering `Error` (id 0) first when
/// the Noise session can still carry it.
async fn fail(ch: &mut Channel, e: ChannelError) -> EndReason {
    if let ChannelError::Protocol(Some(code)) = e {
        let _ = respond(ch, 0, error(code, "malformed message")).await;
    }
    e.into()
}

impl From<ChannelError> for EndReason {
    fn from(e: ChannelError) -> Self {
        match e {
            ChannelError::Disconnected => EndReason::Disconnected,
            ChannelError::Protocol(_) => EndReason::ProtocolError,
        }
    }
}

fn terminated_notice(reason: &str) -> Notice {
    Notice::SessionTerminated {
        reason: reason.into(),
    }
}

/// Send the notice for a termination request and keep its ack for after the close.
async fn terminated(
    ch: &mut Channel,
    t: Option<Terminate>,
    ack: &mut Option<oneshot::Sender<()>>,
) -> EndReason {
    // A dropped control channel only happens on lock.
    let (reason, a) = t.map_or((EndReason::Locked, None), |t| (t.reason, t.ack));
    *ack = a;
    let notice = match reason {
        EndReason::Locked => Notice::Locked,
        other => terminated_notice(other.as_str()),
    };
    let _ = send_notice(ch, notice).await;
    reason
}

async fn send_notice(ch: &mut Channel, n: Notice) -> Result<(), ChannelError> {
    ch.send(&VaultMessage::Notice(n).encode()).await
}

async fn respond(ch: &mut Channel, id: u64, body: ResponseBody) -> Result<(), ChannelError> {
    let msg = VaultMessage::Response(Response { id, body });
    let mut bytes = Zeroizing::new(msg.encode());
    wipe_message(msg);
    if bytes.len() > MAX_MESSAGE {
        bytes.zeroize();
        let msg = VaultMessage::Response(Response {
            id,
            body: error(ErrorCode::TooLarge, "response too large"),
        });
        return ch.send(&msg.encode()).await;
    }
    ch.send(&bytes).await
}

fn error(code: ErrorCode, msg: &str) -> ResponseBody {
    ResponseBody::Error {
        code,
        msg: msg.into(),
    }
}

fn storage_error(e: &vault_container::Error) -> ResponseBody {
    use vault_container::Error as E;
    match e {
        E::Conflict => error(ErrorCode::Conflict, "write conflict"),
        E::Unsupported(_) => error(ErrorCode::TooLarge, "record too large"),
        E::Rollback | E::DivergedReplicas(_) => {
            tracing::warn!("vault storage integrity failure (rollback or diverged replicas)");
            error(ErrorCode::Storage, "storage integrity failure")
        }
        _ => error(ErrorCode::Storage, "storage failure"),
    }
}

/// Serve one request against the container. Holds the container lock for the
/// whole request (including `Commit`), so requests are serialized.
async fn handle(inner: &Inner, id: u64, body: RequestBody) -> ResponseBody {
    let mut guard = inner.container.lock().await;
    let Some(c) = guard.as_mut() else {
        wipe_request(body);
        return error(ErrorCode::Locked, "vault is locked");
    };
    if inner.state().active != Some(id) {
        wipe_request(body);
        return error(ErrorCode::NotApproved, "session no longer active");
    }
    let r = apply(c, &body).await;
    wipe_request(body);
    r
}

async fn apply(c: &mut VaultContainer, body: &RequestBody) -> ResponseBody {
    match body {
        RequestBody::OpenSession { .. } => error(ErrorCode::BadRequest, "session already open"),
        RequestBody::Get { key } => match c.get(key) {
            Some(mut v) if v.len() > MAX_VALUE => {
                v.zeroize();
                error(ErrorCode::TooLarge, "value too large")
            }
            v => ResponseBody::Value(v),
        },
        RequestBody::Put { key, value } => match c.put(key, value) {
            Ok(()) => ResponseBody::Ok,
            Err(e) => storage_error(&e),
        },
        RequestBody::PutIfAbsent { key, value } => match c.put_if_absent(key, value) {
            Ok(b) => ResponseBody::Bool(b),
            Err(e) => storage_error(&e),
        },
        RequestBody::Delete { key } => {
            c.delete(key);
            ResponseBody::Ok
        }
        RequestBody::ScanPrefix { prefix } => {
            let entries = c.scan_prefix(prefix);
            let mut total = 0usize;
            let fits = entries.iter().all(|(k, v)| {
                total = total.saturating_add(k.len() + v.len());
                v.len() <= MAX_VALUE && total <= MAX_ENTRIES_TOTAL
            });
            if fits {
                ResponseBody::Entries(entries)
            } else {
                wipe_entries(entries);
                error(ErrorCode::TooLarge, "scan result too large")
            }
        }
        RequestBody::Commit => match c.commit().await {
            Ok(seq) => ResponseBody::Committed { seq },
            Err(e) => {
                tracing::warn!("vault commit failed");
                storage_error(&e)
            }
        },
        RequestBody::Clear => {
            let all = c.scan_prefix(b"");
            for (k, _) in &all {
                c.delete(k);
            }
            wipe_entries(all);
            ResponseBody::Ok
        }
        RequestBody::Close | RequestBody::Ping => ResponseBody::Ok,
    }
}

fn wipe_entries(entries: Vec<(Vec<u8>, Vec<u8>)>) {
    for (mut k, mut v) in entries {
        k.zeroize();
        v.zeroize();
    }
}

fn wipe_request(body: RequestBody) {
    match body {
        RequestBody::Get { mut key } | RequestBody::Delete { mut key } => key.zeroize(),
        RequestBody::ScanPrefix { mut prefix } => prefix.zeroize(),
        RequestBody::Put { mut key, mut value }
        | RequestBody::PutIfAbsent { mut key, mut value } => {
            key.zeroize();
            value.zeroize();
        }
        RequestBody::OpenSession { .. }
        | RequestBody::Commit
        | RequestBody::Clear
        | RequestBody::Close
        | RequestBody::Ping => {}
    }
}

fn wipe_message(msg: VaultMessage) {
    if let VaultMessage::Response(r) = msg {
        match r.body {
            ResponseBody::Value(Some(mut v)) => v.zeroize(),
            ResponseBody::Entries(es) => wipe_entries(es),
            _ => {}
        }
    }
}
