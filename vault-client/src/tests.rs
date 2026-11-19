// SPDX-License-Identifier: MIT
//! RemoteBackend and channel tests against a real in-process relay and vault.
//! They drive the crate-internal channel ([`Rpc`]) directly, so they are unit tests.

#[path = "../tests/common/mod.rs"]
mod common;

use crate::backend::RemoteBackend;
use crate::native::{TokioClock, TokioMailbox};
use crate::rpc::{Rpc, handshake, open_session};
use crate::{ClientError, Clock, Rng, SystemRng, mailbox_url};
use common::{Stack, eventually, run};
use futures::executor::{LocalPool, LocalSpawner};
use futures::task::LocalSpawnExt;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use vault_proto::noise::PairKeys;
use vault_proto::{RequestBody, ResponseBody};

/// Handshake and channel set-up (driver spawned), not yet approved.
async fn connect(st: &Stack, sp: &LocalSpawner) -> Result<Rc<Rpc>, ClientError> {
    let mut id = [0u8; 16];
    SystemRng.fill(&mut id);
    let keys = PairKeys::generate(st.origin(), id).unwrap();
    let qr = keys.offer().to_qr().unwrap();
    let t = TokioMailbox::open(&mailbox_url(&st.origin(), &id).unwrap()).await?;
    st.scan(&qr);
    let noise = handshake(&t, keys, TokioClock.sleep(10_000)).await?;
    let rpc = Rc::new(Rpc::new(Box::new(t), noise));
    let r = rpc.clone();
    sp.spawn_local(async move {
        r.drive().await;
    })
    .unwrap();
    Ok(rpc)
}

async fn open(st: &Stack, sp: &LocalSpawner) -> Result<RemoteBackend, ClientError> {
    let rpc = connect(st, sp).await?;
    open_session(&rpc, "test browser").await?;
    // The test plays the user who confirmed the code in the browser.
    rpc.set_approved();
    Ok(RemoteBackend::new(rpc))
}

#[test]
fn writes_are_buffered_until_commit_and_then_visible_to_a_new_session() {
    let st = Stack::new();
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    run(&mut pool, &st, async {
        let a = open(&st, &sp).await.unwrap();
        a.put(b"k1", b"v1").await.unwrap();
        a.put(b"k2", b"v2").await.unwrap();
        assert_eq!(a.get(b"k1").await.unwrap(), Some(b"v1".to_vec()));
        a.flush_commit().await.unwrap();
        // A later session (the vault serves one at a time) reads the committed data.
        let b = open(&st, &sp).await.unwrap();
        assert_eq!(b.get(b"k1").await.unwrap(), Some(b"v1".to_vec()));
        assert_eq!(b.get(b"nope").await.unwrap(), None);
        assert_eq!(
            b.scan_prefix(b"k").await.unwrap(),
            vec![
                (b"k1".to_vec(), b"v1".to_vec()),
                (b"k2".to_vec(), b"v2".to_vec())
            ]
        );
    });
}

#[test]
fn uncommitted_writes_are_lost_with_the_session() {
    let st = Stack::new();
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    run(&mut pool, &st, async {
        let a = open(&st, &sp).await.unwrap();
        a.put(b"kept", b"1").await.unwrap();
        a.flush_commit().await.unwrap();
        a.put(b"lost", b"2").await.unwrap();
        // The put_if_absent below is staged at the vault but never committed.
        assert!(a.put_if_absent(b"lost2", b"3").await.unwrap());
        drop(a);
        let b = open(&st, &sp).await.unwrap();
        assert_eq!(b.get(b"kept").await.unwrap(), Some(b"1".to_vec()));
        assert_eq!(b.get(b"lost").await.unwrap(), None);
        assert_eq!(b.get(b"lost2").await.unwrap(), None);
    });
}

#[test]
fn scan_merges_the_overlay_and_put_if_absent_is_correct() {
    let st = Stack::new();
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    run(&mut pool, &st, async {
        let a = open(&st, &sp).await.unwrap();
        a.put(b"p/a", b"1").await.unwrap();
        a.put(b"p/b", b"2").await.unwrap();
        a.put(b"q/x", b"9").await.unwrap();
        a.flush_commit().await.unwrap();
        drop(a);

        let b = open(&st, &sp).await.unwrap();
        // Unknown to the cache: the vault decides.
        assert!(!b.put_if_absent(b"p/a", b"new").await.unwrap());
        assert!(b.put_if_absent(b"p/c", b"3").await.unwrap());
        assert!(!b.put_if_absent(b"p/c", b"again").await.unwrap());
        b.delete(b"p/b").await.unwrap();
        b.put(b"p/d", b"4").await.unwrap();
        assert_eq!(
            b.scan_prefix(b"p/").await.unwrap(),
            vec![
                (b"p/a".to_vec(), b"1".to_vec()),
                (b"p/c".to_vec(), b"3".to_vec()),
                (b"p/d".to_vec(), b"4".to_vec()),
            ]
        );
        // Deleted keys may be re-created through put_if_absent.
        assert!(b.put_if_absent(b"p/b", b"2b").await.unwrap());
        b.flush_commit().await.unwrap();
        drop(b);

        let c = open(&st, &sp).await.unwrap();
        let all = c.scan_prefix(b"").await.unwrap();
        let keys: Vec<&[u8]> = all.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, [&b"p/a"[..], b"p/b", b"p/c", b"p/d", b"q/x"]);
        assert_eq!(c.get(b"p/b").await.unwrap(), Some(b"2b".to_vec()));
    });
}

#[test]
fn clear_removes_everything_after_commit() {
    let st = Stack::new();
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    run(&mut pool, &st, async {
        let a = open(&st, &sp).await.unwrap();
        a.put(b"old", b"1").await.unwrap();
        a.flush_commit().await.unwrap();
        a.clear().await.unwrap();
        assert_eq!(a.get(b"old").await.unwrap(), None);
        a.put(b"fresh", b"2").await.unwrap();
        assert_eq!(a.scan_prefix(b"").await.unwrap().len(), 1);
        a.flush_commit().await.unwrap();
        // The cache stays complete after the clear was sent.
        assert_eq!(a.get(b"old").await.unwrap(), None);
        drop(a);
        let b = open(&st, &sp).await.unwrap();
        assert_eq!(b.get(b"old").await.unwrap(), None);
        assert_eq!(b.get(b"fresh").await.unwrap(), Some(b"2".to_vec()));
    });
}

#[test]
fn a_failed_commit_kills_the_backend() {
    let st = Stack::new();
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    run(&mut pool, &st, async {
        let a = open(&st, &sp).await.unwrap();
        a.put(b"k", b"v").await.unwrap();
        st.knobs.fail.store(true, Ordering::SeqCst);
        assert!(matches!(
            a.flush_commit().await,
            Err(ClientError::CommitFailed(_))
        ));
        st.knobs.fail.store(false, Ordering::SeqCst);
        assert_eq!(a.sizes(), (0, 0), "cache and overlay dropped");
        assert!(a.get(b"k").await.is_err());
        assert!(a.put(b"k", b"v").await.is_err());
        drop(a);
        let b = open(&st, &sp).await.unwrap();
        assert_eq!(b.get(b"k").await.unwrap(), None);
    });
}

#[test]
fn requests_before_approval_not_sent() {
    let st = Stack::new();
    st.approver.hold.store(true, Ordering::SeqCst);
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    run(&mut pool, &st, async {
        let rpc = connect(&st, &sp).await.unwrap();
        // Waiting for the user: OpenSession is out (answer pending), store
        // requests are refused locally and never reach the wire.
        let r2 = rpc.clone();
        let opening = sp
            .spawn_local_with_handle(async move { open_session(&r2, "late").await })
            .unwrap();
        eventually("approval prompt", || {
            !st.approver.calls.lock().unwrap().is_empty()
        })
        .await;
        for body in [
            RequestBody::Get { key: b"k".to_vec() },
            RequestBody::Put {
                key: b"k".to_vec(),
                value: vec![],
            },
            RequestBody::Commit,
            RequestBody::ScanPrefix { prefix: vec![] },
            RequestBody::Clear,
        ] {
            assert!(matches!(
                rpc.call(body).await,
                Err(ClientError::Protocol(_))
            ));
        }
        // Ping is allowed before approval and answered.
        assert_eq!(rpc.call(RequestBody::Ping).await.unwrap(), ResponseBody::Ok);
        st.approver.hold.store(false, Ordering::SeqCst);
        opening.await.unwrap();
        // Vault approval alone does not open the channel locally.
        assert!(matches!(
            rpc.call(RequestBody::Get { key: b"k".to_vec() }).await,
            Err(ClientError::Protocol(_))
        ));
        rpc.set_approved();
        // The vault never saw a premature request: the session is still alive.
        assert_eq!(rpc.call(RequestBody::Ping).await.unwrap(), ResponseBody::Ok);
        let b = RemoteBackend::new(rpc);
        b.put(b"k", b"v").await.unwrap();
        b.flush_commit().await.unwrap();
    });
}

#[test]
fn denied_pairing_is_an_error() {
    let st = Stack::new();
    st.approver.answer.store(false, Ordering::SeqCst);
    let mut pool = LocalPool::new();
    let sp = pool.spawner();
    let r = run(&mut pool, &st, open(&st, &sp));
    assert!(matches!(r, Err(ClientError::Denied)));
}
