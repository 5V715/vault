// SPDX-License-Identifier: MIT
//! The pairing lifecycle: QR offer, Noise handshake, the two-sided code check
//! (the vault's `Approved` and the user's local "Codes match"), keepalive
//! pings and the end of the session.

use crate::backend::RemoteBackend;
use crate::error::{ClientError, CloseReason, perr};
use crate::rpc::{self, Rpc};
use crate::transport::{Clock, MailboxFactory, MailboxTransport, Rng, mailbox_url};
use futures::FutureExt;
use futures::future::{Either, LocalBoxFuture, select};
use futures::pin_mut;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::task::{Poll, Waker};
use vault_proto::noise::PairKeys;

/// Milliseconds between keepalive pings (protocol: at least every 30 s).
pub const PING_INTERVAL_MS: u64 = 30_000;
/// How long to wait for the phone to scan (the relay expires the mailbox after
/// 60 s; this only guards against a relay that never answers).
const PAIR_WAIT_MS: u64 = 75_000;
/// A ping unanswered for this long ends the session.
const PING_TIMEOUT_MS: u64 = 30_000;
/// A goodbye (`Close`) is not waited for longer than this.
const CLOSE_WAIT_MS: u64 = 2_000;

/// State of the vault session, reported through [`SessionCallbacks::on_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// The QR code is shown; waiting for the phone.
    Pairing,
    /// The SAS is shown; waiting for the user's approval on the phone and the
    /// local confirmation ([`VaultSession::confirm_sas`]).
    AwaitingApproval,
    /// Both approved; the store is open ([`VaultSession::backend`]).
    Active,
    /// The session ended (cache and buffered writes are gone) with this reason.
    Closed(CloseReason),
}

/// Runs once both sides approved, with the session's backend, before `Active`
/// is reported. `Err(reason)` ends the session with [`CloseReason::Failed`].
///
/// Hooks run inside the session future: they must not await
/// [`VaultSession::lock`] (it waits for that future, so it would never return).
pub type ActivateHook = Rc<dyn Fn(RemoteBackend) -> LocalBoxFuture<'static, Result<(), String>>>;
/// Runs once the session ended and its backend was discarded, before `Closed`
/// is reported. Must not await [`VaultSession::lock`] (see [`ActivateHook`]).
pub type DeactivateHook = Rc<dyn Fn() -> LocalBoxFuture<'static, ()>>;

/// Callbacks of [`VaultSession::pair`]. They are called on the task that runs
/// the session future.
#[derive(Clone)]
pub struct SessionCallbacks {
    /// The QR text (`<QR prefix>...`) to show.
    pub on_offer: Rc<dyn Fn(&str)>,
    /// The 6-digit code to compare with the phone's.
    pub on_sas: Rc<dyn Fn(&str)>,
    pub on_state: Rc<dyn Fn(SessionState)>,
    /// Optional host step between approval and `Active` (for example loading
    /// the app's account from the store).
    pub on_activate: Option<ActivateHook>,
    /// Optional host step between the end of the session and `Closed` (for
    /// example stopping the app's own server connection).
    pub on_deactivate: Option<DeactivateHook>,
}

impl SessionCallbacks {
    pub fn new(
        on_offer: Rc<dyn Fn(&str)>,
        on_sas: Rc<dyn Fn(&str)>,
        on_state: Rc<dyn Fn(SessionState)>,
    ) -> Self {
        Self {
            on_offer,
            on_sas,
            on_state,
            on_activate: None,
            on_deactivate: None,
        }
    }

    pub fn with_activate(mut self, hook: ActivateHook) -> Self {
        self.on_activate = Some(hook);
        self
    }

    pub fn with_deactivate(mut self, hook: DeactivateHook) -> Self {
        self.on_deactivate = Some(hook);
        self
    }
}

/// Tunables of [`VaultSession::pair_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionOptions {
    /// Milliseconds between keepalive pings (also while the approval is
    /// pending). Must be in `1..=PING_INTERVAL_MS` (the protocol requires a ping
    /// at least every 30 s); [`VaultSession::pair_with`] refuses other values.
    pub ping_interval_ms: u64,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            ping_interval_ms: PING_INTERVAL_MS,
        }
    }
}

/// One-shot broadcast flag with async waiters.
#[derive(Default)]
struct Signal {
    set: Cell<bool>,
    wakers: RefCell<Vec<Waker>>,
}

impl Signal {
    fn fire(&self) {
        self.set.set(true);
        for w in self.wakers.borrow_mut().drain(..) {
            w.wake();
        }
    }

    async fn wait(&self) {
        futures::future::poll_fn(|cx| {
            if self.set.get() {
                Poll::Ready(())
            } else {
                self.wakers.borrow_mut().push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }
}

/// Race `fut` against `stop` (`lock()`) and `cancel` (`cancel_pairing()`);
/// `Err` with the close reason if either fires first.
async fn unless_ended<T>(
    fut: impl Future<Output = T>,
    stop: &Signal,
    cancel: &Signal,
) -> Result<T, CloseReason> {
    let s = stop.wait().fuse();
    let c = cancel.wait().fuse();
    let fut = fut.fuse();
    pin_mut!(fut, s, c);
    futures::select_biased! {
        () = c => Err(CloseReason::Denied),
        () = s => Err(CloseReason::Cancelled),
        v = fut => Ok(v),
    }
}

/// The user's answers in the browser to "does the phone show this code?".
/// "Codes match" and "Cancel" are independent: Cancel wins over a prior
/// "Codes match" until the session is `Active`.
#[derive(Default)]
struct SasCheck {
    /// The code has been reported (`on_sas`); confirming before that is refused.
    shown: Cell<bool>,
    confirmed: Cell<bool>,
    confirmed_sig: Signal,
    cancelled: Cell<bool>,
    cancelled_sig: Signal,
}

impl SasCheck {
    fn confirm(&self) {
        self.confirmed.set(true);
        self.confirmed_sig.fire();
    }

    fn cancel(&self) {
        self.cancelled.set(true);
        self.cancelled_sig.fire();
    }
}

struct Shared {
    state: RefCell<SessionState>,
    /// Set from the two-sided approval until the session ends.
    backend: RefCell<Option<RemoteBackend>>,
    /// The local half of the two-sided SAS check.
    sas_check: SasCheck,
    /// Fired by [`VaultSession::lock`] and by dropping the handle.
    stop: Signal,
    /// Fired when the session future finished (or was dropped).
    done: Signal,
    /// The host released the session ([`VaultSession::lock`] or drop).
    released: Cell<bool>,
}

/// Fires `done` when dropped (also when the session future is dropped without
/// ever being polled).
struct DoneOnDrop(Rc<Shared>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.done.fire();
    }
}

fn valid_device_name(name: &str) -> bool {
    name.len() <= vault_proto::msg::MAX_DEVICE_NAME && !name.chars().any(char::is_control)
}

/// Handle to one vault session (one pairing; pair again with a new one).
///
/// Dropping the handle ends the session like [`Self::lock`] but does not wait
/// for it to wind down.
pub struct VaultSession {
    shared: Rc<Shared>,
}

impl Drop for VaultSession {
    fn drop(&mut self) {
        self.shared.released.set(true);
        self.shared.stop.fire();
    }
}

impl VaultSession {
    /// Pair with a vault through the relay at `relay_origin`, declaring the app
    /// `app_id` (`^[a-z0-9][a-z0-9.-]{0,63}$`) and the page's web `origin`
    /// (`location.origin` in a browser; normalized: lower-case host, default port
    /// dropped; https, or http only for loopback; no path). The vault refuses a
    /// pairing for another app or origin than it is bound to, before asking the
    /// user: the session then ends with [`CloseReason::WrongApp`]. Returns the handle
    /// and the future that runs the whole session (spawn it on a local
    /// executor and let it run to completion; dropping it early skips the
    /// cleanup that discards the backend): it reports the QR text, the SAS and the [`SessionState`]s.
    /// Once `Active`, [`Self::backend`] is the vault-backed store. When the
    /// session ends (disconnect, the vault's `SessionTerminated`/`Locked`, a
    /// failed commit, [`Self::lock`], [`Self::cancel_pairing`] before Active)
    /// the cache and buffered writes are dropped and `Closed` is reported.
    ///
    /// Before both the vault's `Approved` and [`Self::confirm_sas`], only
    /// `OpenSession`, `Ping` and `Close` are sent.
    #[allow(clippy::too_many_arguments)]
    pub fn pair<F, C>(
        factory: F,
        clock: C,
        rng: &dyn Rng,
        relay_origin: &str,
        app_id: &str,
        origin: &str,
        device_name: &str,
        callbacks: SessionCallbacks,
    ) -> Result<(VaultSession, LocalBoxFuture<'static, ()>), ClientError>
    where
        F: MailboxFactory + 'static,
        C: Clock + 'static,
    {
        Self::pair_with(
            factory,
            clock,
            rng,
            relay_origin,
            app_id,
            origin,
            device_name,
            callbacks,
            SessionOptions::default(),
        )
    }

    /// [`Self::pair`] with custom [`SessionOptions`].
    #[allow(clippy::too_many_arguments)]
    pub fn pair_with<F, C>(
        factory: F,
        clock: C,
        rng: &dyn Rng,
        relay_origin: &str,
        app_id: &str,
        origin: &str,
        device_name: &str,
        callbacks: SessionCallbacks,
        options: SessionOptions,
    ) -> Result<(VaultSession, LocalBoxFuture<'static, ()>), ClientError>
    where
        F: MailboxFactory + 'static,
        C: Clock + 'static,
    {
        if !valid_device_name(device_name) {
            return Err(perr("device name too long or with control characters"));
        }
        vault_proto::binding::validate_app_id(app_id).map_err(|e| perr(e.to_string()))?;
        let page_origin =
            vault_proto::binding::normalize_origin(origin).map_err(|e| perr(e.to_string()))?;
        if !(1..=PING_INTERVAL_MS).contains(&options.ping_interval_ms) {
            return Err(perr(format!(
                "ping interval must be 1..={PING_INTERVAL_MS} ms (protocol: at least every 30 s)"
            )));
        }
        let relay = relay_origin.trim_end_matches('/').to_string();
        let mut id = [0u8; 16];
        rng.fill(&mut id);
        let keys = PairKeys::generate(relay.clone(), id, app_id.to_string(), page_origin)
            .map_err(|_| perr("cannot generate pairing keys"))?;
        let qr = keys
            .offer()
            .to_qr()
            .map_err(|e| perr(format!("relay url: {e}")))?;
        let url = mailbox_url(&relay, &id)?;
        let shared = Rc::new(Shared {
            state: RefCell::new(SessionState::Pairing),
            backend: RefCell::new(None),
            sas_check: SasCheck::default(),
            stop: Signal::default(),
            done: Signal::default(),
            released: Cell::new(false),
        });
        let run = Run {
            shared: shared.clone(),
            factory,
            clock,
            cb: callbacks,
            ping_interval_ms: options.ping_interval_ms,
        };
        let done = DoneOnDrop(shared.clone());
        let device = device_name.to_string();
        let fut = async move {
            let _done = done;
            run.session(keys, qr, url, device).await;
        }
        .boxed_local();
        Ok((VaultSession { shared }, fut))
    }

    /// The current state (the last one reported through `on_state`).
    pub fn state(&self) -> SessionState {
        self.shared.state.borrow().clone()
    }

    /// The vault-backed store; `Some` only while the session is `Active`.
    pub fn backend(&self) -> Option<RemoteBackend> {
        if *self.shared.state.borrow() == SessionState::Active {
            self.shared.backend.borrow().clone()
        } else {
            None
        }
    }

    /// The user confirmed in the browser that the phone shows the same code
    /// (the "Codes match" button). The session becomes `Active` only once both
    /// this and the vault's `Approved` happened, in either order; until then no
    /// store request is sent. Refused before the code was shown, after the
    /// pairing was cancelled, and after [`Self::lock`].
    pub fn confirm_sas(&self) -> Result<(), ClientError> {
        let check = &self.shared.sas_check;
        if self.shared.released.get() {
            return Err(perr("no pairing in progress"));
        }
        if !check.shown.get() {
            return Err(perr("no code to confirm yet"));
        }
        if check.cancelled.get() {
            return Err(perr("pairing was cancelled"));
        }
        check.confirm();
        Ok(())
    }

    /// The user cancelled the code comparison (or closed the dialog). Cancel
    /// wins at any point before the session is `Active`, also after "Codes
    /// match" and after the vault's `Approved` (while the host's activation
    /// hook still runs): the session ends with [`CloseReason::Denied`], the
    /// backend is dropped, `on_deactivate` runs and `Active` is never reported.
    /// No store request is sent before both approvals; while `on_activate` runs
    /// the host's own requests (even a commit) may already have reached the
    /// vault, so keep that hook read-only. Also works while the QR is still
    /// shown (`Pairing`): the mailbox wait or handshake is abandoned at once
    /// and the session closes as `Denied`. Idempotent while
    /// pending. Once `Active` (or `Closed`) it changes nothing and returns an
    /// error: [`Self::lock`] ends an active session.
    pub fn cancel_pairing(&self) -> Result<(), ClientError> {
        let check = &self.shared.sas_check;
        match &*self.shared.state.borrow() {
            SessionState::Active => return Err(perr("session is active; use lock")),
            SessionState::Closed(_) => return Err(perr("no pairing in progress")),
            _ => {}
        }
        check.cancel();
        Ok(())
    }

    /// End the session (a goodbye `Close` to the vault, then
    /// [`CloseReason::Locked`]; [`CloseReason::Cancelled`] before the
    /// handshake finished) and wait until the session future finished.
    pub async fn lock(&self) {
        self.shared.released.set(true);
        self.shared.stop.fire();
        self.shared.done.wait().await;
    }
}

/// Everything the session future owns.
struct Run<F, C> {
    shared: Rc<Shared>,
    factory: F,
    clock: C,
    cb: SessionCallbacks,
    ping_interval_ms: u64,
}

impl<F: MailboxFactory, C: Clock> Run<F, C> {
    fn set_state(&self, s: SessionState) {
        *self.shared.state.borrow_mut() = s.clone();
        (self.cb.on_state)(s);
    }

    /// The whole vault session: handshake, approval, serving until it ends,
    /// then discard everything and report `Closed`.
    async fn session(&self, keys: PairKeys, qr: String, url: String, device: String) {
        self.set_state(SessionState::Pairing);
        (self.cb.on_offer)(&qr);
        let reason = self.run(keys, url, &device).await;
        // End of session: forget everything (the backend dies with its cache).
        let backend = self.shared.backend.borrow_mut().take();
        if let Some(b) = backend {
            b.discard();
        }
        if let Some(h) = &self.cb.on_deactivate {
            h().await;
        }
        self.set_state(SessionState::Closed(reason));
    }

    async fn run(&self, keys: PairKeys, url: String, device: &str) -> CloseReason {
        let stop = &self.shared.stop;
        let cancel = &self.shared.sas_check.cancelled_sig;
        let transport = match unless_ended(self.factory.open_mailbox(&url), stop, cancel).await {
            Err(r) => return r,
            Ok(Err(e)) => return CloseReason::RelayUnreachable(e.to_string()),
            Ok(Ok(t)) => t,
        };
        let hs = rpc::handshake(&transport, keys, self.clock.sleep(PAIR_WAIT_MS));
        let noise = match unless_ended(hs, stop, cancel).await {
            Err(r) => {
                transport.close();
                return r;
            }
            Ok(Err(e)) => {
                transport.close();
                return CloseReason::PairingFailed(e.to_string());
            }
            Ok(Ok(n)) => n,
        };
        let rpc = Rc::new(Rpc::new(Box::new(transport), noise));
        (self.cb.on_sas)(&rpc.sas());
        self.shared.sas_check.shown.set(true);
        self.set_state(SessionState::AwaitingApproval);

        let drive = rpc.drive().fuse();
        let main = self.vault_main(&rpc, device).fuse();
        let ping = self.ping_loop(&rpc).fuse();
        let stopped = stop.wait().fuse();
        // Cancel wins until the session is Active (`vault_main` re-checks the
        // flag right before reporting Active, with no await in between).
        let cancelled = self.shared.sas_check.cancelled_sig.wait().fuse();
        pin_mut!(drive, main, ping, stopped, cancelled);
        let (reason, goodbye) = futures::select_biased! {
            () = cancelled => (CloseReason::Denied, true),
            r = main => (r, false),
            r = drive => (r, false),
            r = ping => (r, false),
            () = stopped => (CloseReason::Locked, true),
        };
        if goodbye {
            // Our own goodbye; the vault ends the session.
            let bye = rpc.close();
            let wait = self.clock.sleep(CLOSE_WAIT_MS);
            pin_mut!(bye, wait);
            select(bye, wait).await;
        }
        rpc.fail(reason.clone());
        reason
    }

    /// Approval, then the open store; returns only to report why it cannot go on.
    async fn vault_main(&self, rpc: &Rc<Rpc>, device: &str) -> CloseReason {
        if let Err(e) = rpc::open_session(rpc, device).await {
            return match e {
                ClientError::Denied => CloseReason::Denied,
                ClientError::Closed(CloseReason::WrongApp) => CloseReason::WrongApp,
                e => CloseReason::PairingFailed(e.to_string()),
            };
        }
        // Two-sided check: the vault approved, now the user must confirm in
        // the browser too (or already did). Whoever holds the QR text could
        // have answered as the vault; only the user can tell that their phone
        // showed this code. Until then the channel stays closed for requests.
        self.shared.sas_check.confirmed_sig.wait().await;
        if self.shared.sas_check.cancelled.get() {
            return CloseReason::Denied;
        }
        rpc.set_approved();
        let backend = RemoteBackend::new(rpc.clone());
        *self.shared.backend.borrow_mut() = Some(backend.clone());
        if let Some(h) = &self.cb.on_activate
            && let Err(reason) = h(backend).await
        {
            return CloseReason::Failed(reason);
        }
        // Cancel may have arrived while the activation hook ran.
        if self.shared.sas_check.cancelled.get() {
            return CloseReason::Denied;
        }
        self.set_state(SessionState::Active);
        std::future::pending().await
    }

    /// Keepalive: a ping every `ping_interval_ms` (also while the approval is
    /// pending). Returns why the session ended.
    async fn ping_loop(&self, rpc: &Rc<Rpc>) -> CloseReason {
        loop {
            self.clock.sleep(self.ping_interval_ms).await;
            let call = rpc.call(vault_proto::RequestBody::Ping);
            let timeout = self.clock.sleep(PING_TIMEOUT_MS);
            pin_mut!(call, timeout);
            match select(call, timeout).await {
                Either::Left((Ok(_), _)) => {}
                Either::Left((Err(e), _)) => return CloseReason::KeepaliveFailed(e.to_string()),
                Either::Right(_) => return CloseReason::KeepaliveTimedOut,
            }
        }
    }
}
