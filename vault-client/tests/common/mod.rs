// SPDX-License-Identifier: MIT
//! Native test harness: an in-process relay mailbox and vault server (on tokio
//! worker threads) and helpers to run the single-threaded client side on a
//! `LocalPool` with the runtime entered.
//!
//! Shared by the integration tests and (via `#[path]`) the crate's unit tests, so
//! it must not name `vault_client` or `crate`.
#![allow(dead_code)]

use futures::executor::LocalPool;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vault_container::Result as CResult;
use vault_container::backend::{ContainerBackend, Etag, MemoryBackend, PutOutcome};
use vault_container::engine::Container;
use vault_proto::PairOffer;
use vault_server::{Approver, ServerConfig, SharedBackend, VaultServer};
use zeroize::Zeroizing;

/// Container backend whose writes can be held back or made to fail.
#[derive(Default)]
pub struct Knobs {
    inner: MemoryBackend,
    pub hold: AtomicBool,
    pub fail: AtomicBool,
    pub writes_started: AtomicUsize,
}

impl Knobs {
    async fn gate(&self) -> CResult<()> {
        self.writes_started.fetch_add(1, Ordering::SeqCst);
        while self.hold.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(vault_container::Error::Io("injected".into()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ContainerBackend for Knobs {
    async fn get(&self, name: &str) -> CResult<Option<(Vec<u8>, Etag)>> {
        self.inner.get(name).await
    }
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> CResult<PutOutcome> {
        self.gate().await?;
        self.inner.put_if_absent(name, data).await
    }
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> CResult<PutOutcome> {
        self.gate().await?;
        self.inner.put_if_match(name, data, etag).await
    }
    async fn delete(&self, name: &str) -> CResult<()> {
        self.inner.delete(name).await
    }
    async fn list(&self, prefix: &str) -> CResult<Vec<String>> {
        self.inner.list(prefix).await
    }
}

/// Test approver: answers per `answer`, optionally after `hold` is released.
pub struct TestApprover {
    pub answer: AtomicBool,
    pub hold: AtomicBool,
    pub calls: StdMutex<Vec<(String, String)>>,
}

impl TestApprover {
    pub fn new(answer: bool) -> Self {
        Self {
            answer: AtomicBool::new(answer),
            hold: AtomicBool::new(false),
            calls: StdMutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl Approver for TestApprover {
    async fn approve(&self, device_name: &str, sas: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .push((device_name.into(), sas.into()));
        while self.hold.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.answer.load(Ordering::SeqCst)
    }
}

/// A vault server on a fresh in-memory container.
pub fn vault_server(rt: &tokio::runtime::Runtime, approver: Arc<TestApprover>) -> VaultServer {
    rt.block_on(async move {
        let backend: SharedBackend = Arc::new(MemoryBackend::default());
        let (c, _rk) = Container::create(backend).await.unwrap();
        VaultServer::new(
            c,
            Zeroizing::new([7u8; 32]),
            approver,
            ServerConfig::default(),
        )
        .unwrap()
    })
}

/// Relay mailbox + vault server on a tokio runtime.
pub struct Stack {
    pub rt: tokio::runtime::Runtime,
    pub addr: SocketAddr,
    pub server: VaultServer,
    pub approver: Arc<TestApprover>,
    pub knobs: Arc<Knobs>,
    _stop: oneshot::Sender<()>,
}

impl Stack {
    pub fn new() -> Self {
        Self::with_idle(Duration::from_secs(120))
    }

    /// `relay_idle`: the relay closes mailboxes idle for longer than this.
    pub fn with_idle(relay_idle: Duration) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (addr, stop) = rt.block_on(async {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let (tx, rx) = oneshot::channel::<()>();
            let cfg = vault_mailbox::MailboxConfig {
                idle_timeout: relay_idle,
                ..vault_mailbox::MailboxConfig::default()
            };
            let mailboxes = vault_mailbox::Mailboxes::new(cfg);
            tokio::spawn(vault_mailbox::serve(l, mailboxes, async move {
                let _ = rx.await;
            }));
            (addr, tx)
        });
        let knobs = Arc::new(Knobs::default());
        let approver = Arc::new(TestApprover::new(true));
        let server = {
            let backend: SharedBackend = knobs.clone();
            let appr = approver.clone();
            rt.block_on(async move {
                let (c, _rk) = Container::create(backend).await.unwrap();
                VaultServer::new(c, Zeroizing::new([42u8; 32]), appr, ServerConfig::default())
                    .unwrap()
            })
        };
        Self {
            rt,
            addr,
            server,
            approver,
            knobs,
            _stop: stop,
        }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The phone scans the QR: dial the mailbox and serve the session.
    pub fn scan(&self, qr: &str) {
        scan_with_handle(self.rt.handle(), &self.server, qr);
    }
}

/// `server` scans `qr`: dial the mailbox and serve the session.
pub fn scan_with_handle(rt: &tokio::runtime::Handle, server: &VaultServer, qr: &str) {
    let offer = PairOffer::from_qr(qr).expect("valid offer");
    let server = server.clone();
    rt.spawn(async move {
        if let Ok(h) = server.serve_offer(offer).await {
            let _ = h.finished().await;
        }
    });
}

/// Run `f` on the single-threaded pool with the tokio runtime entered.
pub fn run<F: Future>(pool: &mut LocalPool, st: &Stack, f: F) -> F::Output {
    let _g = st.rt.enter();
    pool.run_until(f)
}

/// Wait (real time) until `cond` holds; panics after 10 s.
pub async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(start.elapsed() < Duration::from_secs(10), "timeout: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
