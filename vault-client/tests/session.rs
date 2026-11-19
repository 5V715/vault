// SPDX-License-Identifier: MIT
//! The pairing lifecycle ([`VaultSession`]) against an in-process relay and vault
//! server (tokio worker threads), with the single-threaded client on a
//! `LocalPool` and real time.

mod common;

use async_trait::async_trait;
use common::{Stack, TestApprover, eventually, run};
use futures::executor::LocalPool;
use futures::task::LocalSpawnExt;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use vault_client::native::{TokioClock, TokioFactory};
use vault_client::{
    ClientError, CloseReason, MailboxFactory, MailboxTransport, PING_INTERVAL_MS, SessionCallbacks,
    SessionOptions, SessionState, SystemRng, VaultSession,
};

/// Counts the frames the client side writes to any mailbox it opens.
#[derive(Clone, Default)]
struct Counting {
    sent: Rc<Cell<usize>>,
}

struct CountingMailbox {
    inner: <TokioFactory as MailboxFactory>::Mailbox,
    sent: Rc<Cell<usize>>,
}

#[async_trait(?Send)]
impl MailboxTransport for CountingMailbox {
    async fn send(&self, frame: &[u8]) -> Result<(), ClientError> {
        self.sent.set(self.sent.get() + 1);
        self.inner.send(frame).await
    }
    async fn recv(&self) -> Result<Option<Vec<u8>>, ClientError> {
        self.inner.recv().await
    }
    fn close(&self) {
        self.inner.close()
    }
}

#[async_trait(?Send)]
impl MailboxFactory for Counting {
    type Mailbox = CountingMailbox;
    async fn open_mailbox(&self, url: &str) -> Result<CountingMailbox, ClientError> {
        Ok(CountingMailbox {
            inner: TokioFactory.open_mailbox(url).await?,
            sent: self.sent.clone(),
        })
    }
}

struct V {
    pool: LocalPool,
    st: Stack,
    net: Counting,
}

fn venv() -> V {
    V {
        pool: LocalPool::new(),
        st: Stack::new(),
        net: Counting::default(),
    }
}

/// What one paired client reported.
#[derive(Clone, Default)]
struct Seen {
    states: Rc<RefCell<Vec<SessionState>>>,
    sas: Rc<RefCell<Vec<String>>>,
    offers: Rc<RefCell<Vec<String>>>,
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
    fn closed_text(&self) -> Option<String> {
        self.closed().map(|r| r.to_string())
    }
    fn callbacks(&self, on_offer: Rc<dyn Fn(&str)>) -> SessionCallbacks {
        let offers = self.offers.clone();
        let sas = self.sas.clone();
        let states = self.states.clone();
        SessionCallbacks::new(
            Rc::new(move |qr: &str| {
                offers.borrow_mut().push(qr.to_string());
                on_offer(qr);
            }),
            Rc::new(move |s: &str| sas.borrow_mut().push(s.to_string())),
            Rc::new(move |s| states.borrow_mut().push(s)),
        )
    }
}

impl V {
    fn frames_sent(&self) -> usize {
        self.net.sent.get()
    }

    fn start(&mut self, cb: SessionCallbacks, ping_ms: u64) -> VaultSession {
        let (session, fut) = VaultSession::pair_with(
            self.net.clone(),
            TokioClock,
            &SystemRng,
            &self.st.origin(),
            "My Browser",
            cb,
            SessionOptions {
                ping_interval_ms: ping_ms,
            },
        )
        .unwrap();
        self.pool.spawner().spawn_local(fut).unwrap();
        session
    }

    fn new_client(&mut self, ping_ms: u64) -> (VaultSession, Seen) {
        let seen = Seen::default();
        let rt = self.st.rt.handle().clone();
        let server = self.st.server.clone();
        let cb = seen.callbacks(Rc::new(move |qr: &str| {
            common::scan_with_handle(&rt, &server, qr);
        }));
        let s = self.start(cb, ping_ms);
        (s, seen)
    }

    /// The user clicks "Codes match" once the code is shown; wait for Active.
    fn confirm_and_wait_active(&mut self, c: &VaultSession, seen: &Seen) {
        let s = seen.clone();
        run(
            &mut self.pool,
            &self.st,
            eventually("sas shown", move || !s.sas.borrow().is_empty()),
        );
        c.confirm_sas().unwrap();
        let s = seen.clone();
        run(
            &mut self.pool,
            &self.st,
            eventually("active", move || s.has(&SessionState::Active)),
        );
    }
}

/// Sleep (real time) on the pool.
fn settle(v: &mut V, ms: u64) {
    run(&mut v.pool, &v.st, async move {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    });
}

/// The store answers (the session is usable).
fn store_works(v: &mut V, c: &VaultSession) -> bool {
    let Some(b) = c.backend() else {
        return false;
    };
    run(&mut v.pool, &v.st, b.get(b"k")).is_ok()
}

#[test]
fn nothing_is_accepted_before_approval_and_denial_closes() {
    let mut v = venv();
    v.st.approver.hold.store(true, Ordering::SeqCst);
    let (c, seen) = v.new_client(PING_INTERVAL_MS);
    let st = v.st.approver.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("prompt", move || !st.calls.lock().unwrap().is_empty()),
    );
    assert!(seen.has(&SessionState::AwaitingApproval));
    assert!(!seen.has(&SessionState::Active));
    assert!(c.backend().is_none());
    v.st.approver.answer.store(false, Ordering::SeqCst);
    v.st.approver.hold.store(false, Ordering::SeqCst);
    let s = seen.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("closed", move || s.closed().is_some()),
    );
    assert_eq!(seen.closed(), Some(CloseReason::Denied));
    assert_eq!(seen.closed_text().as_deref(), Some("denied"));
    assert!(c.backend().is_none());
}

#[test]
fn active_requires_local_sas_confirmation() {
    let mut v = venv();
    let (c, seen) = v.new_client(PING_INTERVAL_MS);
    let st = v.st.approver.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("vault approved", move || {
            !st.calls.lock().unwrap().is_empty()
        }),
    );
    let sent = v.frames_sent();
    // The vault answered Approved; without the local "Codes match" nothing
    // opens: no Active, no store, no data request on the wire.
    settle(&mut v, 400);
    assert!(seen.has(&SessionState::AwaitingApproval));
    assert!(!seen.has(&SessionState::Active));
    assert_eq!(v.frames_sent(), sent, "nothing sent");
    assert!(c.backend().is_none());
    assert_eq!(c.state(), SessionState::AwaitingApproval);
    // Confirming opens the session.
    c.confirm_sas().unwrap();
    let s = seen.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("active", move || s.has(&SessionState::Active)),
    );
    assert!(store_works(&mut v, &c));
    assert!(v.frames_sent() > sent);
}

#[test]
fn confirmation_before_vault_approval_also_works() {
    let mut v = venv();
    v.st.approver.hold.store(true, Ordering::SeqCst);
    let (c, seen) = v.new_client(PING_INTERVAL_MS);
    let s = seen.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("sas shown", move || !s.sas.borrow().is_empty()),
    );
    c.confirm_sas().unwrap();
    settle(&mut v, 200);
    assert!(
        !seen.has(&SessionState::Active),
        "the vault has not approved yet"
    );
    v.st.approver.hold.store(false, Ordering::SeqCst);
    let s = seen.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("active", move || s.has(&SessionState::Active)),
    );
}

#[test]
fn confirm_before_code_shown_is_refused() {
    let mut v = venv();
    let seen = Seen::default();
    let cb = seen.callbacks(Rc::new(|_| {}));
    let c = v.start(cb, PING_INTERVAL_MS);
    // Nobody scans: no handshake, no code.
    assert!(c.confirm_sas().is_err());
    run(&mut v.pool, &v.st, c.lock());
    assert_eq!(seen.closed(), Some(CloseReason::Cancelled));
}

#[test]
fn cancel_aborts_with_denied() {
    for vault_approved_first in [true, false] {
        let mut v = venv();
        v.st.approver
            .hold
            .store(!vault_approved_first, Ordering::SeqCst);
        let (c, seen) = v.new_client(PING_INTERVAL_MS);
        let st = v.st.approver.clone();
        run(
            &mut v.pool,
            &v.st,
            eventually("prompt", move || !st.calls.lock().unwrap().is_empty()),
        );
        let sent = v.frames_sent();
        c.cancel_pairing();
        let s = seen.clone();
        run(
            &mut v.pool,
            &v.st,
            eventually("closed", move || s.closed().is_some()),
        );
        assert_eq!(seen.closed(), Some(CloseReason::Denied));
        assert!(!seen.has(&SessionState::Active));
        // At most the goodbye went out; never a data request.
        assert!(v.frames_sent() <= sent + 1);
        assert!(
            c.confirm_sas().is_err(),
            "a cancelled session stays cancelled"
        );
        v.st.approver.hold.store(false, Ordering::SeqCst);
        let server = v.st.server.clone();
        run(
            &mut v.pool,
            &v.st,
            eventually("vault ended the session", move || {
                server.active_session().is_none() && !server.session_log().is_empty()
            }),
        );
    }
}

#[test]
fn impostor_vault_with_qr_text_cannot_reach_active_without_confirmation() {
    let mut v = venv();
    // The attacker read the QR text and runs its own vault, which approves
    // itself. The user's real vault never gets the mailbox.
    let impostor_approver = Arc::new(TestApprover::new(true));
    let impostor = common::vault_server(&v.st.rt, impostor_approver.clone());
    let seen = Seen::default();
    let cb = {
        let rt = v.st.rt.handle().clone();
        let impostor = impostor.clone();
        seen.callbacks(Rc::new(move |qr: &str| {
            common::scan_with_handle(&rt, &impostor, qr);
        }))
    };
    let c = v.start(cb, PING_INTERVAL_MS);
    let a = impostor_approver.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("impostor approved", move || {
            !a.calls.lock().unwrap().is_empty()
        }),
    );
    let sent = v.frames_sent();
    settle(&mut v, 400);
    // The browser shows the impostor's code but goes nowhere without the user.
    assert!(!seen.has(&SessionState::Active));
    assert_eq!(v.frames_sent(), sent);
    assert!(c.backend().is_none());
    // The user's phone showed no code, so the user cancels.
    c.cancel_pairing();
    let s = seen.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("closed", move || s.closed().is_some()),
    );
    assert_eq!(seen.closed(), Some(CloseReason::Denied));
    assert!(!seen.has(&SessionState::Active));
}

#[test]
fn ping_keeps_session_alive() {
    // The relay closes mailboxes idle for 1 s; pings every 300 ms keep ours open.
    let mut v = V {
        pool: LocalPool::new(),
        st: Stack::with_idle(std::time::Duration::from_secs(1)),
        net: Counting::default(),
    };
    let (c, seen) = v.new_client(300);
    v.confirm_and_wait_active(&c, &seen);
    run(&mut v.pool, &v.st, async {
        tokio::time::sleep(std::time::Duration::from_millis(2600)).await;
    });
    assert_eq!(seen.closed(), None, "session survived 2.6 s idle");
    assert!(store_works(&mut v, &c));

    // Control: at the default interval (30 s, longer than this relay's 1 s idle
    // timeout) the same relay ends the session.
    let (c2, seen2) = v.new_client(PING_INTERVAL_MS);
    v.confirm_and_wait_active(&c2, &seen2);
    let s = seen2.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("idle close", move || s.closed().is_some()),
    );
    assert_eq!(seen2.closed(), Some(CloseReason::Disconnected));
    assert_eq!(seen2.closed_text().as_deref(), Some("disconnected"));
    assert!(c2.backend().is_none());
}

// ---------- new in vault-client: the host hooks used by an embedding app ----------

#[test]
fn lock_discards_the_backend_and_reports_locked() {
    let mut v = venv();
    let (c, seen) = v.new_client(PING_INTERVAL_MS);
    v.confirm_and_wait_active(&c, &seen);
    let b = c.backend().unwrap();
    run(&mut v.pool, &v.st, async {
        b.put(b"k", b"v").await.unwrap();
        b.get(b"k").await.unwrap();
    });
    assert!(b.sizes().0 > 0);
    run(&mut v.pool, &v.st, c.lock());
    assert_eq!(seen.closed(), Some(CloseReason::Locked));
    assert_eq!(seen.closed_text().as_deref(), Some("locked"));
    assert_eq!(b.sizes(), (0, 0), "cache and overlay dropped");
    assert!(run(&mut v.pool, &v.st, b.get(b"k")).is_err());
    assert!(c.backend().is_none());
    assert!(c.confirm_sas().is_err(), "no pairing in progress");
}

#[test]
fn activate_runs_before_active_and_deactivate_before_closed() {
    let mut v = venv();
    let seen = Seen::default();
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let rt = v.st.rt.handle().clone();
    let server = v.st.server.clone();
    let cb = seen
        .callbacks(Rc::new(move |qr: &str| {
            common::scan_with_handle(&rt, &server, qr);
        }))
        .with_activate({
            let (log, states) = (log.clone(), seen.states.clone());
            Rc::new(move |backend: vault_client::RemoteBackend| {
                let (log, states) = (log.clone(), states.clone());
                Box::pin(async move {
                    assert!(!states.borrow().contains(&SessionState::Active));
                    // The host may use the store before Active is reported.
                    backend.get(b"account").await.map_err(|e| e.to_string())?;
                    log.borrow_mut().push("activate".into());
                    Ok(())
                })
            })
        })
        .with_deactivate({
            let (log, states) = (log.clone(), seen.states.clone());
            Rc::new(move || {
                let (log, states) = (log.clone(), states.clone());
                Box::pin(async move {
                    assert!(
                        states
                            .borrow()
                            .iter()
                            .all(|s| !matches!(s, SessionState::Closed(_)))
                    );
                    log.borrow_mut().push("deactivate".into());
                })
            })
        });
    let c = v.start(cb, PING_INTERVAL_MS);
    v.confirm_and_wait_active(&c, &seen);
    assert_eq!(*log.borrow(), ["activate"]);
    run(&mut v.pool, &v.st, c.lock());
    assert_eq!(*log.borrow(), ["activate", "deactivate"]);
    assert_eq!(seen.closed(), Some(CloseReason::Locked));
}

#[test]
fn failed_activation_closes_with_the_hosts_reason() {
    let mut v = venv();
    let seen = Seen::default();
    let rt = v.st.rt.handle().clone();
    let server = v.st.server.clone();
    let cb = seen
        .callbacks(Rc::new(move |qr: &str| {
            common::scan_with_handle(&rt, &server, qr);
        }))
        .with_activate(Rc::new(|_backend| {
            Box::pin(async { Err("stored account unusable: bad".to_string()) })
        }));
    let c = v.start(cb, PING_INTERVAL_MS);
    let s = seen.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("sas shown", move || !s.sas.borrow().is_empty()),
    );
    c.confirm_sas().unwrap();
    let s = seen.clone();
    run(
        &mut v.pool,
        &v.st,
        eventually("closed", move || s.closed().is_some()),
    );
    assert!(!seen.has(&SessionState::Active));
    assert_eq!(
        seen.closed_text().as_deref(),
        Some("stored account unusable: bad")
    );
    assert!(c.backend().is_none());
}

#[test]
fn pair_rejects_bad_device_names_and_relay_urls() {
    let bad = |origin: &str, name: &str| {
        VaultSession::pair(
            TokioFactory,
            TokioClock,
            &SystemRng,
            origin,
            name,
            SessionCallbacks::new(Rc::new(|_| {}), Rc::new(|_| {}), Rc::new(|_| {})),
        )
        .err()
        .map(|e| e.to_string())
    };
    assert_eq!(
        bad("http://127.0.0.1:1", "a\nb").as_deref(),
        Some("protocol error: device name too long or with control characters")
    );
    assert!(bad("http://127.0.0.1:1", &"x".repeat(65)).is_some());
    assert!(bad("ftp://relay.example", "ok").is_some());
    assert!(bad("http://127.0.0.1:1", "ok").is_none());
}

fn pair_with_ping(ping_interval_ms: u64) -> Result<VaultSession, ClientError> {
    VaultSession::pair_with(
        TokioFactory,
        TokioClock,
        &SystemRng,
        "http://127.0.0.1:1",
        "ok",
        SessionCallbacks::new(Rc::new(|_| {}), Rc::new(|_| {}), Rc::new(|_| {})),
        SessionOptions { ping_interval_ms },
    )
    .map(|(s, _fut)| s)
}

#[test]
fn ping_interval_above_protocol_max_rejected() {
    // The protocol requires a ping at least every 30 s.
    for ms in [PING_INTERVAL_MS + 1, 60_000, u64::MAX] {
        assert!(
            matches!(pair_with_ping(ms), Err(ClientError::Protocol(_))),
            "{ms} ms accepted"
        );
    }
    assert!(pair_with_ping(PING_INTERVAL_MS).is_ok());
    assert_eq!(SessionOptions::default().ping_interval_ms, PING_INTERVAL_MS);
}

#[test]
fn ping_interval_zero_rejected() {
    assert!(matches!(pair_with_ping(0), Err(ClientError::Protocol(_))));
    assert!(pair_with_ping(1).is_ok());
}
