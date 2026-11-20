// SPDX-License-Identifier: MIT
//! Vault side of the vault protocol (`docs/protocol/vault.md`).
//!
//! Given a scanned [`PairOffer`], [`VaultServer::serve_offer`] dials the relay
//! mailbox, runs the Noise XX responder with a fresh random static key, then (in
//! a spawned task) checks the offer's app id and origin against the container's
//! binding, waits for the browser's `OpenSession`, asks the [`Approver`] (app,
//! origin, relay host, device name, SAS), unlocks the container if it is locked
//! and serves `KvBackend`-style requests against the encrypted [`Container`].
//!
//! Two ways to run it:
//! - **Locked pairing** ([`VaultServer::new_locked`], and after
//!   [`VaultServer::lock`]): the server holds only the storage backend and the
//!   binding read from the plaintext header. Pairing, the binding check and the
//!   prompt need no key; the approver returns the [`UnlockMaterial`] (on Android
//!   the CRK unwrapped by the biometric prompt) and the server opens the
//!   container with it before it sends `Approved`. If it cannot open it, the
//!   browser is denied ([`EndReason::UnlockFailed`]) and the vault stays locked.
//! - **Unlocked** ([`VaultServer::new`], [`VaultServer::unlock`]): the container
//!   is already open; an approval needs no material.
//!
//! Invariants:
//! - The Noise static key is random per offer (OS RNG), held in `Zeroizing`
//!   memory only while the handshake is set up, and never stored or reused. The
//!   protocol does not pin it: the 6-digit code authenticates every session.
//! - A session whose offer names another app or origin than the container is
//!   bound to gets `Notice::WrongApp` right after the handshake and ends
//!   ([`EndReason::WrongApp`]): before any request is read, before the approver
//!   is asked and without touching the container. The binding is that of the
//!   container installed (or, while locked, of its header) at the time of each
//!   check: right after the handshake, again immediately before the approver is
//!   asked and again (under the container lock, on the opened container) before
//!   `Approved` is granted, so a `lock`/`unlock` that swaps the container in
//!   between cannot let a mismatched offer through.
//! - A taken mailbox (HTTP 409) is reported to the approver
//!   ([`ApprovalRequest::mailbox_taken`]) as a warning; its answer is always a
//!   denial and [`VaultServer::serve_offer`] fails with
//!   [`ServerError::MailboxTaken`].
//! - One approval prompt at a time (server-wide); at most
//!   [`MAX_AWAITING_APPROVAL`] sessions wait for approval, further `OpenSession`s
//!   are answered `Denied`. Each prompt's timeout starts when it is shown.
//! - At most one approved (active) session. A newly approved session ends the
//!   old one (`Notice::SessionTerminated{"replaced"}`) and discards its
//!   uncommitted writes before it is answered `Approved`.
//! - Container access is serialized (one async mutex); a session's requests are
//!   processed in order, and every request re-checks under that mutex that its
//!   session is still the active one.
//! - Writes are staged in the container's pending batch until `Commit`; a
//!   session that ends with uncommitted writes has them discarded (the browser
//!   never got a `Committed` for them).
//! - [`VaultServer::lock`] ends all sessions (`Notice::Locked`), discards pending
//!   writes and drops the container (keeping its backend, binding and high-water
//!   mark for locked pairing); [`VaultServer::unlock`] installs a re-opened
//!   container.
//! - After a session ends the container is compacted when
//!   [`CompactionPolicy`] says so, and only while no session is active and
//!   nothing is staged; never on a commit path.
//! - Nothing here logs keys, values or the SAS.

mod approver;
mod session;
mod transport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, mpsc};
use vault_container::backend::ContainerBackend;
use vault_container::engine::{Container, HighWater};
use vault_proto::PairOffer;
use vault_proto::noise::Responder;
use zeroize::Zeroizing;

pub use approver::{
    ApprovalDecision, ApprovalRequest, Approver, MAILBOX_TAKEN_WARNING, TerminalApprover,
    UnlockMaterial,
};

/// Sessions that may wait for (or be in) user approval at the same time.
pub const MAX_AWAITING_APPROVAL: usize = 2;

/// The backend type the vault's container is opened on.
pub type SharedBackend = Arc<dyn ContainerBackend>;
/// The container type served to browsers.
pub type VaultContainer = Container<SharedBackend>;

/// Default [`CompactionPolicy::max_segments`]: compact once the manifest lists
/// this many segments (each commit adds at least one 64 KiB segment, and every
/// session start decrypts all of them).
pub const COMPACT_MAX_SEGMENTS: usize = 64;
/// Default [`CompactionPolicy::max_dead_ratio`]: compact once at least half of
/// the stored record bytes are overwritten or deleted.
pub const COMPACT_MAX_DEAD_RATIO: f64 = 0.5;
/// Default [`CompactionPolicy::min_segments_for_ratio`]: the dead-bytes rule
/// applies from this many segments on (a fresh container is not rewritten
/// after every session).
pub const COMPACT_MIN_SEGMENTS_FOR_RATIO: usize = 8;

/// When the vault compacts its container. Checked after every session ends
/// (and by [`VaultServer::compact_if_needed`]), only while no session is active
/// and nothing is staged; never on a session's commit path.
#[derive(Clone, Debug)]
pub struct CompactionPolicy {
    /// Compact when the manifest lists at least this many segments.
    pub max_segments: usize,
    /// Compact when [`vault_container::engine::Stats::dead_ratio`] is at least this
    /// (and at least `min_segments_for_ratio` segments exist).
    pub max_dead_ratio: f64,
    pub min_segments_for_ratio: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            max_segments: COMPACT_MAX_SEGMENTS,
            max_dead_ratio: COMPACT_MAX_DEAD_RATIO,
            min_segments_for_ratio: COMPACT_MIN_SEGMENTS_FOR_RATIO,
        }
    }
}

impl CompactionPolicy {
    /// Whether a container with these stats should be compacted.
    pub fn due(&self, st: &vault_container::engine::Stats) -> bool {
        // One segment (or none) is already as compact as it gets.
        if st.segments <= 1 {
            return false;
        }
        st.segments >= self.max_segments
            || (st.segments >= self.min_segments_for_ratio
                && st.dead_ratio() >= self.max_dead_ratio)
    }
}

/// Server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// An approved session ends this long after approval (protocol default 12 h).
    pub session_timeout: Duration,
    /// An approved session ends after this long without a request; `None` disables.
    pub idle_timeout: Option<Duration>,
    /// Limit for receiving `OpenSession`, for waiting for the approval prompt
    /// (prompts are shown one at a time), and for the user's answer; each
    /// starts when the previous step finished.
    pub approval_timeout: Duration,
    /// Limit for connecting to the relay and completing the Noise handshake.
    pub handshake_timeout: Duration,
    /// Limit for sending one application message (a stalled browser ends the session).
    pub send_timeout: Duration,
    /// Extra trusted CA certificates (PEM) for the relay's TLS, besides the webpki roots.
    pub extra_ca_pem: Option<Vec<u8>>,
    /// When to compact the container between sessions.
    pub compaction: CompactionPolicy,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            session_timeout: Duration::from_secs(12 * 60 * 60),
            idle_timeout: None,
            approval_timeout: Duration::from_secs(120),
            handshake_timeout: Duration::from_secs(30),
            send_timeout: Duration::from_secs(60),
            extra_ca_pem: None,
            compaction: CompactionPolicy::default(),
        }
    }
}

/// Errors from [`VaultServer::new`] and [`VaultServer::serve_offer`].
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The container header could not be read (locked pairing needs its binding).
    #[error("container: {0}")]
    Container(vault_container::Error),
    /// The OS random generator failed (no static key for the handshake).
    #[error("no randomness for the handshake key")]
    Rng,
    #[error("pair offer is not usable")]
    BadOffer,
    #[error("relay: {0}")]
    Relay(String),
    /// The relay answered HTTP 409 on the mailbox: two parties already hold it.
    /// Someone other than the browser may have used this pairing text.
    #[error("relay mailbox already taken (HTTP 409)")]
    MailboxTaken,
    #[error("TLS setup: {0}")]
    Tls(&'static str),
    #[error("handshake failed")]
    Handshake,
    #[error("timed out")]
    Timeout,
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndReason {
    /// The browser sent `Close`.
    Closed,
    /// The connection dropped (browser, relay or network).
    Disconnected,
    /// The user denied the approval.
    Denied,
    /// No `OpenSession` or no user answer within the approval timeout.
    ApprovalTimeout,
    /// The browser sent a store request before approval.
    NotApproved,
    /// A newer session was approved.
    Replaced,
    /// The vault was locked.
    Locked,
    /// The session timeout elapsed.
    Timeout,
    /// The idle timeout elapsed.
    IdleTimeout,
    /// Malformed, undecryptable or out-of-sequence traffic.
    ProtocolError,
    /// Too many sessions were already waiting for approval.
    Busy,
    /// The session task failed (panicked); cleanup still ran.
    Internal,
    /// The offer's app id or origin is not the container's binding
    /// (`Notice::WrongApp` was sent; nobody was asked).
    WrongApp,
    /// The user approved, but the vault was locked and the approval's
    /// [`UnlockMaterial`] did not open the container (the browser got `Denied`).
    UnlockFailed,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Disconnected => "disconnected",
            Self::Denied => "denied",
            Self::ApprovalTimeout => "approval timeout",
            Self::NotApproved => "not approved",
            Self::Replaced => "replaced",
            Self::Locked => "locked",
            Self::Timeout => "timeout",
            Self::IdleTimeout => "idle",
            Self::ProtocolError => "protocol error",
            Self::Busy => "busy",
            Self::Internal => "internal error",
            Self::WrongApp => "wrong app",
            Self::UnlockFailed => "unlock failed",
        }
    }
}

impl std::fmt::Display for EndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One finished session (after a successful handshake), for the caller to persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: u64,
    /// As sent in `OpenSession`; empty if the browser never sent one.
    pub device_name: String,
    /// Handshake completion, milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub reason: EndReason,
}

/// A session past the Noise handshake, running in its own task.
pub struct SessionHandle {
    id: u64,
    sas: String,
    task: tokio::task::JoinHandle<EndReason>,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The 6-digit code the browser also shows.
    pub fn sas(&self) -> &str {
        &self.sas
    }

    /// Wait for the session to end. Dropping the handle does not end the session.
    pub async fn finished(self) -> EndReason {
        self.task.await.unwrap_or(EndReason::Internal)
    }
}

/// Sent to a session task to end it.
pub(crate) struct Terminate {
    pub reason: EndReason,
    /// Signalled once the session has stopped serving and sent its notice.
    pub ack: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Default)]
pub(crate) struct State {
    next_id: u64,
    /// The approved session, if any.
    pub active: Option<u64>,
    /// Control channels of all live sessions (approved or not).
    pub sessions: HashMap<u64, mpsc::UnboundedSender<Terminate>>,
    pub log: Vec<SessionRecord>,
}

/// The app binding of the container, as `(app_id, origin)`.
pub(crate) type BindingPair = (String, String);

/// What the offer declared, for the binding check and the approval prompt.
pub(crate) struct Offered {
    pub app_id: String,
    pub origin: String,
    pub relay_host: String,
}

impl Offered {
    /// Whether the offer names exactly this binding. Checked against the
    /// installed container at every decision point (after the handshake, before
    /// the prompt, before `Approved`), never from a value computed earlier: a
    /// `lock`/`unlock` can swap the container at any time.
    pub(crate) fn matches(&self, b: &BindingPair) -> bool {
        self.app_id == b.0 && self.origin == b.1
    }
}

pub(crate) struct Inner {
    /// `None` while locked. Lock order: `container`, then `state`.
    pub container: Mutex<Option<VaultContainer>>,
    /// Binding of the installed container, or while locked of the backend's
    /// header (not secret). Updated under the `container` lock.
    binding: StdMutex<BindingPair>,
    /// Storage of the container, for opening it at approval while locked.
    /// Updated under the `container` lock.
    backend: StdMutex<SharedBackend>,
    /// Mirrors `container.is_none()` for the synchronous [`VaultServer::is_locked`].
    locked: AtomicBool,
    /// High-water mark of the last container this server dropped (on `lock`):
    /// an unlock never accepts an older manifest, whatever the host passes.
    last_high_water: StdMutex<Option<HighWater>>,
    pub approver: Arc<dyn Approver>,
    /// One approval prompt at a time.
    pub approval: tokio::sync::Semaphore,
    /// Sessions between `OpenSession` and the user's answer.
    pub awaiting: std::sync::atomic::AtomicUsize,
    pub config: ServerConfig,
    tls: Arc<rustls::ClientConfig>,
    state: StdMutex<State>,
}

impl Inner {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn binding(&self) -> MutexGuard<'_, BindingPair> {
        self.binding.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn backend(&self) -> MutexGuard<'_, SharedBackend> {
        self.backend.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn last_high_water(&self) -> MutexGuard<'_, Option<HighWater>> {
        self.last_high_water
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Install `c` as the open container (the caller holds the `container` lock).
    pub(crate) fn install(&self, slot: &mut Option<VaultContainer>, c: VaultContainer) {
        *self.binding() = binding_of(&c);
        *self.backend() = c.backend().clone();
        if let Some(mut old) = slot.replace(c) {
            old.discard_pending();
        }
        self.locked.store(false, Ordering::SeqCst);
    }

    /// Open the container with an approval's material (the caller holds the
    /// `container` lock and the vault is locked). The material is consumed and
    /// dropped (zeroized) here, whatever the outcome.
    pub(crate) async fn open_with(
        &self,
        material: UnlockMaterial,
    ) -> Result<VaultContainer, vault_container::Error> {
        let backend = self.backend().clone();
        let floor = *self.last_high_water();
        match material {
            UnlockMaterial::Unlocked => Err(vault_container::Error::Unsupported(
                "approval without unlock material on a locked vault".into(),
            )),
            UnlockMaterial::Crk { crk, high_water } => {
                Container::open_with_crk(backend, crk, newer(high_water, floor)).await
            }
            UnlockMaterial::RecoveryKey { key, high_water } => {
                Container::open_with_recovery(backend, &key, newer(high_water, floor))
                    .await
                    .map(|(c, _crk)| c)
            }
        }
    }
}

/// The stricter of two high-water marks (the higher `seq`; on a tie the one
/// this server saw itself).
fn newer(host: Option<HighWater>, seen: Option<HighWater>) -> Option<HighWater> {
    match (host, seen) {
        (Some(h), Some(s)) if h.seq > s.seq => Some(h),
        (_, Some(s)) => Some(s),
        (h, None) => h,
    }
}

/// A fresh Noise static key for one offer, from the OS RNG.
fn fresh_static_key() -> Result<Zeroizing<[u8; 32]>, ServerError> {
    use rand::TryRngCore;
    let mut k = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng
        .try_fill_bytes(&mut *k)
        .map_err(|_| ServerError::Rng)?;
    Ok(k)
}

pub(crate) fn binding_of(c: &VaultContainer) -> BindingPair {
    let (a, o) = c.binding();
    (a.to_string(), o.to_string())
}

/// Host (and port) of a relay URL, for the approval prompt.
pub fn relay_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// Compact the container if the policy says so, no session is active and no
/// write is staged (compaction folds staged writes in, so it must never run
/// under a session). Holds the container lock throughout, so no request runs
/// meanwhile. Returns the new manifest seq if it compacted.
pub(crate) async fn compact_if_needed(
    inner: &Inner,
) -> Result<Option<u64>, vault_container::Error> {
    let mut guard = inner.container.lock().await;
    let Some(c) = guard.as_mut() else {
        return Ok(None);
    };
    if inner.state().active.is_some() || c.pending_len() > 0 {
        return Ok(None);
    }
    let st = c.stats();
    if !inner.config.compaction.due(&st) {
        return Ok(None);
    }
    let seq = c.compact().await?;
    tracing::info!(
        segments_before = st.segments,
        seq,
        "vault container compacted"
    );
    Ok(Some(seq))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The vault session server. Cheap to clone; clones share all state.
#[derive(Clone)]
pub struct VaultServer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for VaultServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultServer").finish_non_exhaustive()
    }
}

impl VaultServer {
    /// An unlocked server: `container` is open and served as is.
    pub fn new(
        container: VaultContainer,
        approver: Arc<dyn Approver>,
        config: ServerConfig,
    ) -> Result<Self, ServerError> {
        let backend = container.backend().clone();
        let binding = binding_of(&container);
        Self::build(backend, binding, Some(container), approver, config)
    }

    /// A locked server for the container on `backend`: it pairs and checks the
    /// binding (read from the plaintext header, no key needed) and opens the
    /// container only when an approval brings the [`UnlockMaterial`].
    pub async fn new_locked(
        backend: SharedBackend,
        approver: Arc<dyn Approver>,
        config: ServerConfig,
    ) -> Result<Self, ServerError> {
        let b = vault_container::engine::read_binding(&*backend)
            .await
            .map_err(ServerError::Container)?;
        Self::build(backend, (b.app_id, b.origin), None, approver, config)
    }

    fn build(
        backend: SharedBackend,
        binding: BindingPair,
        container: Option<VaultContainer>,
        approver: Arc<dyn Approver>,
        config: ServerConfig,
    ) -> Result<Self, ServerError> {
        let tls = Arc::new(transport::tls_config(config.extra_ca_pem.as_deref())?);
        Ok(Self {
            inner: Arc::new(Inner {
                binding: StdMutex::new(binding),
                backend: StdMutex::new(backend),
                locked: AtomicBool::new(container.is_none()),
                last_high_water: StdMutex::new(None),
                container: Mutex::new(container),
                approver,
                approval: tokio::sync::Semaphore::new(1),
                awaiting: std::sync::atomic::AtomicUsize::new(0),
                config,
                tls,
                state: StdMutex::new(State::default()),
            }),
        })
    }

    /// Dial the offer's relay mailbox and run the Noise handshake (bounded by
    /// `handshake_timeout`) with a fresh random static key; on success the
    /// session continues in a spawned task (binding check, approval, unlock if
    /// locked, then serving) and its handle is returned. Works whether or not the
    /// vault is locked.
    ///
    /// An offer for another app or origin than the container's binding still
    /// gets the handshake, so that the browser learns why: the session then
    /// only sends `Notice::WrongApp` and ends with [`EndReason::WrongApp`].
    ///
    /// If the relay answers that the mailbox is taken (HTTP 409), the approver
    /// is shown an [`ApprovalRequest`] with `mailbox_taken` set (and no code);
    /// its answer is treated as a denial and this returns
    /// [`ServerError::MailboxTaken`]. No session exists, so there is nobody to
    /// send a notice to.
    pub async fn serve_offer(&self, offer: PairOffer) -> Result<SessionHandle, ServerError> {
        // The binding is not decided here: the session task checks it against the
        // container installed at each decision point (see `Offered::matches`).
        let offered = Offered {
            app_id: offer.app_id.clone(),
            origin: offer.origin.clone(),
            relay_host: relay_host(&offer.relay_url).to_string(),
        };
        let responder = {
            let key = fresh_static_key()?;
            Responder::new(*key, &offer).map_err(|_| ServerError::Handshake)?
        };
        let url = transport::mailbox_url(&offer)?;
        let tls = self.inner.tls.clone();
        let handshake = async move {
            let mut ws = transport::connect(&url, tls).await?;
            let m1 = transport::recv_raw(&mut ws).await?;
            let (m2, r) = responder
                .handle_msg1(&m1)
                .map_err(|_| ServerError::Handshake)?;
            transport::send_raw(&mut ws, m2).await?;
            let m3 = transport::recv_raw(&mut ws).await?;
            // Checks the remote static against `offer.browser_eph_pub` (protocol MUST).
            let noise = r.handle_msg3(&m3).map_err(|_| ServerError::Handshake)?;
            Ok::<_, ServerError>((ws, noise))
        };
        let (ws, noise) = match tokio::time::timeout(self.inner.config.handshake_timeout, handshake)
            .await
            .map_err(|_| ServerError::Timeout)?
        {
            Ok(x) => x,
            Err(ServerError::MailboxTaken) => {
                self.report_mailbox_taken(&offered).await;
                return Err(ServerError::MailboxTaken);
            }
            Err(e) => return Err(e),
        };
        let sas = noise.sas().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = {
            let mut st = self.inner.state();
            st.next_id += 1;
            let id = st.next_id;
            st.sessions.insert(id, tx);
            id
        };
        tracing::info!(session = id, "vault session: handshake complete");
        let channel = transport::Channel::new(ws, noise, self.inner.config.send_timeout);
        let task = tokio::spawn(session::run(
            self.inner.clone(),
            id,
            channel,
            sas.clone(),
            offered,
            rx,
        ));
        Ok(SessionHandle { id, sas, task })
    }

    /// Tell the approver (one prompt at a time, bounded by `approval_timeout`)
    /// that the mailbox was taken. Whatever it answers is a denial.
    async fn report_mailbox_taken(&self, offered: &Offered) {
        tracing::warn!("vault pairing: relay mailbox already taken");
        let cfg = &self.inner.config;
        let req = ApprovalRequest {
            device_name: "",
            sas: "",
            app_id: &offered.app_id,
            origin: &offered.origin,
            relay_host: &offered.relay_host,
            mailbox_taken: true,
        };
        let shown = async {
            let _permit = self.inner.approval.acquire().await.ok()?;
            let d = self.inner.approver.approve(&req).await;
            Some(approver::effective(&req, d))
        };
        // The answer is dropped: `effective` made it a denial, and any unlock
        // material it carried is zeroized unused.
        let _ = tokio::time::timeout(cfg.approval_timeout * 2, shown).await;
    }

    /// End all sessions (`Notice::Locked`), discard uncommitted writes and drop
    /// the container. Waits for in-flight requests to finish. The server stays
    /// usable for locked pairing: the next approval must bring
    /// [`UnlockMaterial`].
    pub async fn lock(&self) {
        let mut c = self.inner.container.lock().await;
        let taken = c.take();
        self.inner.locked.store(true, Ordering::SeqCst);
        if let Some(t) = &taken {
            let mut hw = self.inner.last_high_water();
            *hw = newer(Some(t.high_water()), *hw);
        }
        let txs: Vec<_> = {
            let mut st = self.inner.state();
            st.active = None;
            st.sessions.drain().map(|(_, tx)| tx).collect()
        };
        drop(c);
        if let Some(mut taken) = taken {
            taken.discard_pending();
        }
        for tx in txs {
            let _ = tx.send(Terminate {
                reason: EndReason::Locked,
                ack: None,
            });
        }
        tracing::info!("vault locked");
    }

    /// Install a re-opened container (after [`VaultServer::lock`], or in place
    /// of the current one, whose uncommitted writes are discarded).
    pub async fn unlock(&self, container: VaultContainer) {
        let mut c = self.inner.container.lock().await;
        self.inner.install(&mut c, container);
        tracing::info!("vault unlocked");
    }

    /// The container's high-water mark (for the caller to persist), if unlocked.
    pub async fn high_water(&self) -> Option<vault_container::engine::HighWater> {
        self.inner
            .container
            .lock()
            .await
            .as_ref()
            .map(|c| c.high_water())
    }

    /// Size accounting of the container, if unlocked.
    pub async fn container_stats(&self) -> Option<vault_container::engine::Stats> {
        self.inner
            .container
            .lock()
            .await
            .as_ref()
            .map(|c| c.stats())
    }

    /// Run the compaction policy now (it also runs after every session).
    /// Does nothing while a session is active or the vault is locked.
    /// Returns the new manifest seq if it compacted.
    pub async fn compact_if_needed(&self) -> Result<Option<u64>, vault_container::Error> {
        compact_if_needed(&self.inner).await
    }

    /// The container's app binding, `(app_id, origin)` (while locked, as read
    /// from its header); offers for anything else are refused with
    /// `Notice::WrongApp`.
    pub fn binding(&self) -> (String, String) {
        self.inner.binding().clone()
    }

    /// Whether no container is open (locked pairing).
    pub fn is_locked(&self) -> bool {
        self.inner.locked.load(Ordering::SeqCst)
    }

    /// Id of the approved session, if any.
    pub fn active_session(&self) -> Option<u64> {
        self.inner.state().active
    }

    /// Finished sessions, oldest first (in memory only; the caller persists it).
    pub fn session_log(&self) -> Vec<SessionRecord> {
        self.inner.state().log.clone()
    }
}
