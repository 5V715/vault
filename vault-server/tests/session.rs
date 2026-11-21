// SPDX-License-Identifier: MIT
//! End-to-end tests: an in-process relay, a vault server on an in-memory container,
//! and a browser-side test client built from `vault-proto`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use vault_container::Crk;
use vault_container::Result as CResult;
use vault_container::backend::{ContainerBackend, Etag, MemoryBackend, PutOutcome};
use vault_container::engine::Container;
use vault_proto::msg::VaultMessage;
use vault_proto::noise::{PairKeys, Session};
use vault_proto::{
    Chunk, Chunker, ErrorCode, Notice, PairOffer, Reassembler, Request, RequestBody, ResponseBody,
};
use vault_server::{
    ApprovalDecision, ApprovalRequest, Approver, CompactionPolicy, EndReason, ServerConfig,
    ServerError, SessionHandle, SharedBackend, UnlockMaterial, VaultServer,
};

const T: Duration = Duration::from_secs(10);
/// The binding of the test vaults.
const APP: &str = "test-app";
const ORIGIN: &str = "https://app.example";

// ---------- relay ----------

struct Relay {
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

async fn relay() -> Relay {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let mailboxes = vault_mailbox::Mailboxes::new(vault_mailbox::MailboxConfig::default());
    tokio::spawn(vault_mailbox::serve(l, mailboxes, async move {
        let _ = rx.await;
    }));
    Relay { addr, _stop: tx }
}

impl Relay {
    fn url(&self) -> String {
        format!("ws://{}/", self.addr)
    }
}

// ---------- approvers ----------

#[derive(Default)]
struct Auto {
    answer: bool,
    calls: Mutex<Vec<(String, String)>>,
    /// (app id, origin, relay host) of each prompt.
    shown: Mutex<Vec<(String, String, String)>>,
}

impl Auto {
    fn new(answer: bool) -> Arc<Self> {
        Arc::new(Self {
            answer,
            calls: Mutex::default(),
            shown: Mutex::default(),
        })
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Approver for Auto {
    async fn approve(&self, req: &ApprovalRequest<'_>) -> ApprovalDecision {
        self.calls
            .lock()
            .unwrap()
            .push((req.device_name.into(), req.sas.into()));
        self.shown.lock().unwrap().push((
            req.app_id.into(),
            req.origin.into(),
            req.relay_host.into(),
        ));
        self.answer.into()
    }
}

/// Records prompts and their concurrency; each prompt waits for one answer from `tx`.
struct Gate {
    prompts: Mutex<Vec<(String, String)>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    answers: tokio::sync::Mutex<mpsc::UnboundedReceiver<bool>>,
    tx: mpsc::UnboundedSender<bool>,
}

impl Gate {
    fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            prompts: Mutex::default(),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            answers: tokio::sync::Mutex::new(rx),
            tx,
        })
    }
    fn prompts(&self) -> Vec<(String, String)> {
        self.prompts.lock().unwrap().clone()
    }
    async fn wait_prompts(&self, n: usize) -> Vec<(String, String)> {
        tokio::time::timeout(T, async {
            loop {
                let p = self.prompts();
                if p.len() >= n {
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("prompt did not appear")
    }
    fn answer(&self, yes: bool) {
        self.tx.send(yes).unwrap();
    }
}

struct InFlight<'a>(&'a AtomicUsize);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Approver for Gate {
    async fn approve(&self, req: &ApprovalRequest<'_>) -> ApprovalDecision {
        let (device_name, sas) = (req.device_name, req.sas);
        let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let _g = InFlight(&self.in_flight);
        self.max_in_flight.fetch_max(n, Ordering::SeqCst);
        self.prompts
            .lock()
            .unwrap()
            .push((device_name.into(), sas.into()));
        let mut rx = self.answers.lock().await;
        rx.recv().await.unwrap_or(false).into()
    }
}

/// Never answers.
struct Never;

#[async_trait::async_trait]
impl Approver for Never {
    async fn approve(&self, _: &ApprovalRequest<'_>) -> ApprovalDecision {
        std::future::pending().await
    }
}

// ---------- backends ----------

/// Memory backend whose writes can be made to fail (1 = I/O error), conflict (2),
/// report a rollback (3) or panic (4).
#[derive(Default)]
struct Failing {
    inner: MemoryBackend,
    mode: AtomicU8,
}

#[async_trait::async_trait]
impl ContainerBackend for Failing {
    async fn get(&self, name: &str) -> CResult<Option<(Vec<u8>, Etag)>> {
        self.inner.get(name).await
    }
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> CResult<PutOutcome> {
        match self.mode.load(Ordering::SeqCst) {
            1 => Err(vault_container::Error::Io("injected".into())),
            4 => panic!("injected backend panic"),
            _ => self.inner.put_if_absent(name, data).await,
        }
    }
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> CResult<PutOutcome> {
        match self.mode.load(Ordering::SeqCst) {
            1 => Err(vault_container::Error::Io("injected".into())),
            2 => Ok(PutOutcome::Conflict),
            3 => Err(vault_container::Error::Rollback),
            _ => self.inner.put_if_match(name, data, etag).await,
        }
    }
    async fn delete(&self, name: &str) -> CResult<()> {
        self.inner.delete(name).await
    }
    async fn list(&self, prefix: &str) -> CResult<Vec<String>> {
        self.inner.list(prefix).await
    }
}

// ---------- server ----------

async fn server_on(
    backend: SharedBackend,
    approver: Arc<dyn Approver>,
    config: ServerConfig,
) -> (VaultServer, vault_container::RecoveryKey) {
    let (c, rk) = Container::create(backend, APP, ORIGIN).await.unwrap();
    let s = VaultServer::new(c, approver, config).unwrap();
    (s, rk)
}

async fn server(approver: Arc<dyn Approver>, config: ServerConfig) -> VaultServer {
    server_on(Arc::new(MemoryBackend::new()), approver, config)
        .await
        .0
}

// ---------- browser test client ----------

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

struct Browser {
    ws: Ws,
    /// How long `recv` waits for the next message: a liveness bound only (no
    /// test relies on it expiring), `T` unless a test moves a lot of data.
    patience: Duration,
    noise: Session,
    chunker: Chunker,
    reasm: Reassembler,
    next_id: u64,
}

fn mailbox_id() -> [u8; 16] {
    static N: AtomicU64 = AtomicU64::new(1);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&N.fetch_add(1, Ordering::SeqCst).to_be_bytes());
    id[8..12].copy_from_slice(&std::process::id().to_be_bytes());
    id
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Browser {
    async fn send_message(&mut self, bytes: &[u8]) {
        for c in self.chunker.split(bytes).unwrap() {
            let f = self.noise.encrypt(&c.encode()).unwrap();
            self.ws.send(Message::Binary(f.into())).await.unwrap();
        }
    }

    async fn send(&mut self, body: RequestBody) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send_message(&Request { id, body }.encode()).await;
        id
    }

    /// Next vault message, or `None` once the connection ends.
    async fn recv(&mut self) -> Option<VaultMessage> {
        tokio::time::timeout(self.patience, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Binary(b))) => {
                        let pt = self.noise.decrypt(&b).unwrap();
                        let chunk = Chunk::decode(&pt).unwrap();
                        if let Some(m) = self.reasm.push(chunk).unwrap() {
                            return Some(VaultMessage::decode(&m).unwrap());
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .expect("vault did not answer in time")
    }

    async fn call(&mut self, body: RequestBody) -> ResponseBody {
        let id = self.send(body).await;
        match self.recv().await {
            Some(VaultMessage::Response(r)) => {
                assert_eq!(r.id, id);
                r.body
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    /// Send a request and return whatever arrives next.
    async fn call_any(&mut self, body: RequestBody) -> Option<VaultMessage> {
        self.send(body).await;
        self.recv().await
    }

    async fn expect_closed(&mut self) {
        assert!(self.recv().await.is_none(), "expected the session to close");
    }
}

/// Handshake only (no `OpenSession`).
async fn pair(server: &VaultServer, relay: &Relay) -> (Browser, SessionHandle) {
    pair_hooked(server, relay, async {}).await
}

/// Like [`pair`], running `between` after msg2 arrived and before msg3 is sent,
/// i.e. while the vault is in the middle of the handshake.
async fn pair_hooked(
    server: &VaultServer,
    relay: &Relay,
    between: impl std::future::Future<Output = ()>,
) -> (Browser, SessionHandle) {
    pair_as(server, relay, APP, between).await
}

/// Like [`pair_hooked`], for an offer that declares `app`.
async fn pair_as(
    server: &VaultServer,
    relay: &Relay,
    app: &str,
    between: impl std::future::Future<Output = ()>,
) -> (Browser, SessionHandle) {
    let keys = PairKeys::generate(relay.url(), mailbox_id(), app.into(), ORIGIN.into()).unwrap();
    let offer: PairOffer = keys.offer().clone();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/mailbox/{}",
        relay.addr,
        hex(&offer.mailbox_id)
    ))
    .await
    .unwrap();
    let (init, m1) = keys.into_initiator().unwrap();
    ws.send(Message::Binary(m1.into())).await.unwrap();
    let srv = tokio::spawn({
        let server = server.clone();
        async move { server.serve_offer(offer).await }
    });
    let m2 = match tokio::time::timeout(T, ws.next()).await.unwrap() {
        Some(Ok(Message::Binary(b))) => b.to_vec(),
        other => panic!("expected msg2, got {other:?}"),
    };
    let (m3, noise) = init.handle_msg2(&m2).unwrap();
    between.await;
    ws.send(Message::Binary(m3.into())).await.unwrap();
    let handle = srv.await.unwrap().unwrap();
    assert_eq!(handle.sas(), noise.sas());
    (
        Browser {
            ws,
            patience: T,
            noise,
            chunker: Chunker::new(),
            reasm: Reassembler::new(),
            next_id: 1,
        },
        handle,
    )
}

/// Handshake plus approved `OpenSession`.
async fn open(server: &VaultServer, relay: &Relay, device: &str) -> (Browser, SessionHandle) {
    let (mut b, h) = pair(server, relay).await;
    let r = b
        .call(RequestBody::OpenSession {
            device_name: device.into(),
        })
        .await;
    assert_eq!(r, ResponseBody::Approved);
    (b, h)
}

async fn ended(h: SessionHandle) -> EndReason {
    tokio::time::timeout(T, h.finished()).await.unwrap()
}

fn put(k: &[u8], v: &[u8]) -> RequestBody {
    RequestBody::Put {
        key: k.to_vec(),
        value: v.to_vec(),
    }
}

fn get(k: &[u8]) -> RequestBody {
    RequestBody::Get { key: k.to_vec() }
}

async fn expect_error0(b: &mut Browser, want: ErrorCode) {
    match b.recv().await {
        Some(VaultMessage::Response(r)) => {
            assert_eq!(r.id, 0);
            assert!(is_error(&r.body, want), "{r:?}");
        }
        other => panic!("expected error response, got {other:?}"),
    }
}

fn is_error(r: &ResponseBody, want: ErrorCode) -> bool {
    matches!(r, ResponseBody::Error { code, .. } if *code == want)
}

// ---------- tests ----------

#[tokio::test(flavor = "multi_thread")]
async fn requests_before_approval_refused() {
    let relay = relay().await;
    let approver = Auto::new(true);
    let s = server(approver.clone(), ServerConfig::default()).await;

    // A store request instead of OpenSession.
    let (mut b, h) = pair(&s, &relay).await;
    let r = b.call(get(b"k")).await;
    assert!(is_error(&r, ErrorCode::NotApproved), "{r:?}");
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::NotApproved);
    assert!(approver.calls().is_empty());

    // A store request while approval is still pending.
    let s = server(Arc::new(Never), ServerConfig::default()).await;
    let (mut b, h) = pair(&s, &relay).await;
    b.send(RequestBody::OpenSession {
        device_name: "Firefox".into(),
    })
    .await;
    let id = b.send(put(b"k", b"v")).await;
    match b.recv().await {
        Some(VaultMessage::Response(r)) => {
            assert_eq!(r.id, id);
            assert!(is_error(&r.body, ErrorCode::NotApproved), "{r:?}");
        }
        other => panic!("{other:?}"),
    }
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::NotApproved);
    assert_eq!(s.active_session(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_approval_closes() {
    let relay = relay().await;
    let approver = Auto::new(false);
    let s = server(approver.clone(), ServerConfig::default()).await;
    let (mut b, h) = pair(&s, &relay).await;
    let sas = h.sas().to_string();
    let r = b
        .call(RequestBody::OpenSession {
            device_name: "Laptop".into(),
        })
        .await;
    assert_eq!(r, ResponseBody::Denied);
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::Denied);
    assert_eq!(approver.calls(), vec![("Laptop".to_string(), sas)]);
    // The prompt shows the app, the origin and the relay host as well.
    assert_eq!(
        *approver.shown.lock().unwrap(),
        vec![(APP.to_string(), ORIGIN.to_string(), relay.addr.to_string())]
    );
    let log = s.session_log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].device_name, "Laptop");
    assert_eq!(log[0].reason, EndReason::Denied);
    assert!(log[0].ended_at_ms >= log[0].started_at_ms);
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_timeout_closes() {
    let relay = relay().await;
    let cfg = ServerConfig {
        approval_timeout: Duration::from_millis(300),
        ..ServerConfig::default()
    };
    let s = server(Arc::new(Never), cfg).await;
    let (mut b, h) = pair(&s, &relay).await;
    let r = b
        .call(RequestBody::OpenSession {
            device_name: "Laptop".into(),
        })
        .await;
    assert_eq!(r, ResponseBody::Denied);
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::ApprovalTimeout);
}

#[tokio::test(flavor = "multi_thread")]
async fn crud_commit_roundtrip() {
    let relay = relay().await;
    let approver = Auto::new(true);
    let s = server(approver.clone(), ServerConfig::default()).await;
    let (mut b, h) = open(&s, &relay, "Firefox on Linux").await;
    assert_eq!(approver.calls()[0].1, h.sas());
    assert_eq!(s.active_session(), Some(h.id()));

    assert_eq!(b.call(put(b"a/1", b"one")).await, ResponseBody::Ok);
    assert_eq!(b.call(put(b"a/2", b"two")).await, ResponseBody::Ok);
    assert_eq!(b.call(put(b"b/1", b"bee")).await, ResponseBody::Ok);
    let pia = |k: &[u8], v: &[u8]| RequestBody::PutIfAbsent {
        key: k.to_vec(),
        value: v.to_vec(),
    };
    assert_eq!(b.call(pia(b"a/1", b"x")).await, ResponseBody::Bool(false));
    assert_eq!(
        b.call(pia(b"a/3", b"three")).await,
        ResponseBody::Bool(true)
    );
    assert_eq!(
        b.call(get(b"a/1")).await,
        ResponseBody::Value(Some(b"one".to_vec()))
    );
    assert_eq!(b.call(get(b"zz")).await, ResponseBody::Value(None));
    assert_eq!(
        b.call(RequestBody::Delete {
            key: b"a/2".to_vec()
        })
        .await,
        ResponseBody::Ok
    );
    assert_eq!(
        b.call(RequestBody::ScanPrefix {
            prefix: b"a/".to_vec()
        })
        .await,
        ResponseBody::Entries(vec![
            (b"a/1".to_vec(), b"one".to_vec()),
            (b"a/3".to_vec(), b"three".to_vec()),
        ])
    );
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
    assert_eq!(b.call(RequestBody::Clear).await, ResponseBody::Ok);
    assert_eq!(
        b.call(RequestBody::ScanPrefix { prefix: vec![] }).await,
        ResponseBody::Entries(vec![])
    );
    assert_eq!(b.call(put(b"c", b"sea")).await, ResponseBody::Ok);
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 2 }
    );
    // A second OpenSession inside an active session is unexpected but not fatal.
    let r = b
        .call(RequestBody::OpenSession {
            device_name: "again".into(),
        })
        .await;
    assert!(is_error(&r, ErrorCode::BadRequest), "{r:?}");
    assert_eq!(b.call(RequestBody::Close).await, ResponseBody::Ok);
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::Closed);
    assert_eq!(s.active_session(), None);

    // Committed state is visible to the next session.
    let (mut b, _h) = open(&s, &relay, "Firefox on Linux").await;
    assert_eq!(
        b.call(RequestBody::ScanPrefix { prefix: vec![] }).await,
        ResponseBody::Entries(vec![(b"c".to_vec(), b"sea".to_vec())])
    );
    let log = s.session_log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].device_name, "Firefox on Linux");
    assert_eq!(log[0].reason, EndReason::Closed);
}

#[tokio::test(flavor = "multi_thread")]
async fn uncommitted_writes_discarded_when_session_ends() {
    let relay = relay().await;
    let s = server(Auto::new(true), ServerConfig::default()).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(put(b"k", b"v")).await, ResponseBody::Ok);
    drop(b); // disconnect without Commit
    assert_eq!(ended(h).await, EndReason::Disconnected);
    let (mut b, _h) = open(&s, &relay, "B").await;
    assert_eq!(b.call(get(b"k")).await, ResponseBody::Value(None));
}

#[tokio::test(flavor = "multi_thread")]
async fn second_session_terminates_first() {
    let relay = relay().await;
    let s = server(Auto::new(true), ServerConfig::default()).await;
    let (mut a, ha) = open(&s, &relay, "A").await;
    assert_eq!(a.call(put(b"kept", b"1")).await, ResponseBody::Ok);
    assert_eq!(
        a.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
    assert_eq!(a.call(put(b"lost", b"2")).await, ResponseBody::Ok);

    let (mut b, hb) = open(&s, &relay, "B").await;
    assert_eq!(
        a.recv().await,
        Some(VaultMessage::Notice(Notice::SessionTerminated {
            reason: "replaced".into()
        }))
    );
    a.expect_closed().await;
    assert_eq!(ended(ha).await, EndReason::Replaced);
    assert_eq!(s.active_session(), Some(hb.id()));

    assert_eq!(b.call(get(b"lost")).await, ResponseBody::Value(None));
    assert_eq!(
        b.call(get(b"kept")).await,
        ResponseBody::Value(Some(b"1".to_vec()))
    );
    // B commits cleanly: A's discarded write does not resurface.
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
    let log = s.session_log();
    assert_eq!(log.len(), 1);
    assert_eq!(
        (log[0].device_name.as_str(), log[0].reason),
        ("A", EndReason::Replaced)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_ends_session() {
    let relay = relay().await;
    let backend: SharedBackend = Arc::new(MemoryBackend::new());
    let (s, rk) = server_on(backend.clone(), Auto::new(true), ServerConfig::default()).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(put(b"kept", b"1")).await, ResponseBody::Ok);
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
    assert_eq!(b.call(put(b"lost", b"2")).await, ResponseBody::Ok);

    s.lock().await;
    assert!(s.is_locked());
    assert_eq!(b.recv().await, Some(VaultMessage::Notice(Notice::Locked)));
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::Locked);
    assert_eq!(s.active_session(), None);

    // Pairing still works while locked, but an approval that brings no unlock
    // material cannot unlock the vault: the browser is denied.
    let (mut b, h) = pair(&s, &relay).await;
    let r = b
        .call(RequestBody::OpenSession {
            device_name: "A".into(),
        })
        .await;
    assert_eq!(r, ResponseBody::Denied);
    assert_eq!(ended(h).await, EndReason::UnlockFailed);
    assert!(s.is_locked());

    // Unlock with a re-opened container: committed data survives, pending did not.
    let (c, _) = Container::open_with_recovery(backend, &rk, None)
        .await
        .unwrap();
    s.unlock(c).await;
    let (mut b, _h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(get(b"lost")).await, ResponseBody::Value(None));
    assert_eq!(
        b.call(get(b"kept")).await,
        ResponseBody::Value(Some(b"1".to_vec()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_ends_session_awaiting_approval() {
    let relay = relay().await;
    let s = server(Arc::new(Never), ServerConfig::default()).await;
    let (mut b, h) = pair(&s, &relay).await;
    b.send(RequestBody::OpenSession {
        device_name: "A".into(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    s.lock().await;
    assert_eq!(b.recv().await, Some(VaultMessage::Notice(Notice::Locked)));
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::Locked);
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_ends_session() {
    let relay = relay().await;
    let cfg = ServerConfig {
        session_timeout: Duration::from_millis(400),
        ..ServerConfig::default()
    };
    let s = server(Auto::new(true), cfg).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(put(b"k", b"v")).await, ResponseBody::Ok);
    assert_eq!(
        b.recv().await,
        Some(VaultMessage::Notice(Notice::SessionTerminated {
            reason: "timeout".into()
        }))
    );
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::Timeout);
    let (mut b, _h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(get(b"k")).await, ResponseBody::Value(None));
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_timeout_ends_session() {
    let relay = relay().await;
    let cfg = ServerConfig {
        idle_timeout: Some(Duration::from_millis(500)),
        ..ServerConfig::default()
    };
    let s = server(Auto::new(true), cfg).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    // Activity keeps the session alive past the idle limit.
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(b.call(get(b"k")).await, ResponseBody::Value(None));
    }
    assert_eq!(
        b.recv().await,
        Some(VaultMessage::Notice(Notice::SessionTerminated {
            reason: "idle".into()
        }))
    );
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::IdleTimeout);
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_failure_reported_as_error() {
    let relay = relay().await;
    let backend = Arc::new(Failing::default());
    let (s, _) = server_on(backend.clone(), Auto::new(true), ServerConfig::default()).await;
    let (mut b, _h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(put(b"k", b"v")).await, ResponseBody::Ok);

    backend.mode.store(1, Ordering::SeqCst);
    let r = b.call(RequestBody::Commit).await;
    assert!(is_error(&r, ErrorCode::Storage), "{r:?}");

    backend.mode.store(2, Ordering::SeqCst);
    let r = b.call(RequestBody::Commit).await;
    assert!(is_error(&r, ErrorCode::Conflict), "{r:?}");

    // Rollback / diverged replicas are storage integrity failures, not conflicts.
    backend.mode.store(3, Ordering::SeqCst);
    let r = b.call(RequestBody::Commit).await;
    assert!(is_error(&r, ErrorCode::Storage), "{r:?}");

    // The batch was kept; the session is still usable and a retry succeeds.
    backend.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        b.call(get(b"k")).await,
        ResponseBody::Value(Some(b"v".to_vec()))
    );
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn large_value_chunked_roundtrip() {
    let relay = relay().await;
    let s = server(Auto::new(true), ServerConfig::default()).await;
    let (mut b, _h) = open(&s, &relay, "A").await;
    // 8 MiB through Noise, the relay and the container in a debug build: slow CI
    // runners (windows-latest) need far more than T per answer. Only liveness is
    // at stake here; the answers' order and content are still checked exactly.
    b.patience = Duration::from_secs(180);
    let big: Vec<u8> = (0..8 * 1024 * 1024)
        .map(|i: usize| (i % 251) as u8)
        .collect();
    assert_eq!(b.call(put(b"big", &big)).await, ResponseBody::Ok);
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
    assert_eq!(
        b.call(get(b"big")).await,
        ResponseBody::Value(Some(big.clone()))
    );
    assert_eq!(
        b.call(RequestBody::ScanPrefix {
            prefix: b"b".to_vec()
        })
        .await,
        ResponseBody::Entries(vec![(b"big".to_vec(), big)])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_message_closes_session() {
    let relay = relay().await;
    let s = server(Auto::new(true), ServerConfig::default()).await;

    // Well-encrypted but undecodable application message.
    let (mut b, h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(put(b"k", b"v")).await, ResponseBody::Ok);
    b.send_message(&[0xff; 10]).await;
    expect_error0(&mut b, ErrorCode::BadRequest).await;
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::ProtocolError);

    // A frame that is not valid Noise ciphertext.
    let (mut b, h) = open(&s, &relay, "A").await;
    b.ws.send(Message::Binary(vec![1u8; 40].into()))
        .await
        .unwrap();
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::ProtocolError);

    // Chunks out of sequence (first chunk with index 1).
    let (mut b, h) = open(&s, &relay, "A").await;
    let c = Chunk {
        msg_id: 99,
        index: 1,
        last: true,
        payload: Request {
            id: 1,
            body: get(b"k"),
        }
        .encode(),
    };
    let f = b.noise.encrypt(&c.encode()).unwrap();
    b.ws.send(Message::Binary(f.into())).await.unwrap();
    expect_error0(&mut b, ErrorCode::BadRequest).await;
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::ProtocolError);

    // Undecodable before approval too; an oversize field is reported as TooLarge.
    let (mut b, h) = pair(&s, &relay).await;
    b.send(RequestBody::OpenSession {
        device_name: "x".repeat(65),
    })
    .await;
    expect_error0(&mut b, ErrorCode::TooLarge).await;
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::ProtocolError);

    // The uncommitted write of the first session was discarded.
    let (mut b, _h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(get(b"k")).await, ResponseBody::Value(None));
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_with_wrong_browser_key_fails() {
    let relay = relay().await;
    let s = server(Auto::new(true), ServerConfig::default()).await;
    // The vault scans offer A but the browser handshakes with key B (same mailbox).
    let a = PairKeys::generate(relay.url(), mailbox_id(), APP.into(), ORIGIN.into()).unwrap();
    let b =
        PairKeys::generate(relay.url(), a.offer().mailbox_id, APP.into(), ORIGIN.into()).unwrap();
    let offer_a = a.offer().clone();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/mailbox/{}",
        relay.addr,
        hex(&offer_a.mailbox_id)
    ))
    .await
    .unwrap();
    let (init, m1) = b.into_initiator().unwrap();
    ws.send(Message::Binary(m1.into())).await.unwrap();
    let srv = tokio::spawn({
        let s = s.clone();
        async move { s.serve_offer(offer_a).await }
    });
    // The prologues differ (offer B vs. scanned offer A), so msg2 does not decrypt
    // and the browser gives up; the vault must not produce a session.
    let m2 = match tokio::time::timeout(T, ws.next()).await.unwrap() {
        Some(Ok(Message::Binary(b))) => b.to_vec(),
        other => panic!("expected msg2, got {other:?}"),
    };
    assert!(init.handle_msg2(&m2).is_err());
    ws.close(None).await.unwrap();
    let r = tokio::time::timeout(T, srv).await.unwrap().unwrap();
    assert!(matches!(r, Err(ServerError::Handshake)), "{r:?}");
    assert!(s.session_log().is_empty());
}

async fn open_request(b: &mut Browser, device: &str) -> u64 {
    b.send(RequestBody::OpenSession {
        device_name: device.into(),
    })
    .await
}

async fn expect_response(b: &mut Browser, id: u64) -> ResponseBody {
    match b.recv().await {
        Some(VaultMessage::Response(r)) => {
            assert_eq!(r.id, id);
            r.body
        }
        other => panic!("expected response {id}, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_are_serialized_and_bound_to_the_shown_sas() {
    let relay = relay().await;
    let gate = Gate::new();
    let s = server(gate.clone(), ServerConfig::default()).await;
    let (mut a, ha) = pair(&s, &relay).await;
    let (mut b, hb) = pair(&s, &relay).await;
    let ia = open_request(&mut a, "A").await;
    let ib = open_request(&mut b, "B").await;

    // Only one prompt is shown while it is unanswered.
    let first = gate.wait_prompts(1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gate.prompts().len(), 1);
    assert_eq!(gate.max_in_flight.load(Ordering::SeqCst), 1);

    // The answer applies to the session whose SAS was shown.
    let (yes, yes_id, no, no_id, no_h) = if first[0].1 == ha.sas() {
        assert_eq!(first[0].0, "A");
        (&mut a, ia, &mut b, ib, hb)
    } else {
        assert_eq!(first[0], ("B".to_string(), hb.sas().to_string()));
        (&mut b, ib, &mut a, ia, ha)
    };
    gate.answer(true);
    assert_eq!(expect_response(yes, yes_id).await, ResponseBody::Approved);

    // Then the other session is prompted, with its own SAS.
    let second = gate.wait_prompts(2).await;
    assert_eq!(second[1].1, no_h.sas());
    assert_ne!(second[1].1, first[0].1);
    gate.answer(false);
    assert_eq!(expect_response(no, no_id).await, ResponseBody::Denied);
    no.expect_closed().await;
    assert_eq!(ended(no_h).await, EndReason::Denied);
    assert_eq!(gate.max_in_flight.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_timeout_starts_when_prompt_is_shown() {
    let relay = relay().await;
    let gate = Gate::new();
    let cfg = ServerConfig {
        approval_timeout: Duration::from_millis(700),
        ..ServerConfig::default()
    };
    let s = server(gate.clone(), cfg).await;
    let (mut a, _ha) = pair(&s, &relay).await;
    let (mut b, _hb) = pair(&s, &relay).await;
    let ia = open_request(&mut a, "A").await;
    let ib = open_request(&mut b, "B").await;
    gate.wait_prompts(1).await;
    tokio::time::sleep(Duration::from_millis(450)).await;
    gate.answer(true);
    gate.wait_prompts(2).await;
    // 900 ms after its OpenSession, but only 450 ms after its prompt.
    tokio::time::sleep(Duration::from_millis(450)).await;
    gate.answer(true);
    let ra = expect_response(&mut a, ia).await;
    let rb = expect_response(&mut b, ib).await;
    assert_eq!((ra, rb), (ResponseBody::Approved, ResponseBody::Approved));
}

#[tokio::test(flavor = "multi_thread")]
async fn at_most_two_sessions_await_approval() {
    let relay = relay().await;
    let gate = Gate::new();
    let s = server(gate.clone(), ServerConfig::default()).await;
    let (mut a, _ha) = pair(&s, &relay).await;
    let (mut b, _hb) = pair(&s, &relay).await;
    let (mut c, hc) = pair(&s, &relay).await;
    open_request(&mut a, "A").await;
    gate.wait_prompts(1).await;
    open_request(&mut b, "B").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ic = open_request(&mut c, "C").await;
    assert_eq!(expect_response(&mut c, ic).await, ResponseBody::Denied);
    c.expect_closed().await;
    assert_eq!(ended(hc).await, EndReason::Busy);
    assert_eq!(gate.prompts().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_is_answered_in_every_state() {
    let relay = relay().await;
    let gate = Gate::new();
    let cfg = ServerConfig {
        idle_timeout: Some(Duration::from_millis(600)),
        ..ServerConfig::default()
    };
    let s = server(gate.clone(), cfg).await;
    let (mut b, h) = pair(&s, &relay).await;
    assert_eq!(b.call(RequestBody::Ping).await, ResponseBody::Ok);
    let open = open_request(&mut b, "A").await;
    gate.wait_prompts(1).await;
    assert_eq!(b.call(RequestBody::Ping).await, ResponseBody::Ok);
    gate.answer(true);
    assert_eq!(expect_response(&mut b, open).await, ResponseBody::Approved);
    assert_eq!(b.call(RequestBody::Ping).await, ResponseBody::Ok);
    // Pings keep the connection alive but do not count as activity.
    let ended_idle = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match b.call_any(RequestBody::Ping).await {
                Some(VaultMessage::Response(r)) => assert_eq!(r.body, ResponseBody::Ok),
                Some(VaultMessage::Notice(n)) => return Some(n),
                None => panic!("closed without notice"),
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .expect("pings must not reset the idle timeout");
    assert_eq!(
        ended_idle,
        Some(Notice::SessionTerminated {
            reason: "idle".into()
        })
    );
    assert_eq!(ended(h).await, EndReason::IdleTimeout);
}

#[tokio::test(flavor = "multi_thread")]
async fn panic_in_request_still_cleans_up() {
    let relay = relay().await;
    let backend = Arc::new(Failing::default());
    let (s, _) = server_on(backend.clone(), Auto::new(true), ServerConfig::default()).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    assert_eq!(b.call(put(b"k", b"v")).await, ResponseBody::Ok);
    backend.mode.store(4, Ordering::SeqCst);
    b.send(RequestBody::Commit).await;
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::Internal);
    assert_eq!(s.active_session(), None);
    let log = s.session_log();
    assert_eq!(log.len(), 1);
    assert_eq!(
        (log[0].device_name.as_str(), log[0].reason),
        ("A", EndReason::Internal)
    );

    backend.mode.store(0, Ordering::SeqCst);
    let (mut b, _h) = open(&s, &relay, "B").await;
    assert_eq!(b.call(get(b"k")).await, ResponseBody::Value(None));
    assert_eq!(b.call(put(b"j", b"w")).await, ResponseBody::Ok);
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
}

// ---------- compaction policy ----------

fn policy(max_segments: usize, max_dead_ratio: f64, min_segments_for_ratio: usize) -> ServerConfig {
    ServerConfig {
        compaction: CompactionPolicy {
            max_segments,
            max_dead_ratio,
            min_segments_for_ratio,
        },
        ..ServerConfig::default()
    }
}

async fn commit_keys(b: &mut Browser, keys: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) {
    for (k, v) in keys {
        assert_eq!(b.call(put(&k, &v)).await, ResponseBody::Ok);
        assert!(matches!(
            b.call(RequestBody::Commit).await,
            ResponseBody::Committed { .. }
        ));
    }
}

async fn close(b: &mut Browser, h: SessionHandle) {
    assert_eq!(b.call(RequestBody::Close).await, ResponseBody::Ok);
    assert_eq!(ended(h).await, EndReason::Closed);
}

fn distinct(n: u8) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n).map(|i| (vec![b'k', i], vec![i; 100])).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_runs_after_session_when_threshold_crossed() {
    let relay = relay().await;
    let backend: SharedBackend = Arc::new(MemoryBackend::new());
    let (s, rk) = server_on(backend.clone(), Auto::new(true), policy(4, 1.0, usize::MAX)).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    commit_keys(&mut b, distinct(5)).await;
    // Still in the session: never compacted on the commit path.
    assert_eq!(s.container_stats().await.unwrap().segments, 5);
    let before = s.high_water().await.unwrap();
    close(&mut b, h).await;
    let st = s.container_stats().await.unwrap();
    assert_eq!(st.segments, 1, "compacted at session end");
    assert_eq!(
        backend.list("seg/").await.unwrap().len(),
        1,
        "old segments deleted"
    );
    let hw = s.high_water().await.unwrap();
    assert_eq!(
        hw.seq,
        before.seq + 1,
        "compaction is a new, later manifest"
    );

    // Everything is still readable, over the protocol and after a reopen with
    // the new high-water mark (anti-rollback keeps working).
    let (mut b, _h) = open(&s, &relay, "B").await;
    let want: Vec<_> = distinct(5);
    assert_eq!(
        b.call(RequestBody::ScanPrefix { prefix: vec![] }).await,
        ResponseBody::Entries(want.clone())
    );
    let (c, _) = Container::open_with_recovery(backend.clone(), &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c.scan_prefix(b""), want);
    // The pre-compaction mark is older: still accepted (a forward move).
    assert!(
        Container::open_with_recovery(backend, &rk, Some(before))
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_runs_when_dead_bytes_dominate() {
    let relay = relay().await;
    let s = server(Auto::new(true), policy(1000, 0.5, 2)).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    commit_keys(&mut b, (0..3u8).map(|i| (b"same".to_vec(), vec![i; 200]))).await;
    let st = s.container_stats().await.unwrap();
    assert!(st.dead_ratio() > 0.5, "{st:?}");
    close(&mut b, h).await;
    let st = s.container_stats().await.unwrap();
    assert_eq!(st.segments, 1);
    assert_eq!(st.dead_ratio(), 0.0);
    let (mut b, _h) = open(&s, &relay, "B").await;
    assert_eq!(
        b.call(get(b"same")).await,
        ResponseBody::Value(Some(vec![2; 200]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_compaction_below_threshold() {
    let relay = relay().await;
    let s = server(Auto::new(true), ServerConfig::default()).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    commit_keys(&mut b, distinct(3)).await;
    let before = s.high_water().await.unwrap();
    close(&mut b, h).await;
    assert_eq!(s.container_stats().await.unwrap().segments, 3);
    assert_eq!(s.high_water().await.unwrap(), before, "no new manifest");
    // Default thresholds as documented.
    let d = CompactionPolicy::default();
    assert_eq!(d.max_segments, vault_server::COMPACT_MAX_SEGMENTS);
    assert_eq!(vault_server::COMPACT_MAX_SEGMENTS, 64);
    assert_eq!(d.max_dead_ratio, 0.5);
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_never_folds_another_sessions_pending_writes() {
    let relay = relay().await;
    let s = server(Auto::new(true), policy(2, 1.0, usize::MAX)).await;
    let (mut a, ha) = open(&s, &relay, "A").await;
    commit_keys(&mut a, distinct(3)).await;
    // B replaces A and stages a write: A's end must not compact (B is active).
    let (mut b, hb) = open(&s, &relay, "B").await;
    assert_eq!(ended(ha).await, EndReason::Replaced);
    assert_eq!(b.call(put(b"staged", b"x")).await, ResponseBody::Ok);
    assert_eq!(s.container_stats().await.unwrap().segments, 3);
    // B goes away without committing: its write is discarded, then compaction runs.
    drop(b);
    assert_eq!(ended(hb).await, EndReason::Disconnected);
    assert_eq!(s.container_stats().await.unwrap().segments, 1);
    let (mut c, _h) = open(&s, &relay, "C").await;
    assert_eq!(c.call(get(b"staged")).await, ResponseBody::Value(None));
    assert_eq!(
        c.call(get(&[b'k', 2])).await,
        ResponseBody::Value(Some(vec![2; 100]))
    );
}

/// A container with another binding.
async fn other_container() -> vault_server::VaultContainer {
    let backend: SharedBackend = Arc::new(MemoryBackend::new());
    Container::create(backend, "other-app", ORIGIN)
        .await
        .unwrap()
        .0
}

/// The vault is locked and unlocked with a container bound to another app while
/// the handshake runs: the session must be checked against the container it would
/// serve, not the one installed when the offer was scanned.
#[tokio::test(flavor = "multi_thread")]
async fn binding_rechecked_after_handshake() {
    let relay = relay().await;
    let approver = Auto::new(true);
    let s = server(approver.clone(), ServerConfig::default()).await;
    let swap = {
        let s = s.clone();
        async move {
            s.lock().await;
            s.unlock(other_container().await).await;
        }
    };
    let (mut b, h) = pair_hooked(&s, &relay, swap).await;
    // The vault sends the notice right after the handshake, unasked, and closes.
    assert_eq!(b.recv().await, Some(VaultMessage::Notice(Notice::WrongApp)));
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::WrongApp);
    assert!(approver.calls().is_empty(), "approver must not be asked");
    assert_eq!(s.active_session(), None);
}

/// The container is swapped after the handshake but before the prompt.
#[tokio::test(flavor = "multi_thread")]
async fn binding_rechecked_before_prompt() {
    let relay = relay().await;
    let approver = Auto::new(true);
    let s = server(approver.clone(), ServerConfig::default()).await;
    let (mut b, h) = pair(&s, &relay).await;
    s.unlock(other_container().await).await;
    let r = b
        .call_any(RequestBody::OpenSession {
            device_name: "A".into(),
        })
        .await;
    assert_eq!(r, Some(VaultMessage::Notice(Notice::WrongApp)));
    assert_eq!(ended(h).await, EndReason::WrongApp);
    assert!(approver.calls().is_empty(), "approver must not be asked");
}

/// The vault ends a session with a notice while the client's last frames are still
/// unread. Dropping the socket with unread data makes Windows reset the connection
/// and discard the notice in flight, so the vault must drain the client first.
#[tokio::test(flavor = "multi_thread")]
async fn final_notice_survives_unread_client_data() {
    let relay = relay().await;
    let approver = Auto::new(true);
    let s = server(approver.clone(), ServerConfig::default()).await;
    let (mut b, h) = pair(&s, &relay).await;
    s.unlock(other_container().await).await;
    // The first request makes the vault end the session with WrongApp; the pings
    // behind it are never read by the vault's session loop.
    b.send(RequestBody::OpenSession {
        device_name: "A".into(),
    })
    .await;
    for _ in 0..8 {
        b.send(RequestBody::Ping).await;
    }
    assert_eq!(b.recv().await, Some(VaultMessage::Notice(Notice::WrongApp)));
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::WrongApp);
}

/// The container is swapped while the prompt is shown: a "yes" for the old
/// binding must not activate the session on the new container.
#[tokio::test(flavor = "multi_thread")]
async fn binding_rechecked_before_approved() {
    let relay = relay().await;
    let gate = Gate::new();
    let s = server(gate.clone(), ServerConfig::default()).await;
    let (mut b, h) = pair(&s, &relay).await;
    open_request(&mut b, "A").await;
    gate.wait_prompts(1).await;
    s.unlock(other_container().await).await;
    gate.answer(true);
    assert_eq!(b.recv().await, Some(VaultMessage::Notice(Notice::WrongApp)));
    assert_eq!(ended(h).await, EndReason::WrongApp);
    assert_eq!(s.active_session(), None);
}

// ---------- locked pairing ----------

/// Approver for a locked vault: records each prompt (whether the server was
/// still locked, mailbox-taken flag, SAS), waits for `go`, then approves with
/// its unlock material (once) or denies when it has none left.
struct Unlocker {
    material: Mutex<Option<UnlockMaterial>>,
    /// (mailbox_taken, sas) of each prompt.
    seen: Mutex<Vec<(bool, String)>>,
    go: tokio::sync::Semaphore,
}

impl Unlocker {
    fn new(material: UnlockMaterial, gated: bool) -> Arc<Self> {
        Arc::new(Self {
            material: Mutex::new(Some(material)),
            seen: Mutex::default(),
            go: tokio::sync::Semaphore::new(if gated { 0 } else { 1 << 20 }),
        })
    }
    fn seen(&self) -> Vec<(bool, String)> {
        self.seen.lock().unwrap().clone()
    }
    async fn wait_seen(&self, n: usize) -> Vec<(bool, String)> {
        tokio::time::timeout(T, async {
            loop {
                let p = self.seen();
                if p.len() >= n {
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("prompt did not appear")
    }
    fn material_taken(&self) -> bool {
        self.material.lock().unwrap().is_none()
    }
}

#[async_trait::async_trait]
impl Approver for Unlocker {
    async fn approve(&self, req: &ApprovalRequest<'_>) -> ApprovalDecision {
        self.seen
            .lock()
            .unwrap()
            .push((req.mailbox_taken, req.sas.into()));
        self.go.acquire().await.unwrap().forget();
        match self.material.lock().unwrap().take() {
            Some(m) => ApprovalDecision::Approve(m),
            None => ApprovalDecision::Deny,
        }
    }
}

/// A fresh container (bound to `APP`) and its root key; the container itself
/// is dropped, as on a phone where only the wrapped CRK is kept.
async fn locked_backend() -> (SharedBackend, Crk) {
    let backend: SharedBackend = Arc::new(MemoryBackend::new());
    let (c, rk) = Container::create(backend.clone(), APP, ORIGIN)
        .await
        .unwrap();
    drop(c);
    let (_, crk) = Container::open_with_recovery(backend.clone(), &rk, None)
        .await
        .unwrap();
    (backend, crk)
}

async fn locked_server(backend: SharedBackend, approver: Arc<dyn Approver>) -> VaultServer {
    VaultServer::new_locked(backend, approver, ServerConfig::default())
        .await
        .unwrap()
}

fn crk_material(crk: Crk) -> UnlockMaterial {
    UnlockMaterial::Crk {
        crk,
        high_water: None,
    }
}

/// A locked vault runs the handshake (and so shows the code) without any key.
#[tokio::test(flavor = "multi_thread")]
async fn serve_offer_while_locked_shows_code() {
    let relay = relay().await;
    let (backend, crk) = locked_backend().await;
    let approver = Unlocker::new(crk_material(crk), true);
    let s = locked_server(backend, approver.clone()).await;
    assert!(s.is_locked());
    assert_eq!(s.binding(), (APP.to_string(), ORIGIN.to_string()));
    // `pair` checks that the vault's code equals the browser's.
    let (mut b, h) = pair(&s, &relay).await;
    assert_eq!(h.sas().len(), 6);
    assert!(s.is_locked());
    open_request(&mut b, "A").await;
    let seen = approver.wait_seen(1).await;
    assert_eq!(seen, vec![(false, h.sas().to_string())]);
    // The prompt is shown while the vault is still locked.
    assert!(s.is_locked());
    assert!(!approver.material_taken());
}

/// The approval unlocks the container (with the CRK the approver produced at
/// that moment), and only then is `Approved` sent.
#[tokio::test(flavor = "multi_thread")]
async fn approval_unlocks_then_sends_approved() {
    let relay = relay().await;
    let (backend, crk) = locked_backend().await;
    let approver = Unlocker::new(crk_material(crk), true);
    let s = locked_server(backend.clone(), approver.clone()).await;
    let (mut b, h) = pair(&s, &relay).await;
    let id = open_request(&mut b, "A").await;
    approver.wait_seen(1).await;
    assert!(s.is_locked());
    assert_eq!(s.high_water().await, None);
    approver.go.add_permits(1);
    assert_eq!(expect_response(&mut b, id).await, ResponseBody::Approved);
    assert!(!s.is_locked());
    assert_eq!(s.active_session(), Some(h.id()));
    assert_eq!(b.call(put(b"k", b"v")).await, ResponseBody::Ok);
    assert_eq!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    );
    assert_eq!(s.high_water().await.map(|hw| hw.seq), Some(1));
}

/// Material that does not open the container: the browser is denied and the
/// vault stays locked.
#[tokio::test(flavor = "multi_thread")]
async fn approval_unlock_failure_denies() {
    let relay = relay().await;
    let (backend, _) = locked_backend().await;
    // The root key of another container.
    let (_, wrong) = locked_backend().await;
    let approver = Unlocker::new(crk_material(wrong), false);
    let s = locked_server(backend, approver.clone()).await;
    let (mut b, h) = pair(&s, &relay).await;
    let r = b
        .call(RequestBody::OpenSession {
            device_name: "A".into(),
        })
        .await;
    assert_eq!(r, ResponseBody::Denied);
    assert_eq!(ended(h).await, EndReason::UnlockFailed);
    assert!(approver.material_taken());
    assert!(s.is_locked());
    assert_eq!(s.active_session(), None);
    assert_eq!(s.high_water().await, None);
}

/// The binding check of a locked vault uses the plaintext header: a wrong app
/// is refused before the approver is asked, and nothing is unlocked.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_app_checked_from_header_while_locked() {
    let relay = relay().await;
    let (backend, crk) = locked_backend().await;
    let approver = Unlocker::new(crk_material(crk), false);
    let s = locked_server(backend, approver.clone()).await;
    let (mut b, h) = pair_as(&s, &relay, "other-app", async {}).await;
    assert_eq!(b.recv().await, Some(VaultMessage::Notice(Notice::WrongApp)));
    b.expect_closed().await;
    assert_eq!(ended(h).await, EndReason::WrongApp);
    assert!(approver.seen().is_empty(), "approver must not be asked");
    assert!(!approver.material_taken());
    assert!(s.is_locked());
}

/// Every offer gets its own random Noise static key: nothing is pinned or reused.
#[tokio::test(flavor = "multi_thread")]
async fn per_session_static_key_is_fresh() {
    let relay = relay().await;
    let s = server(Auto::new(false), ServerConfig::default()).await;
    let (b1, _h1) = pair(&s, &relay).await;
    let (b2, _h2) = pair(&s, &relay).await;
    let (k1, k2) = (b1.noise.remote_static(), b2.noise.remote_static());
    assert_ne!(k1, k2);
    assert_ne!(k1, [0u8; 32]);
    // Locked mode too.
    let (backend, crk) = locked_backend().await;
    let l = locked_server(backend, Unlocker::new(crk_material(crk), false)).await;
    let (b3, _h3) = pair(&l, &relay).await;
    let k3 = b3.noise.remote_static();
    assert!(k3 != k1 && k3 != k2);
}

/// Mailbox already taken (HTTP 409): the approver is told so that it can warn,
/// and an `Approve` it returns anyway is ignored: nothing is unlocked and the
/// unlock material is dropped unused.
#[tokio::test(flavor = "multi_thread")]
async fn mailbox_taken_reported_to_approver() {
    let relay = relay().await;
    let (backend, crk) = locked_backend().await;
    let approver = Unlocker::new(crk_material(crk), false);
    let s = locked_server(backend, approver.clone()).await;
    let keys = PairKeys::generate(relay.url(), mailbox_id(), APP.into(), ORIGIN.into()).unwrap();
    let url = format!(
        "ws://{}/mailbox/{}",
        relay.addr,
        hex(&keys.offer().mailbox_id)
    );
    // The browser and someone who read the pairing text hold both places.
    let (_browser, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_impostor, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let r = s.serve_offer(keys.offer().clone()).await;
    assert!(matches!(r, Err(ServerError::MailboxTaken)), "{r:?}");
    assert_eq!(approver.seen(), vec![(true, String::new())]);
    assert!(approver.material_taken(), "the approver answered Approve");
    assert!(
        s.is_locked(),
        "an Approve for a taken mailbox must not unlock"
    );
    assert_eq!(s.active_session(), None);
    assert!(s.session_log().is_empty());
}

/// Approves with unlock material, but calls `lock()` itself just before it
/// returns: the lock lands after the user's answer and before the session takes
/// the container lock (the session polls `ctl` before the approval, so the
/// queued `Terminate` is not seen first).
struct LockingApprover {
    server: std::sync::OnceLock<VaultServer>,
    material: Mutex<Option<UnlockMaterial>>,
}

#[async_trait::async_trait]
impl Approver for LockingApprover {
    async fn approve(&self, _: &ApprovalRequest<'_>) -> ApprovalDecision {
        self.server.get().unwrap().lock().await;
        match self.material.lock().unwrap().take() {
            Some(m) => ApprovalDecision::Approve(m),
            None => ApprovalDecision::Deny,
        }
    }
}

/// A `lock()` that races an approval wins: nothing is unlocked or installed,
/// and the browser never gets `Approved`.
#[tokio::test(flavor = "multi_thread")]
async fn lock_during_approval_wins() {
    let relay = relay().await;
    let (backend, crk) = locked_backend().await;
    let approver = Arc::new(LockingApprover {
        server: std::sync::OnceLock::new(),
        material: Mutex::new(Some(crk_material(crk))),
    });
    let s = locked_server(backend, approver.clone()).await;
    approver.server.set(s.clone()).unwrap();
    let (mut b, h) = pair(&s, &relay).await;
    let r = b
        .call_any(RequestBody::OpenSession {
            device_name: "A".into(),
        })
        .await;
    assert_eq!(r, Some(VaultMessage::Notice(Notice::Locked)));
    assert_eq!(ended(h).await, EndReason::Locked);
    assert!(approver.material.lock().unwrap().is_none());
    assert!(
        s.is_locked(),
        "lock() returned, so the vault must stay locked"
    );
    assert_eq!(s.high_water().await, None);
    assert_eq!(s.active_session(), None);
}

/// The header (read without a key) names this app, but the authenticated
/// manifest does not: opening refuses, the browser is denied, nothing unlocks.
#[tokio::test(flavor = "multi_thread")]
async fn open_time_binding_mismatch_denies() {
    let relay = relay().await;
    // A container really bound to another app ...
    let backend: SharedBackend = Arc::new(MemoryBackend::new());
    let (c, rk) = Container::create(backend.clone(), "other-app", ORIGIN)
        .await
        .unwrap();
    drop(c);
    let (_, crk) = Container::open_with_recovery(backend.clone(), &rk, None)
        .await
        .unwrap();
    // ... whose header is replaced by one that names `APP`.
    let (donor, _) = locked_backend().await;
    let (forged, _) = donor.get("header").await.unwrap().unwrap();
    let (_, etag) = backend.get("header").await.unwrap().unwrap();
    assert!(matches!(
        backend
            .put_if_match("header", &forged, &etag)
            .await
            .unwrap(),
        PutOutcome::Written(_)
    ));
    let approver = Unlocker::new(crk_material(crk), false);
    let s = locked_server(backend, approver.clone()).await;
    assert_eq!(s.binding(), (APP.to_string(), ORIGIN.to_string()));
    let (mut b, h) = pair(&s, &relay).await;
    let r = b
        .call(RequestBody::OpenSession {
            device_name: "A".into(),
        })
        .await;
    assert_eq!(r, ResponseBody::Denied);
    assert_eq!(ended(h).await, EndReason::UnlockFailed);
    assert_eq!(approver.seen().len(), 1);
    assert!(s.is_locked());
    assert_eq!(s.active_session(), None);
}

// ---------- host hooks: observer, manifest key, forced compaction ----------

#[derive(Default)]
struct Recorder {
    approved: Mutex<Vec<u64>>,
    marks: Mutex<Vec<vault_container::engine::HighWater>>,
}

impl vault_server::SessionObserver for Recorder {
    fn approved(&self, session: u64) {
        self.approved.lock().unwrap().push(session);
    }
    fn high_water(&self, hw: vault_container::engine::HighWater) {
        self.marks.lock().unwrap().push(hw);
    }
}

fn observed(rec: &Arc<Recorder>, base: ServerConfig) -> ServerConfig {
    ServerConfig {
        observer: Some(vault_server::Observer(rec.clone())),
        ..base
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn observer_sees_approval_and_every_new_manifest() {
    let relay = relay().await;
    let rec = Arc::new(Recorder::default());
    let s = server(Auto::new(true), observed(&rec, policy(2, 1.0, usize::MAX))).await;
    let (mut b, h) = open(&s, &relay, "A").await;
    assert_eq!(*rec.approved.lock().unwrap(), vec![h.id()]);
    commit_keys(&mut b, distinct(2)).await;
    {
        let marks = rec.marks.lock().unwrap();
        assert_eq!(marks.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1, 2]);
    }
    let hw = s.high_water().await.unwrap();
    assert_eq!(*rec.marks.lock().unwrap().last().unwrap(), hw);
    close(&mut b, h).await;
    // Compaction at session end is a new manifest too.
    let hw = s.high_water().await.unwrap();
    assert_eq!(hw.seq, 3);
    assert_eq!(*rec.marks.lock().unwrap().last().unwrap(), hw);
}

#[tokio::test(flavor = "multi_thread")]
async fn manifest_key_only_while_unlocked() {
    let (backend, crk) = locked_backend().await;
    let want = crk.subkeys().manifest;
    let s = VaultServer::new(
        Container::open_with_crk(backend.clone(), crk, None)
            .await
            .unwrap(),
        Auto::new(true),
        ServerConfig::default(),
    )
    .unwrap();
    assert_eq!(s.manifest_key().await.unwrap()[..], want[..]);
    s.lock().await;
    assert!(s.manifest_key().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_now_ignores_policy_but_not_sessions() {
    let relay = relay().await;
    let rec = Arc::new(Recorder::default());
    // A policy that never compacts on its own.
    let s = server(
        Auto::new(true),
        observed(&rec, policy(usize::MAX, 2.0, usize::MAX)),
    )
    .await;
    let (mut b, h) = open(&s, &relay, "A").await;
    commit_keys(&mut b, distinct(3)).await;
    assert_eq!(s.compact_now().await.unwrap(), None, "a session is active");
    close(&mut b, h).await;
    assert_eq!(s.container_stats().await.unwrap().segments, 3);
    let seq = s.compact_now().await.unwrap().expect("compacted");
    assert_eq!(s.container_stats().await.unwrap().segments, 1);
    assert_eq!(rec.marks.lock().unwrap().last().unwrap().seq, seq);
    s.lock().await;
    assert_eq!(s.compact_now().await.unwrap(), None, "locked");
}
