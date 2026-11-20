// SPDX-License-Identifier: MIT
//! [`VaultHandle`]: one vault (one app + origin) and its session server.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use vault_container::Crk;
use vault_container::backend::ContainerBackend;
use vault_container::engine::HighWater;
use vault_container::replicate::{ReplicaId, ReplicaSet};
use vault_proto::PairOffer;
use vault_server::{
    ApprovalDecision, ApprovalRequest, Approver, SessionObserver, UnlockMaterial, VaultServer,
};
use zeroize::Zeroizing;

use crate::error::FfiError;
use crate::replica::{backend_for, result_of};
use crate::runtime::OwnedRuntime;
use crate::state::StateCell;
use crate::types::{
    ApprovalFfi, ApprovalRequestFfi, ApproverCallback, BindingInfo, NamedReplica, ReplicaResult,
    SessionInfo, SessionListener, SessionStateFfi, VaultStats,
};

/// Counts of approval CRK copies (test hook; always zero outside tests).
#[derive(Default)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SecretCounts {
    /// Copies alive now.
    pub live: std::sync::atomic::AtomicUsize,
    /// Copies ever made.
    pub total: std::sync::atomic::AtomicUsize,
}

/// A copy of secret bytes from Kotlin (the CRK of an approval), wiped on drop.
/// In tests it also counts the live copies, so a test can check that none
/// outlives the approval.
pub(crate) struct SecretBytes {
    bytes: Zeroizing<Vec<u8>>,
    #[cfg(test)]
    counts: Arc<SecretCounts>,
}

impl SecretBytes {
    fn new(bytes: Vec<u8>, _counts: &Arc<SecretCounts>) -> Self {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::SeqCst;
            _counts.live.fetch_add(1, SeqCst);
            _counts.total.fetch_add(1, SeqCst);
        }
        Self {
            bytes: Zeroizing::new(bytes),
            #[cfg(test)]
            counts: _counts.clone(),
        }
    }
}

#[cfg(test)]
impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.counts
            .live
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Session listeners by session id. An approval can be reported before
/// `serve_offer` has registered the listener: it is remembered until then.
#[derive(Default)]
struct Listeners {
    by_id: HashMap<u64, Arc<dyn SessionListener>>,
    approved_early: HashSet<u64>,
}

/// Shared between the handle, the approver bridge and the observer.
pub(crate) struct Shared {
    pub(crate) state: Arc<StateCell>,
    /// Answers the prompts: the approver of the latest `serve_offer`.
    approver: Mutex<Option<Arc<dyn ApproverCallback>>>,
    listeners: Mutex<Listeners>,
    /// The handle, for the unlocked-vault CRK check (set right after build;
    /// weak: the server owns this, so a strong reference would be a cycle
    /// keeping the open container alive).
    handle: std::sync::OnceLock<Weak<VaultHandle>>,
    /// [`SecretBytes`] counts (tests only).
    pub(crate) secrets: Arc<SecretCounts>,
}

/// Tell `l` about `state`. A listener error or panic is logged (without its
/// text, which comes from the app) and otherwise ignored: the session goes on.
fn notify(l: &Arc<dyn SessionListener>, id: u64, state: SessionStateFfi) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| l.on_state(id, state)));
    match r {
        Ok(Ok(())) => {}
        Ok(Err(_)) => tracing::warn!(session = id, "vault-ffi: session listener failed"),
        Err(_) => tracing::warn!(session = id, "vault-ffi: session listener panicked"),
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub(crate) fn new(state: Arc<StateCell>) -> Arc<Self> {
        Arc::new(Self {
            state,
            approver: Mutex::new(None),
            listeners: Mutex::new(Listeners::default()),
            handle: std::sync::OnceLock::new(),
            secrets: Arc::default(),
        })
    }

    fn emit(&self, id: u64, state: SessionStateFfi) {
        let l = locked(&self.listeners).by_id.get(&id).cloned();
        if let Some(l) = l {
            notify(&l, id, state);
        }
    }
}

/// [`Approver`] that forwards to the Kotlin [`ApproverCallback`].
pub(crate) struct Bridge(pub(crate) Arc<Shared>);

#[async_trait::async_trait]
impl Approver for Bridge {
    async fn approve(&self, req: &ApprovalRequest<'_>) -> ApprovalDecision {
        let Some(cb) = locked(&self.0.approver).clone() else {
            return ApprovalDecision::Deny;
        };
        let request = ApprovalRequestFfi {
            app_id: req.app_id.into(),
            origin: req.origin.into(),
            relay_host: req.relay_host.into(),
            device_name: req.device_name.into(),
            sas: req.sas.into(),
            mailbox_taken: req.mailbox_taken,
        };
        // In its own task, so that a panicking implementation cannot take the
        // session (or `serve_offer`) down with it; dropping this future (the
        // server's timeout) aborts the task and so cancels the Kotlin call.
        let counts = self.0.secrets.clone();
        let task = tokio::spawn(async move {
            match cb.approve(request).await {
                // Into zeroizing memory before anything else happens.
                Ok(ApprovalFfi::Approve { crk }) => Some(SecretBytes::new(crk, &counts)),
                Ok(ApprovalFfi::Deny) => None,
                Err(_) => {
                    // Kotlin threw (e.g. the activity died): a denial. The
                    // error text is not logged (it comes from the app).
                    tracing::warn!("vault-ffi: approver failed; denied");
                    None
                }
            }
        });
        let _abort = AbortOnDrop(task.abort_handle());
        let crk = match task.await {
            Ok(Some(crk)) => crk,
            Ok(None) => return ApprovalDecision::Deny,
            Err(_) => {
                tracing::warn!("vault-ffi: approver panicked; denied");
                return ApprovalDecision::Deny;
            }
        };
        if req.mailbox_taken {
            return ApprovalDecision::Deny;
        }
        let Ok(crk) = Crk::from_slice(&crk.bytes) else {
            return ApprovalDecision::Deny;
        };
        // An unlocked vault does not need the key, but an approval still
        // requires the unwrapped CRK: it must be this vault's.
        let handle = self.0.handle.get().and_then(Weak::upgrade);
        if let Some(handle) = handle
            && let Some(mk) = handle.server.manifest_key().await
            && !bool::from(crk.subkeys().manifest.ct_eq(&*mk))
        {
            return ApprovalDecision::Deny;
        }
        let high_water = self.0.state.get().high_water;
        ApprovalDecision::Approve(UnlockMaterial::Crk { crk, high_water })
    }
}

/// [`SessionObserver`] feeding new high-water marks to the persisting task
/// and approvals to the listeners.
pub(crate) struct Watch {
    shared: Arc<Shared>,
    marks: mpsc::UnboundedSender<HighWater>,
}

impl Watch {
    /// The observer, plus the task that saves every new mark in order.
    pub(crate) fn start(rt: &OwnedRuntime, shared: Arc<Shared>) -> Arc<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<HighWater>();
        let state = shared.state.clone();
        rt.handle().spawn(async move {
            while let Some(hw) = rx.recv().await {
                let state = state.clone();
                let saved = tokio::task::spawn_blocking(move || state.update(|s| s.raise(hw)))
                    .await
                    .unwrap_or_else(|_| Err(FfiError::internal("store task panicked")));
                if saved.is_err() {
                    tracing::warn!(seq = hw.seq, "vault-ffi: saving the high-water mark failed");
                }
            }
        });
        Arc::new(Self { shared, marks: tx })
    }
}

impl SessionObserver for Watch {
    fn approved(&self, session: u64) {
        let known = {
            let mut l = locked(&self.shared.listeners);
            let known = l.by_id.contains_key(&session);
            if !known {
                l.approved_early.insert(session);
            }
            known
        };
        if known {
            self.shared.emit(session, SessionStateFfi::Active);
        }
    }

    fn high_water(&self, hw: HighWater) {
        let _ = self.marks.send(hw);
    }
}

/// One vault: its container directory, session server and persisted state.
///
/// Locked or unlocked; it unlocks only through an approved session (whose
/// approver supplies the CRK) or at create/restore, and `lock()` drops the
/// container and every key.
#[derive(uniffi::Object)]
pub struct VaultHandle {
    pub(crate) rt: Arc<OwnedRuntime>,
    pub(crate) server: VaultServer,
    pub(crate) primary: Arc<dyn ContainerBackend>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) extra_ca_pem: Option<String>,
    /// One replication at a time.
    replicating: tokio::sync::Mutex<()>,
}

impl VaultHandle {
    pub(crate) fn new(
        rt: Arc<OwnedRuntime>,
        server: VaultServer,
        primary: Arc<dyn ContainerBackend>,
        shared: Arc<Shared>,
        extra_ca_pem: Option<String>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            rt,
            server,
            primary,
            shared,
            extra_ca_pem,
            replicating: tokio::sync::Mutex::new(()),
        });
        let _ = this.shared.handle.set(Arc::downgrade(&this));
        this
    }

    /// Approval CRK copies `(alive now, ever made)` (test hook).
    #[cfg(test)]
    pub(crate) fn secret_buffers(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::SeqCst;
        let c = &self.shared.secrets;
        (c.live.load(SeqCst), c.total.load(SeqCst))
    }
}

impl std::fmt::Debug for VaultHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultHandle")
            .field("binding", &self.server.binding())
            .field("locked", &self.server.is_locked())
            .finish_non_exhaustive()
    }
}

impl Drop for VaultHandle {
    /// A released handle locks its vault: sessions end and the container and
    /// its keys are dropped (sessions would otherwise keep the server alive).
    fn drop(&mut self) {
        let server = self.server.clone();
        self.rt.handle().spawn(async move { server.lock().await });
    }
}

#[uniffi::export]
impl VaultHandle {
    /// The app binding (from the container header while locked).
    pub fn binding(&self) -> BindingInfo {
        let (app_id, origin) = self.server.binding();
        BindingInfo { app_id, origin }
    }

    pub fn is_locked(&self) -> bool {
        self.server.is_locked()
    }

    /// The approved session, if any.
    pub fn active_session(&self) -> Option<u64> {
        self.server.active_session()
    }

    pub async fn stats(self: Arc<Self>) -> Result<VaultStats, FfiError> {
        let this = self.clone();
        self.rt
            .run(async move {
                let st = this.server.container_stats().await;
                let hw = this.server.high_water().await;
                let persisted = this.shared.state.get();
                Ok(VaultStats {
                    locked: this.server.is_locked(),
                    seq: hw.or(persisted.high_water).map(|h| h.seq),
                    segments: st.map(|s| s.segments as u64),
                    stored_bytes: st.map(|s| s.stored_bytes),
                    live_bytes: st.map(|s| s.live_bytes),
                    active_session: this.server.active_session(),
                    last_replication_ms: persisted
                        .replicas
                        .replicas
                        .values()
                        .filter_map(|r| r.last_ok_at_ms)
                        .max(),
                })
            })
            .await
    }

    /// Compact the container now (the user's "compact now"). Needs an unlocked
    /// vault without an active session. Returns the new manifest seq.
    pub async fn compact(self: Arc<Self>) -> Result<u64, FfiError> {
        let this = self.clone();
        self.rt
            .run(async move {
                if this.server.is_locked() {
                    return Err(FfiError::locked());
                }
                match this.server.compact_now().await? {
                    Some(seq) => Ok(seq),
                    None if this.server.is_locked() => Err(FfiError::locked()),
                    None => Err(FfiError::busy("a session is active")),
                }
            })
            .await
    }

    /// End every session, discard uncommitted writes and drop the container
    /// with its keys. The vault keeps pairing (locked): the next approval
    /// must bring the CRK again.
    pub async fn lock(self: Arc<Self>) {
        let this = self.clone();
        let _ = self
            .rt
            .run(async move {
                this.server.lock().await;
                Ok(())
            })
            .await;
    }

    /// Dial the relay named in the scanned pairing code `qr` and run the
    /// pairing handshake; returns once the 6-digit code is known. The session
    /// then continues in the background: `approver` is asked (after the
    /// browser's request, for one prompt at a time; it also answers later
    /// prompts of this vault until the next `serve_offer`) and `listener`
    /// hears `Active` and `Ended`.
    ///
    /// Fails with `MailboxTaken` after warning `approver` if the relay mailbox
    /// was already taken; nothing is approved then.
    pub async fn serve_offer(
        self: Arc<Self>,
        qr: String,
        approver: Arc<dyn ApproverCallback>,
        listener: Arc<dyn SessionListener>,
    ) -> Result<SessionInfo, FfiError> {
        let this = self.clone();
        self.rt
            .run(async move {
                let offer = PairOffer::from_qr(qr.trim())?;
                *locked(&this.shared.approver) = Some(approver);
                let h = this.server.serve_offer(offer).await?;
                let id = h.id();
                let early = {
                    let mut l = locked(&this.shared.listeners);
                    l.by_id.insert(id, listener.clone());
                    l.approved_early.remove(&id)
                };
                if early {
                    notify(&listener, id, SessionStateFfi::Active);
                }
                let info = SessionInfo {
                    session_id: id,
                    sas: h.sas().to_string(),
                };
                let shared = this.shared.clone();
                tokio::spawn(async move {
                    let reason = h.finished().await;
                    let l = locked(&shared.listeners).by_id.remove(&id);
                    if let Some(l) = l {
                        notify(
                            &l,
                            id,
                            SessionStateFfi::Ended {
                                reason: reason.into(),
                            },
                        );
                    }
                });
                Ok(info)
            })
            .await
    }

    /// Secret export: the manifest key, for sealing under K_bg (background
    /// replication). Only while unlocked (`Locked` otherwise). With it alone
    /// a holder can copy the sealed container to replicas but neither read
    /// vault data nor forge a manifest. Seal it and wipe the `ByteArray`.
    pub async fn export_manifest_key(self: Arc<Self>) -> Result<Vec<u8>, FfiError> {
        let this = self.clone();
        self.rt
            .run(async move {
                match this.server.manifest_key().await {
                    Some(k) => Ok(k.to_vec()),
                    None => Err(FfiError::locked()),
                }
            })
            .await
    }

    /// Copy the sealed container to `targets`, locked or unlocked, with only
    /// the manifest key (from K_bg). Each replica is synced independently;
    /// per-replica anti-rollback marks are kept in the saved state. Refused
    /// (`Busy`) while a session is active or another replication runs;
    /// `WrongKey` if `manifest_key` does not open this vault's manifest;
    /// `RollbackDetected` (nothing pushed) if the local container is older
    /// than, or forks from, the saved high-water mark.
    pub async fn replicate(
        self: Arc<Self>,
        manifest_key: Vec<u8>,
        targets: Vec<NamedReplica>,
    ) -> Result<Vec<ReplicaResult>, FfiError> {
        let mk = Zeroizing::new(manifest_key);
        let this = self.clone();
        self.rt
            .run(async move {
                let mut key = Zeroizing::new([0u8; 32]);
                if mk.len() != 32 {
                    return Err(FfiError::invalid("the manifest key has 32 bytes"));
                }
                key.copy_from_slice(&mk);
                drop(mk);
                let _one = this
                    .replicating
                    .try_lock()
                    .map_err(|_| FfiError::busy("a replication is already running"))?;
                if this.server.active_session().is_some() {
                    return Err(FfiError::busy("a session is active"));
                }
                // The key must open this vault's manifest (fail early and
                // clearly rather than once per replica).
                let Some((sealed, _)) = this.primary.get("manifest").await? else {
                    return Err(FfiError::storage("the vault has no manifest"));
                };
                let primary = vault_container::manifest::open(&sealed, &key)?;
                // The anti-rollback floor: a primary older than, or forked
                // from, the saved mark was rolled back; push nothing. (A
                // primary ahead of the mark is normal: the mark trails
                // commits. The floor is not raised here: the manifest key
                // alone cannot authenticate a manifest.)
                let Some(floor) = this.shared.state.get().high_water else {
                    return Err(FfiError::HighWaterMissing {
                        detail: "no saved high-water mark for this vault".into(),
                    });
                };
                let hash = vault_container::manifest::hash(&sealed);
                if primary.seq < floor.seq
                    || (primary.seq == floor.seq && !bool::from(hash.ct_eq(&floor.hash)))
                {
                    return Err(FfiError::RollbackDetected {
                        detail: format!(
                            "the vault is at seq {}, the saved mark at seq {}",
                            primary.seq, floor.seq
                        ),
                    });
                }
                let mut replicas = Vec::with_capacity(targets.len());
                for t in targets {
                    if t.id.is_empty() {
                        return Err(FfiError::invalid("empty replica id"));
                    }
                    let b = backend_for(t.target, this.extra_ca_pem.as_deref()).await?;
                    replicas.push((ReplicaId(t.id), b));
                }
                let state = this.shared.state.get().replicas;
                let mut set = ReplicaSet::new(this.primary.clone(), replicas, state);
                let cell = this.shared.state.clone();
                let statuses = set
                    .replicate_once(&key, |st| {
                        cell.update(|p| p.replicas = st.clone())
                            .map_err(|e| vault_container::Error::Io(e.to_string()))
                    })
                    .await;
                let final_state = set.state().clone();
                cell.update(|p| p.replicas = final_state)?;
                Ok(statuses.into_iter().map(result_of).collect())
            })
            .await
    }
}
