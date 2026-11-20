// SPDX-License-Identifier: MIT
//! Host tests: the FFI surface driven from Rust (standing in for Kotlin),
//! against real container directories, an in-process relay and a browser
//! stand-in (`vault-client` native).

use std::cell::RefCell;
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use vault_client::native::{TokioClock, TokioFactory};
use vault_client::{CloseReason, SessionCallbacks, SessionState, SystemRng, VaultSession};
use vault_container::backend::LocalBackend;
use vault_container::engine::{Container, HighWater};
use vault_container::{Crk, RecoveryKey};
use vault_server::ServerError;

use crate::state::Persisted;
use crate::*;

const APP: &str = "test-app";
const ORIGIN: &str = "https://app.example";
const T: Duration = Duration::from_secs(10);

// ---------- Kotlin stand-ins ----------

#[derive(Default)]
struct MemStore {
    blob: Mutex<Option<Vec<u8>>>,
    saves: AtomicUsize,
}

impl HighWaterStore for MemStore {
    fn load(&self) -> Result<Option<Vec<u8>>, FfiError> {
        Ok(self.blob.lock().unwrap().clone())
    }
    fn save(&self, state: Vec<u8>) -> Result<(), FfiError> {
        *self.blob.lock().unwrap() = Some(state);
        self.saves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl MemStore {
    fn state(&self) -> Persisted {
        Persisted::decode(self.blob.lock().unwrap().as_ref().expect("saved")).unwrap()
    }
    fn seq(&self) -> Option<u64> {
        self.blob
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| Persisted::decode(b).ok())
            .and_then(|p| p.high_water)
            .map(|h| h.seq)
    }
}

fn store() -> Arc<MemStore> {
    Arc::new(MemStore::default())
}

enum Mode {
    Approve(Vec<u8>),
    Deny,
    Hang,
}

struct TestApprover {
    mode: Mutex<Mode>,
    requests: Mutex<Vec<ApprovalRequestFfi>>,
}

impl TestApprover {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(mode),
            requests: Mutex::default(),
        })
    }
    fn requests(&self) -> Vec<ApprovalRequestFfi> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ApproverCallback for TestApprover {
    async fn approve(&self, request: ApprovalRequestFfi) -> ApprovalFfi {
        self.requests.lock().unwrap().push(request);
        let answer = match &*self.mode.lock().unwrap() {
            Mode::Approve(crk) => Some(ApprovalFfi::Approve { crk: crk.clone() }),
            Mode::Deny => Some(ApprovalFfi::Deny),
            Mode::Hang => None,
        };
        match answer {
            Some(a) => a,
            None => std::future::pending().await,
        }
    }
}

#[derive(Default)]
struct TestListener {
    events: Mutex<Vec<(u64, SessionStateFfi)>>,
}

impl SessionListener for TestListener {
    fn on_state(&self, session_id: u64, state: SessionStateFfi) {
        self.events.lock().unwrap().push((session_id, state));
    }
}

impl TestListener {
    fn events(&self) -> Vec<(u64, SessionStateFfi)> {
        self.events.lock().unwrap().clone()
    }
    fn ended(&self) -> Option<EndReasonFfi> {
        self.events().into_iter().find_map(|(_, s)| match s {
            SessionStateFfi::Ended { reason } => Some(reason),
            _ => None,
        })
    }
}

// ---------- helpers ----------

fn config() -> FactoryConfig {
    FactoryConfig {
        extra_ca_pem: None,
        approval_timeout_secs: 10,
        idle_timeout_secs: None,
    }
}

fn factory() -> Arc<VaultFactory> {
    VaultFactory::new(config()).unwrap()
}

fn path_str(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

async fn create(f: &VaultFactory, dir: &Path, st: &Arc<MemStore>) -> CreatedVault {
    f.create(path_str(dir), APP.into(), ORIGIN.into(), st.clone())
        .await
        .unwrap()
}

/// Wait (real time) until `cond` holds; panics after `T`.
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(start.elapsed() < T, "timeout: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Write `k = v` straight into the container in `dir` and commit.
async fn commit_direct(dir: &Path, crk: &[u8], k: &[u8], v: &[u8]) -> HighWater {
    let b = LocalBackend::open(dir).await.unwrap();
    let mut c = Container::open_with_crk(b, Crk::from_slice(crk).unwrap(), None)
        .await
        .unwrap();
    c.put(k, v).unwrap();
    c.commit().await.unwrap();
    c.high_water()
}

struct Relay {
    addr: SocketAddr,
    mailboxes: vault_mailbox::Mailboxes,
    _stop: oneshot::Sender<()>,
}

async fn relay_with(tls: Option<Arc<rustls::ServerConfig>>) -> Relay {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let mailboxes = vault_mailbox::Mailboxes::new(vault_mailbox::MailboxConfig::default());
    tokio::spawn(vault_mailbox::serve_tls(
        l,
        mailboxes.clone(),
        tls,
        async move {
            let _ = rx.await;
        },
    ));
    Relay {
        addr,
        mailboxes,
        _stop: tx,
    }
}

async fn relay() -> Relay {
    relay_with(None).await
}

/// What the browser stand-in reported.
#[derive(Clone, Default)]
struct Seen {
    states: Rc<RefCell<Vec<SessionState>>>,
    sas: Rc<RefCell<Option<String>>>,
}

impl Seen {
    fn has(&self, s: &SessionState) -> bool {
        self.states.borrow().contains(s)
    }
    fn closed(&self) -> Option<CloseReason> {
        self.states.borrow().iter().find_map(|s| match s {
            SessionState::Closed(r) => Some(r.clone()),
            _ => None,
        })
    }
}

/// The browser stand-in pairs through `relay` and the vault scans its QR via
/// `handle.serve_offer`. Must run inside a `LocalSet`.
async fn pair(
    relay: &Relay,
    handle: &Arc<VaultHandle>,
    approver: Arc<TestApprover>,
    listener: Arc<TestListener>,
) -> (VaultSession, Seen, Result<SessionInfo, FfiError>) {
    let seen = Seen::default();
    let (qr_tx, mut qr_rx) = mpsc::unbounded_channel::<String>();
    let cb = {
        let states = seen.states.clone();
        let sas = seen.sas.clone();
        SessionCallbacks::new(
            Rc::new(move |qr: &str| {
                let _ = qr_tx.send(qr.to_string());
            }),
            Rc::new(move |s: &str| *sas.borrow_mut() = Some(s.to_string())),
            Rc::new(move |s| states.borrow_mut().push(s)),
        )
    };
    let (session, fut) = VaultSession::pair(
        TokioFactory,
        TokioClock,
        &SystemRng,
        &format!("http://{}", relay.addr),
        APP,
        ORIGIN,
        "Test Browser",
        cb,
    )
    .unwrap();
    tokio::task::spawn_local(fut);
    let qr = tokio::time::timeout(T, qr_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let info = handle.clone().serve_offer(qr, approver, listener).await;
    (session, seen, info)
}

/// The user clicks "Codes match" in the browser once the code is shown.
async fn confirm_and_wait_active(session: &VaultSession, seen: &Seen) {
    let s = seen.clone();
    eventually("sas shown", move || s.sas.borrow().is_some()).await;
    session.confirm_sas().unwrap();
    let s = seen.clone();
    eventually("active", move || s.has(&SessionState::Active)).await;
}

async fn wait_closed(seen: &Seen) -> CloseReason {
    let s = seen.clone();
    eventually("browser closed", move || s.closed().is_some()).await;
    seen.closed().unwrap()
}

async fn wait_ended(l: &TestListener) -> EndReasonFfi {
    eventually("session ended", || l.ended().is_some()).await;
    l.ended().unwrap()
}

// ---------- tests ----------

#[tokio::test(flavor = "multi_thread")]
async fn create_open_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("v");
    let f = factory();
    let st = store();
    let c = create(&f, &dir, &st).await;
    assert_eq!(c.crk.len(), 32);
    assert_eq!(c.manifest_key.len(), 32);
    let rk = RecoveryKey::parse(&c.recovery_key).unwrap();
    // The exported keys belong together and to this container.
    let crk = Crk::from_slice(&c.crk).unwrap();
    assert_eq!(crk.subkeys().manifest[..], c.manifest_key[..]);
    let b = LocalBackend::open(&dir).await.unwrap();
    let (_, crk2) = Container::open_with_recovery(b, &rk, None).await.unwrap();
    assert_eq!(crk2.to_bytes()[..], c.crk[..]);

    let h = c.handle;
    assert!(!h.is_locked());
    assert_eq!(
        h.binding(),
        BindingInfo {
            app_id: APP.into(),
            origin: ORIGIN.into()
        }
    );
    let s = h.clone().stats().await.unwrap();
    assert_eq!((s.locked, s.seq, s.segments), (false, Some(0), Some(0)));
    assert_eq!(
        st.seq(),
        Some(0),
        "initial mark saved before create returns"
    );
    drop(h);

    let h = f.open(path_str(&dir), st.clone()).await.unwrap();
    assert!(h.is_locked());
    assert_eq!(h.binding().app_id, APP);
    let s = h.clone().stats().await.unwrap();
    assert_eq!((s.locked, s.seq, s.segments), (true, Some(0), None));

    // Create refuses a non-empty directory and leaves an invalid one uncreated.
    let again = f
        .create(path_str(&dir), APP.into(), ORIGIN.into(), store())
        .await;
    assert!(
        matches!(again, Err(FfiError::AlreadyExists { .. })),
        "{again:?}"
    );
    let bad = tmp.path().join("bad");
    let r = f
        .create(path_str(&bad), "Bad App".into(), ORIGIN.into(), store())
        .await;
    assert!(matches!(r, Err(FfiError::InvalidInput { .. })), "{r:?}");
    assert!(!bad.exists());
    // Opening looks without creating.
    let missing = tmp.path().join("missing");
    let r = f.open(path_str(&missing), store()).await;
    assert!(matches!(r, Err(FfiError::NotFound { .. })), "{r:?}");
    assert!(!missing.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_and_approve_via_callback() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("v");
    let f = factory();
    let st = store();
    let c = create(&f, &dir, &st).await;
    let crk = c.crk.clone();
    let h = c.handle;
    h.clone().lock().await;
    assert!(h.is_locked());
    let relay = relay().await;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let approver = TestApprover::new(Mode::Approve(crk.clone()));
            let listener = Arc::new(TestListener::default());
            let (session, seen, info) = pair(&relay, &h, approver.clone(), listener.clone()).await;
            let info = info.unwrap();
            confirm_and_wait_active(&session, &seen).await;
            assert_eq!(seen.sas.borrow().as_deref(), Some(info.sas.as_str()));
            let reqs = approver.requests();
            assert_eq!(
                reqs,
                vec![ApprovalRequestFfi {
                    app_id: APP.into(),
                    origin: ORIGIN.into(),
                    relay_host: relay.addr.to_string(),
                    device_name: "Test Browser".into(),
                    sas: info.sas.clone(),
                    mailbox_taken: false,
                }]
            );
            eventually("listener active", || {
                listener.events() == vec![(info.session_id, SessionStateFfi::Active)]
            })
            .await;
            assert!(!h.is_locked(), "the approval unlocked the vault");
            assert_eq!(h.active_session(), Some(info.session_id));

            let b = session.backend().unwrap();
            b.put(b"k", b"v").await.unwrap();
            b.flush_commit().await.unwrap();
            eventually("high-water saved", || st.seq() == Some(1)).await;
            assert_eq!(h.clone().stats().await.unwrap().seq, Some(1));

            session.lock().await;
            assert_eq!(wait_ended(&listener).await, EndReasonFfi::Closed);
        })
        .await;

    // Lock, reopen from disk and read back through a new approved session.
    h.clone().lock().await;
    drop(h);
    let h = f.open(path_str(&dir), st.clone()).await.unwrap();
    assert!(h.is_locked());
    local
        .run_until(async {
            let approver = TestApprover::new(Mode::Approve(crk.clone()));
            let listener = Arc::new(TestListener::default());
            let (session, seen, info) = pair(&relay, &h, approver, listener).await;
            info.unwrap();
            confirm_and_wait_active(&session, &seen).await;
            let b = session.backend().unwrap();
            assert_eq!(b.get(b"k").await.unwrap().as_deref(), Some(&b"v"[..]));
            session.lock().await;
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_requires_crk_unwrap() {
    let tmp = tempfile::tempdir().unwrap();
    let f = factory();
    let st = store();
    let c = create(&f, &tmp.path().join("v"), &st).await;
    let h = c.handle;
    let relay = relay().await;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Unlocked vault: an approval with another key is a denial.
            for wrong in [vec![7u8; 32], vec![1u8; 31], Vec::new()] {
                let listener = Arc::new(TestListener::default());
                let (session, seen, info) = pair(
                    &relay,
                    &h,
                    TestApprover::new(Mode::Approve(wrong)),
                    listener.clone(),
                )
                .await;
                info.unwrap();
                session.confirm_sas().ok();
                assert_eq!(wait_ended(&listener).await, EndReasonFfi::Denied);
                assert_eq!(wait_closed(&seen).await, CloseReason::Denied);
                assert!(!seen.has(&SessionState::Active));
            }
            // Locked vault: the wrong key does not open it.
            h.clone().lock().await;
            let listener = Arc::new(TestListener::default());
            let (_session, seen, info) = pair(
                &relay,
                &h,
                TestApprover::new(Mode::Approve(vec![7u8; 32])),
                listener.clone(),
            )
            .await;
            info.unwrap();
            assert_eq!(wait_ended(&listener).await, EndReasonFfi::UnlockFailed);
            assert_eq!(wait_closed(&seen).await, CloseReason::Denied);
            assert!(h.is_locked());
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approver_deny_and_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let f = VaultFactory::new(FactoryConfig {
        approval_timeout_secs: 1,
        ..config()
    })
    .unwrap();
    let c = create(&f, &tmp.path().join("v"), &store()).await;
    let h = c.handle;
    h.clone().lock().await;
    let relay = relay().await;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let listener = Arc::new(TestListener::default());
            let (_s, seen, info) =
                pair(&relay, &h, TestApprover::new(Mode::Deny), listener.clone()).await;
            info.unwrap();
            assert_eq!(wait_ended(&listener).await, EndReasonFfi::Denied);
            assert_eq!(wait_closed(&seen).await, CloseReason::Denied);

            let listener = Arc::new(TestListener::default());
            let approver = TestApprover::new(Mode::Hang);
            let (_s, _seen, info) = pair(&relay, &h, approver.clone(), listener.clone()).await;
            info.unwrap();
            assert_eq!(wait_ended(&listener).await, EndReasonFfi::ApprovalTimeout);
            assert_eq!(approver.requests().len(), 1);
            assert!(h.is_locked());
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mailbox_taken_forces_deny() {
    use futures::StreamExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let f = factory();
    let c = create(&f, &tmp.path().join("v"), &store()).await;
    let crk = c.crk.clone();
    let h = c.handle;
    h.clone().lock().await;
    let relay = relay().await;
    let keys = vault_proto::noise::PairKeys::generate(
        format!("http://{}", relay.addr),
        [5; 16],
        APP.into(),
        ORIGIN.into(),
    )
    .unwrap();
    let qr = keys.offer().to_qr().unwrap();
    // Two parties already hold the mailbox.
    let url = format!("ws://{}/mailbox/{}", relay.addr, "05".repeat(16));
    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let approver = TestApprover::new(Mode::Approve(crk));
    let listener = Arc::new(TestListener::default());
    let r = h
        .clone()
        .serve_offer(qr, approver.clone(), listener.clone())
        .await;
    assert!(matches!(r, Err(FfiError::MailboxTaken { .. })), "{r:?}");
    let reqs = approver.requests();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].mailbox_taken);
    assert_eq!(
        (reqs[0].sas.as_str(), reqs[0].device_name.as_str()),
        ("", "")
    );
    assert!(
        h.is_locked(),
        "an approval of a taken mailbox unlocks nothing"
    );
    assert_eq!(h.secret_buffers(), (0, 1), "the CRK copy was dropped");
    assert!(listener.events().is_empty());
    let _ = (a.next(), b.next());
    drop((a, b));
}

#[tokio::test(flavor = "multi_thread")]
async fn crk_zeroized_after_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let f = factory();
    let c = create(&f, &tmp.path().join("v"), &store()).await;
    let crk = c.crk.clone();
    let h = c.handle;
    h.clone().lock().await;
    let relay = relay().await;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let listener = Arc::new(TestListener::default());
            let (session, seen, info) = pair(
                &relay,
                &h,
                TestApprover::new(Mode::Approve(crk.clone())),
                listener.clone(),
            )
            .await;
            info.unwrap();
            confirm_and_wait_active(&session, &seen).await;
            assert!(!h.is_locked());
            // The copy Rust made of the Kotlin bytes (a `Zeroizing` buffer,
            // wiped on drop) is gone once the session is active.
            assert_eq!(h.secret_buffers(), (0, 1));
            session.lock().await;
            wait_ended(&listener).await;
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn manifest_key_export_for_background() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("v");
    let f = factory();
    let st = store();
    let c = create(&f, &dir, &st).await;
    let h = c.handle;
    assert_eq!(
        h.clone().export_manifest_key().await.unwrap(),
        c.manifest_key
    );
    h.clone().lock().await;
    let r = h.clone().export_manifest_key().await;
    assert!(matches!(r, Err(FfiError::Locked { .. })), "{r:?}");
    drop(h);
    let h = f.open(path_str(&dir), st).await.unwrap();
    let r = h.clone().export_manifest_key().await;
    assert!(matches!(r, Err(FfiError::Locked { .. })), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn replicate_with_manifest_key_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("v");
    let rep = tmp.path().join("replica");
    let f = factory();
    let st = store();
    let c = create(&f, &dir, &st).await;
    let mk = c.manifest_key.clone();
    drop(c.handle);
    commit_direct(&dir, &c.crk, b"k", b"v").await;

    // Background job: a locked handle and only the manifest key.
    let h = f.open(path_str(&dir), st.clone()).await.unwrap();
    assert!(h.is_locked());
    let target = || NamedReplica {
        id: "r1".into(),
        target: ReplicaTarget::Local {
            path: path_str(&rep),
        },
    };
    let res = h
        .clone()
        .replicate(mk.clone(), vec![target()])
        .await
        .unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, "r1");
    assert_eq!(
        res[0].outcome,
        ReplicaOutcomeFfi::Synced {
            seq: 1,
            uploaded: 1,
            deleted: 0
        }
    );
    assert!(rep.join("header").exists() && rep.join("manifest").exists());
    let saved = st.state();
    let mark = saved.replicas.replicas[&vault_container::replicate::ReplicaId("r1".into())]
        .last_seen
        .unwrap();
    assert_eq!(mark.seq, 1);
    assert!(
        h.clone()
            .stats()
            .await
            .unwrap()
            .last_replication_ms
            .is_some()
    );
    assert!(h.is_locked(), "replication does not unlock");

    // Idempotent.
    let res = h
        .clone()
        .replicate(mk.clone(), vec![target()])
        .await
        .unwrap();
    assert_eq!(
        res[0].outcome,
        ReplicaOutcomeFfi::Synced {
            seq: 1,
            uploaded: 0,
            deleted: 0
        }
    );
    // A wrong key or a malformed one is refused before any replica is touched.
    let r = h.clone().replicate(vec![9; 32], vec![target()]).await;
    assert!(matches!(r, Err(FfiError::WrongKey { .. })), "{r:?}");
    let r = h.clone().replicate(mk[..31].to_vec(), vec![target()]).await;
    assert!(matches!(r, Err(FfiError::InvalidInput { .. })), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_from_local_replica() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("v");
    let rep = tmp.path().join("replica");
    let f = factory();
    let c = create(&f, &dir, &store()).await;
    drop(c.handle);
    commit_direct(&dir, &c.crk, b"k", b"v").await;
    let h = f.open(path_str(&dir), store()).await.unwrap();
    let source = || ReplicaTarget::Local {
        path: path_str(&rep),
    };
    h.clone()
        .replicate(
            c.manifest_key.clone(),
            vec![NamedReplica {
                id: "r1".into(),
                target: source(),
            }],
        )
        .await
        .unwrap();

    let phone2 = tmp.path().join("phone2");
    std::fs::create_dir(&phone2).unwrap();
    let new_dir = phone2.join("v");
    let entries = || std::fs::read_dir(&phone2).unwrap().count();

    // Failures leave nothing behind.
    let other = RecoveryKey::generate().to_display().to_string();
    let r = f
        .restore(path_str(&new_dir), source(), "r1".into(), other, store())
        .await;
    assert!(matches!(r, Err(FfiError::WrongKey { .. })), "{r:?}");
    let mut typo = c.recovery_key.clone();
    typo.replace_range(0..1, if typo.starts_with('A') { "B" } else { "A" });
    let r = f
        .restore(path_str(&new_dir), source(), "r1".into(), typo, store())
        .await;
    assert!(matches!(r, Err(FfiError::RecoveryKeyTypo { .. })), "{r:?}");
    assert_eq!(entries(), 0, "no staging or target directory left");

    let st = store();
    let r = f
        .restore(
            path_str(&new_dir),
            source(),
            "r1".into(),
            c.recovery_key.clone(),
            st.clone(),
        )
        .await
        .unwrap();
    assert_eq!(r.seq, 1);
    assert_eq!(entries(), 1, "only the vault directory");
    let h2 = r.handle;
    assert!(!h2.is_locked());
    assert_eq!(h2.binding().origin, ORIGIN);
    assert_eq!(h2.clone().stats().await.unwrap().seq, Some(1));
    assert_eq!(st.seq(), Some(1), "restored mark saved");
    assert!(
        st.state()
            .replicas
            .replicas
            .contains_key(&vault_container::replicate::ReplicaId("r1".into())),
        "the source is a known replica"
    );
    // The keys for the new phone's Keystore: the same vault.
    assert_eq!(
        h2.clone().export_manifest_key().await.unwrap(),
        c.manifest_key
    );
    let crk = f
        .rewrap_with_recovery_key(path_str(&new_dir), c.recovery_key.clone(), st.clone())
        .await
        .unwrap();
    assert_eq!(crk, c.crk);
    // The data came along.
    drop(h2);
    let b = LocalBackend::open(&new_dir).await.unwrap();
    let rc = Container::open_with_crk(b, Crk::from_slice(&crk).unwrap(), None)
        .await
        .unwrap();
    assert_eq!(rc.get(b"k").as_deref(), Some(&b"v"[..]));

    // A non-empty target is refused.
    let r = f
        .restore(
            path_str(&new_dir),
            source(),
            "r1".into(),
            c.recovery_key,
            store(),
        )
        .await;
    assert!(matches!(r, Err(FfiError::AlreadyExists { .. })), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rewrap_with_recovery_key() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("v");
    let f = factory();
    let st = store();
    let c = create(&f, &dir, &st).await;
    drop(c.handle);
    let hw = commit_direct(&dir, &c.crk, b"k", b"v").await;

    let crk = f
        .rewrap_with_recovery_key(path_str(&dir), c.recovery_key.clone(), st.clone())
        .await
        .unwrap();
    assert_eq!(crk, c.crk, "a fresh copy of the same CRK");
    assert_eq!(
        st.state().high_water,
        Some(hw),
        "mark raised to the opened manifest"
    );

    // The saved floor is enforced: a mark ahead of the container is a rollback.
    let ahead = store();
    ahead
        .save(
            Persisted {
                high_water: Some(HighWater {
                    seq: 99,
                    hash: [0; 32],
                }),
                ..Persisted::default()
            }
            .encode()
            .unwrap(),
        )
        .unwrap();
    let r = f
        .rewrap_with_recovery_key(path_str(&dir), c.recovery_key.clone(), ahead)
        .await;
    assert!(matches!(r, Err(FfiError::Rollback { .. })), "{r:?}");
    let r = f
        .rewrap_with_recovery_key(path_str(&dir), "not a key".into(), st.clone())
        .await;
    assert!(r.is_err());
    let r = f
        .rewrap_with_recovery_key(path_str(&tmp.path().join("none")), c.recovery_key, st)
        .await;
    assert!(matches!(r, Err(FfiError::NotFound { .. })), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_map_to_ffi_enum() {
    use vault_container::Error as E;
    fn kind(e: &FfiError) -> &'static str {
        match e {
            FfiError::InvalidInput { .. } => "InvalidInput",
            FfiError::NotFound { .. } => "NotFound",
            FfiError::AlreadyExists { .. } => "AlreadyExists",
            FfiError::Locked { .. } => "Locked",
            FfiError::Busy { .. } => "Busy",
            FfiError::RecoveryKeyTypo { .. } => "RecoveryKeyTypo",
            FfiError::WrongKey { .. } => "WrongKey",
            FfiError::Rollback { .. } => "Rollback",
            FfiError::Diverged { .. } => "Diverged",
            FfiError::BadOffer { .. } => "BadOffer",
            FfiError::Relay { .. } => "Relay",
            FfiError::Tls { .. } => "Tls",
            FfiError::MailboxTaken { .. } => "MailboxTaken",
            FfiError::Timeout { .. } => "Timeout",
            FfiError::Storage { .. } => "Storage",
            FfiError::Unsupported { .. } => "Unsupported",
            FfiError::Callback { .. } => "Callback",
            FfiError::Internal { .. } => "Internal",
        }
    }
    let cases: Vec<(FfiError, &str)> = vec![
        (E::RecoveryKeyTypo.into(), "RecoveryKeyTypo"),
        (E::Crypto.into(), "WrongKey"),
        (E::BadSecrets.into(), "WrongKey"),
        (E::Rollback.into(), "Rollback"),
        (E::DivergedReplicas(vec!["a".into()]).into(), "Diverged"),
        (E::InvalidBinding("x".into()).into(), "InvalidInput"),
        (E::Conflict.into(), "AlreadyExists"),
        (E::PreV1Container.into(), "Unsupported"),
        (E::UnsupportedFormatVersion(9).into(), "Unsupported"),
        (E::IncompleteCreate.into(), "Storage"),
        (E::Io("disk".into()).into(), "Storage"),
        (E::Format("bad".into()).into(), "Storage"),
        (ServerError::MailboxTaken.into(), "MailboxTaken"),
        (ServerError::BadOffer.into(), "BadOffer"),
        (ServerError::Relay("connect failed".into()).into(), "Relay"),
        (ServerError::Handshake.into(), "Relay"),
        (ServerError::Tls("bad pem").into(), "Tls"),
        (ServerError::Timeout.into(), "Timeout"),
        (ServerError::Rng.into(), "Internal"),
        (ServerError::Container(E::Rollback).into(), "Rollback"),
        (
            uniffi::UnexpectedUniFFICallbackError::new("kotlin threw").into(),
            "Callback",
        ),
    ];
    for (e, want) in &cases {
        assert_eq!(kind(e), *want, "{e:?}");
    }

    // End to end, and no secret in any message.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("v");
    let f = factory();
    let st = store();
    let c = create(&f, &dir, &st).await;
    let h = c.handle;
    let r = f.parse_offer("vault-pair:v1:@@@".into());
    assert!(matches!(r, Err(FfiError::BadOffer { .. })), "{r:?}");
    let r = f.parse_offer("https://example.com".into());
    assert!(matches!(r, Err(FfiError::BadOffer { .. })), "{r:?}");
    let r = h
        .clone()
        .serve_offer(
            "nope".into(),
            TestApprover::new(Mode::Deny),
            Arc::new(TestListener::default()),
        )
        .await;
    assert!(matches!(r, Err(FfiError::BadOffer { .. })), "{r:?}");
    let other = RecoveryKey::generate().to_display().to_string();
    let r = f
        .rewrap_with_recovery_key(path_str(&dir), other.clone(), st.clone())
        .await;
    let e = r.unwrap_err();
    assert!(matches!(e, FfiError::WrongKey { .. }), "{e:?}");
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains(&other[..8]) && !shown.contains(&c.recovery_key[..8]));
    let secret = "s3cr3t-pa55w0rd";
    let r = h
        .clone()
        .replicate(
            c.manifest_key.clone(),
            vec![NamedReplica {
                id: "dav".into(),
                target: ReplicaTarget::WebDav {
                    url: "ftp://example.com/x".into(),
                    user: "u".into(),
                    password: secret.into(),
                },
            }],
        )
        .await;
    let e = r.unwrap_err();
    assert!(
        matches!(e, FfiError::InvalidInput { .. } | FfiError::Storage { .. }),
        "{e:?}"
    );
    assert!(!format!("{e} {e:?}").contains(secret));
    h.clone().lock().await;
    let r = h.clone().compact().await;
    assert!(matches!(r, Err(FfiError::Locked { .. })), "{r:?}");
    let r = h.clone().export_manifest_key().await;
    assert!(matches!(r, Err(FfiError::Locked { .. })), "{r:?}");
}

/// A CA and a leaf for 127.0.0.1 (in-test only): `(ca_pem, leaf_pem, key_pem)`.
fn local_pki() -> (String, String, String) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.subject_alt_names
        .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
    let leaf = p
        .signed_by(&leaf_key, &Issuer::from_params(&ca_params, &ca_key))
        .unwrap();
    (ca.pem(), leaf.pem(), leaf_key.serialize_pem())
}

#[tokio::test(flavor = "multi_thread")]
async fn extra_ca_only_when_set() {
    // A PEM that holds no certificate is refused up front.
    let r = VaultFactory::new(FactoryConfig {
        extra_ca_pem: Some("not a pem".into()),
        ..config()
    });
    assert!(matches!(r, Err(FfiError::Tls { .. })), "{r:?}");

    let (ca, leaf, key) = local_pki();
    let tls = vault_mailbox::tls_server_config(leaf.as_bytes(), key.as_bytes()).unwrap();
    let relay = relay_with(Some(tls)).await;
    let qr = || {
        vault_proto::noise::PairKeys::generate(
            format!("https://{}", relay.addr),
            [3; 16],
            APP.into(),
            ORIGIN.into(),
        )
        .unwrap()
        .offer()
        .to_qr()
        .unwrap()
    };
    let tmp = tempfile::tempdir().unwrap();

    // Without the extra CA, only the bundled roots: the local CA is refused.
    let plain = factory();
    let h = create(&plain, &tmp.path().join("a"), &store()).await.handle;
    let r = h
        .clone()
        .serve_offer(
            qr(),
            TestApprover::new(Mode::Deny),
            Arc::new(TestListener::default()),
        )
        .await;
    match r {
        Err(FfiError::Relay { detail }) => assert!(detail.contains("TLS"), "{detail}"),
        other => panic!("expected a TLS failure, got {other:?}"),
    }
    assert_eq!(relay.mailboxes.open_connections(), 0);

    // With it, TLS succeeds and the vault waits in the mailbox for the browser.
    let trusting = VaultFactory::new(FactoryConfig {
        extra_ca_pem: Some(ca),
        ..config()
    })
    .unwrap();
    let h = create(&trusting, &tmp.path().join("b"), &store())
        .await
        .handle;
    let serve = tokio::spawn(h.clone().serve_offer(
        qr(),
        TestApprover::new(Mode::Deny),
        Arc::new(TestListener::default()),
    ));
    eventually("vault in the mailbox", || {
        relay.mailboxes.open_connections() == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!serve.is_finished(), "waiting for the browser's handshake");
    serve.abort();
}
