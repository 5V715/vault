// SPDX-License-Identifier: MIT

//! Engine-level tests: commits, crash consistency, anti-rollback, compaction, GC.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use vault_container::backend::{ContainerBackend, Etag, LocalBackend, MemoryBackend, PutOutcome};
use vault_container::engine::{Container, HighWater};
use vault_container::{Error, Result};

fn local() -> LocalBackend {
    LocalBackend::open_blocking(tempfile::tempdir().unwrap().keep()).unwrap()
}

/// Test-only wrapper that injects faults into the wrapped backend.
#[derive(Clone)]
struct Faulty<B> {
    inner: B,
    /// When set, every manifest `put_if_match` fails with an I/O error (nothing written).
    fail_manifest: Arc<AtomicBool>,
    /// Number of upcoming segment `put_if_absent` calls to answer with `Conflict`.
    seg_conflicts: Arc<AtomicUsize>,
    /// Counts segment `put_if_absent` calls that reached the inner backend.
    seg_puts: Arc<AtomicUsize>,
}

impl<B> Faulty<B> {
    fn new(inner: B) -> Self {
        Self {
            inner,
            fail_manifest: Arc::default(),
            seg_conflicts: Arc::default(),
            seg_puts: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl<B: ContainerBackend> ContainerBackend for Faulty<B> {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        self.inner.get(name).await
    }
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        if name.starts_with("seg/") {
            // Decrement-if-positive without the `fetch_update`/`try_update`
            // naming churn across toolchains (deprecated on newer stable).
            let mut cur = self.seg_conflicts.load(Ordering::SeqCst);
            let mut took = false;
            while cur > 0 {
                match self.seg_conflicts.compare_exchange(
                    cur,
                    cur - 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => {
                        took = true;
                        break;
                    }
                    Err(actual) => cur = actual,
                }
            }
            if took {
                return Ok(PutOutcome::Conflict);
            }
            self.seg_puts.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.put_if_absent(name, data).await
    }
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        if name == "manifest" && self.fail_manifest.load(Ordering::SeqCst) {
            return Err(Error::Io("injected crash before manifest write".into()));
        }
        self.inner.put_if_match(name, data, etag).await
    }
    async fn delete(&self, name: &str) -> Result<()> {
        self.inner.delete(name).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list(prefix).await
    }
}

fn seg_name(id: &[u8; 16]) -> String {
    format!("seg/{}", data_encoding::HEXLOWER.encode(id))
}

async fn live_seg_names<B: ContainerBackend>(c: &Container<B>) -> Vec<String> {
    let mut v: Vec<String> = c.segment_ids().iter().map(seg_name).collect();
    v.sort();
    v
}

fn kv(k: &str, v: &str) -> (Vec<u8>, Vec<u8>) {
    (k.as_bytes().to_vec(), v.as_bytes().to_vec())
}

#[tokio::test]
async fn crud_and_scan_roundtrip() {
    let (mut c, _rk) = Container::create(MemoryBackend::new()).await.unwrap();
    assert_eq!(c.seq(), 0);
    c.put(b"a/1", b"one").unwrap();
    c.put(b"a/2", b"two").unwrap();
    c.put(b"b/1", b"bee").unwrap();
    assert_eq!(c.get(b"a/1").as_deref(), Some(&b"one"[..]));
    assert!(!c.put_if_absent(b"a/1", b"nope").unwrap());
    assert!(c.put_if_absent(b"a/3", b"three").unwrap());
    c.delete(b"a/2");
    assert_eq!(c.get(b"a/2"), None);
    assert_eq!(
        c.scan_prefix(b"a/"),
        vec![kv("a/1", "one"), kv("a/3", "three")]
    );

    assert_eq!(c.commit().await.unwrap(), 1);
    assert_eq!(c.pending_len(), 0);
    assert_eq!(
        c.scan_prefix(b"a/"),
        vec![kv("a/1", "one"), kv("a/3", "three")]
    );
    // put_if_absent sees committed state too; overwrite and delete of committed keys
    assert!(!c.put_if_absent(b"b/1", b"x").unwrap());
    c.put(b"a/1", b"uno").unwrap();
    c.delete(b"b/1");
    assert!(c.put_if_absent(b"b/1", b"again").unwrap());
    assert_eq!(c.commit().await.unwrap(), 2);
    assert_eq!(c.get(b"a/1").as_deref(), Some(&b"uno"[..]));
    assert_eq!(c.get(b"b/1").as_deref(), Some(&b"again"[..]));
    // empty commit is a no-op
    assert_eq!(c.commit().await.unwrap(), 2);
}

async fn crash_case<B: ContainerBackend + Clone>(backend: B) {
    let f = Faulty::new(backend.clone());
    let (mut c, rk) = Container::create(f.clone()).await.unwrap();
    c.put(b"k", b"committed").unwrap();
    c.commit().await.unwrap();
    let hw = c.high_water();

    c.put(b"k", b"lost").unwrap();
    c.put(b"other", b"lost").unwrap();
    f.fail_manifest.store(true, Ordering::SeqCst);
    let puts_before = f.seg_puts.load(Ordering::SeqCst);
    assert!(matches!(c.commit().await, Err(Error::Io(_))));
    // the segment write happened, only the manifest switch did not
    assert_eq!(f.seg_puts.load(Ordering::SeqCst), puts_before + 1);
    drop(c); // crash

    let (c, _) = Container::open_with_recovery(backend.clone(), &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c.high_water(), hw);
    assert_eq!(c.get(b"k").as_deref(), Some(&b"committed"[..]));
    assert_eq!(c.get(b"other"), None);
    assert_eq!(
        backend.list("seg/").await.unwrap().len(),
        2,
        "orphan remains"
    );
}

#[tokio::test]
async fn crash_before_manifest_commit_recovers_last_state() {
    crash_case(MemoryBackend::new()).await;
}

#[tokio::test]
async fn crash_before_manifest_commit_recovers_last_state_local() {
    crash_case(local()).await;
}

#[tokio::test]
async fn reopen_replays_to_same_state() {
    let backend = local();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    for round in 0..5u32 {
        for i in 0..20u32 {
            c.put(
                format!("k{i:02}").as_bytes(),
                format!("r{round}-{i}").as_bytes(),
            )
            .unwrap();
        }
        c.delete(format!("k{round:02}").as_bytes());
        c.commit().await.unwrap();
    }
    c.put(b"big", &vec![7u8; 200 * 1024]).unwrap();
    c.commit().await.unwrap();
    let state = c.scan_prefix(b"");
    let hw = c.high_water();
    // every round re-puts all keys, so only the last round's delete sticks
    assert_eq!(state.len(), 20);
    assert_eq!(c.get(b"k04"), None);
    assert_eq!(c.get(b"k00").as_deref(), Some(&b"r4-0"[..]));
    drop(c);

    let (c1, crk) = Container::open_with_recovery(backend.clone(), &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c1.scan_prefix(b""), state);
    assert_eq!(c1.high_water(), hw);
    drop(c1);
    let c2 = Container::open_with_crk(backend, crk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c2.scan_prefix(b""), state);
}

#[tokio::test]
async fn compaction_preserves_state_and_drops_old_segments() {
    let backend = local();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    for round in 0..4u32 {
        c.put(b"same", format!("v{round}").as_bytes()).unwrap();
        c.put(format!("only{round}").as_bytes(), b"x").unwrap();
        c.commit().await.unwrap();
    }
    c.delete(b"only0");
    c.commit().await.unwrap();
    let old = backend.list("seg/").await.unwrap();
    assert_eq!(old.len(), 5);
    // a pending change is folded into the compaction
    c.put(b"pending", b"p").unwrap();
    let state = c.scan_prefix(b"");

    let seq = c.compact().await.unwrap();
    assert_eq!(seq, 6);
    assert_eq!(c.pending_len(), 0);
    assert_eq!(c.scan_prefix(b""), state);
    let now = backend.list("seg/").await.unwrap();
    assert_eq!(now, live_seg_names(&c).await);
    assert_eq!(now.len(), 1);
    assert!(old.iter().all(|o| !now.contains(o)));

    let hw = c.high_water();
    drop(c);
    let (c, _) = Container::open_with_recovery(backend, &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c.scan_prefix(b""), state);
}

#[tokio::test]
async fn stats_track_live_and_dead_bytes() {
    let backend = MemoryBackend::new();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    let st = c.stats();
    assert_eq!((st.segments, st.stored_bytes, st.live_bytes), (0, 0, 0));
    c.put(b"k", &[1u8; 1000]).unwrap();
    c.commit().await.unwrap();
    let one = c.stats();
    assert_eq!(one.segments, 1);
    assert!(one.stored_bytes >= 1000);
    assert_eq!(one.stored_bytes, one.live_bytes, "nothing dead yet");
    assert_eq!(one.dead_ratio(), 0.0);
    // Overwriting the same key three times: three quarters are dead.
    for v in 2..5u8 {
        c.put(b"k", &[v; 1000]).unwrap();
        c.commit().await.unwrap();
    }
    let st = c.stats();
    assert_eq!(st.segments, 4);
    assert_eq!(st.stored_bytes, 4 * one.stored_bytes);
    assert_eq!(st.live_bytes, one.live_bytes);
    assert!((st.dead_ratio() - 0.75).abs() < 1e-9);
    // Pending changes are not counted.
    c.put(b"other", b"x").unwrap();
    assert_eq!(c.stats(), st);
    c.discard_pending();
    // Reopening counts the same.
    let hw = c.high_water();
    let (mut c2, _) = Container::open_with_recovery(backend.clone(), &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c2.stats(), st);
    c2.compact().await.unwrap();
    let after = c2.stats();
    assert_eq!(after.segments, 1);
    assert_eq!(after.stored_bytes, after.live_bytes);
    assert_eq!(after.live_bytes, one.live_bytes);
    // A delete of everything leaves only dead bytes.
    c2.delete(b"k");
    c2.commit().await.unwrap();
    let gone = c2.stats();
    assert_eq!(gone.live_bytes, 0);
    assert_eq!(gone.dead_ratio(), 1.0);
}

#[tokio::test]
async fn compaction_of_empty_container() {
    let backend = MemoryBackend::new();
    let (mut c, _) = Container::create(backend.clone()).await.unwrap();
    c.put(b"k", b"v").unwrap();
    c.commit().await.unwrap();
    c.delete(b"k");
    c.commit().await.unwrap();
    c.compact().await.unwrap();
    assert!(c.segment_ids().is_empty());
    assert!(backend.list("seg/").await.unwrap().is_empty());
    assert!(c.scan_prefix(b"").is_empty());
}

#[tokio::test]
async fn orphan_segments_garbage_collected() {
    let backend = MemoryBackend::new();
    let f = Faulty::new(backend.clone());
    let (mut c, rk) = Container::create(f.clone()).await.unwrap();
    c.put(b"keep", b"1").unwrap();
    c.commit().await.unwrap();
    c.put(b"orphaned", b"2").unwrap();
    f.fail_manifest.store(true, Ordering::SeqCst);
    assert!(c.commit().await.is_err());
    drop(c);

    let (mut c, _) = Container::open_with_recovery(backend.clone(), &rk, None)
        .await
        .unwrap();
    assert_eq!(backend.list("seg/").await.unwrap().len(), 2);
    assert_eq!(c.gc_orphans().await.unwrap(), 1);
    assert_eq!(
        backend.list("seg/").await.unwrap(),
        live_seg_names(&c).await
    );
    assert_eq!(c.gc_orphans().await.unwrap(), 0);
    let hw = c.high_water();
    drop(c);
    let (c, _) = Container::open_with_recovery(backend, &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c.get(b"keep").as_deref(), Some(&b"1"[..]));
}

#[tokio::test]
async fn gc_refuses_when_manifest_moved() {
    let backend = MemoryBackend::new();
    let (mut a, rk) = Container::create(backend.clone()).await.unwrap();
    let (mut b, _) = Container::open_with_recovery(backend.clone(), &rk, None)
        .await
        .unwrap();
    b.put(b"from-b", b"1").unwrap();
    b.commit().await.unwrap();
    // a's view is stale: b's segment looks like an orphan to a, but must survive
    assert!(matches!(a.gc_orphans().await, Err(Error::Conflict)));
    assert_eq!(
        backend.list("seg/").await.unwrap(),
        live_seg_names(&b).await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_manifest_commit_conflicts() {
    let backend = local();
    let (c, rk) = Container::create(backend.clone()).await.unwrap();
    drop(c);
    for round in 0..5u32 {
        let (mut a, _) = Container::open_with_recovery(backend.clone(), &rk, None)
            .await
            .unwrap();
        let (mut b, _) = Container::open_with_recovery(backend.clone(), &rk, None)
            .await
            .unwrap();
        a.put(b"who", format!("a{round}").as_bytes()).unwrap();
        b.put(b"who", format!("b{round}").as_bytes()).unwrap();
        let ta = tokio::spawn(async move {
            let r = a.commit().await;
            (a, r)
        });
        let tb = tokio::spawn(async move {
            let r = b.commit().await;
            (b, r)
        });
        let (a, ra) = ta.await.unwrap();
        let (b, rb) = tb.await.unwrap();
        let ok: Vec<_> = [&ra, &rb].into_iter().filter(|r| r.is_ok()).collect();
        assert_eq!(ok.len(), 1, "round {round}: exactly one commit wins");
        let (winner, loser_r) = if ra.is_ok() { (&a, &rb) } else { (&b, &ra) };
        assert!(matches!(loser_r, Err(Error::Conflict)));
        // the loser's segment was cleaned up: only the winner's segments remain
        assert_eq!(
            backend.list("seg/").await.unwrap(),
            live_seg_names(winner).await
        );
        let expect = winner.get(b"who");
        let hw = winner.high_water();
        drop((a, b));
        let (c, _) = Container::open_with_recovery(backend.clone(), &rk, Some(hw))
            .await
            .unwrap();
        assert_eq!(c.get(b"who"), expect);
    }
}

#[tokio::test]
async fn reopen_with_high_water_rejects_rollback() {
    let backend = MemoryBackend::new();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    c.put(b"k", b"1").unwrap();
    c.commit().await.unwrap();
    let hw1 = c.high_water();
    let (old_manifest, _) = backend.get("manifest").await.unwrap().unwrap();
    c.put(b"k", b"2").unwrap();
    c.commit().await.unwrap();
    let hw2 = c.high_water();
    assert_eq!(hw2.seq, 2);
    drop(c);

    // An older high-water mark is fine: the store moved forward.
    let (c, _) = Container::open_with_recovery(backend.clone(), &rk, Some(hw1))
        .await
        .unwrap();
    assert_eq!(c.high_water(), hw2);
    drop(c);

    // A fork: same seq, different hash.
    let forked = HighWater {
        seq: hw2.seq,
        hash: [0xAA; 32],
    };
    assert!(matches!(
        Container::open_with_recovery(backend.clone(), &rk, Some(forked)).await,
        Err(Error::Rollback)
    ));

    // The provider serves the old (validly sealed) manifest again.
    let (_, etag) = backend.get("manifest").await.unwrap().unwrap();
    backend
        .put_if_match("manifest", &old_manifest, &etag)
        .await
        .unwrap();
    assert!(matches!(
        Container::open_with_recovery(backend.clone(), &rk, Some(hw2)).await,
        Err(Error::Rollback)
    ));
    // without a high-water mark the old state is all we can see
    let (c, _) = Container::open_with_recovery(backend, &rk, None)
        .await
        .unwrap();
    assert_eq!(c.get(b"k").as_deref(), Some(&b"1"[..]));
}

#[tokio::test]
async fn reload_rejects_rollback() {
    let backend = MemoryBackend::new();
    let (mut c, _) = Container::create(backend.clone()).await.unwrap();
    let (genesis, _) = backend.get("manifest").await.unwrap().unwrap();
    c.put(b"k", b"1").unwrap();
    c.commit().await.unwrap();
    let (_, etag) = backend.get("manifest").await.unwrap().unwrap();
    backend
        .put_if_match("manifest", &genesis, &etag)
        .await
        .unwrap();
    assert!(matches!(c.reload().await, Err(Error::Rollback)));
    // state unchanged after the refused reload
    assert_eq!(c.get(b"k").as_deref(), Some(&b"1"[..]));
    assert_eq!(c.seq(), 1);
}

#[tokio::test]
async fn commit_conflict_keeps_pending() {
    let backend = MemoryBackend::new();
    let (mut a, rk) = Container::create(backend.clone()).await.unwrap();
    let (mut b, _) = Container::open_with_recovery(backend.clone(), &rk, None)
        .await
        .unwrap();
    a.put(b"a", b"from-a").unwrap();
    a.commit().await.unwrap();

    b.put(b"b", b"from-b").unwrap();
    b.delete(b"gone");
    assert!(matches!(b.commit().await, Err(Error::Conflict)));
    assert_eq!(b.pending_len(), 2);
    assert_eq!(b.get(b"b").as_deref(), Some(&b"from-b"[..]));
    assert_eq!(b.seq(), 0);
    // the failed attempt left no orphan behind
    assert_eq!(
        backend.list("seg/").await.unwrap(),
        live_seg_names(&a).await
    );
    // retrying without reloading still conflicts and keeps the batch
    assert!(matches!(b.commit().await, Err(Error::Conflict)));
    assert_eq!(b.pending_len(), 2);

    b.reload().await.unwrap();
    assert_eq!(b.get(b"a").as_deref(), Some(&b"from-a"[..]));
    assert_eq!(b.get(b"b").as_deref(), Some(&b"from-b"[..]));
    assert_eq!(b.commit().await.unwrap(), 2);

    b.put(b"c", b"x").unwrap();
    b.discard_pending();
    assert_eq!(b.get(b"c"), None);
    assert_eq!(b.pending_len(), 0);
}

#[tokio::test]
async fn delete_then_reopen() {
    let backend = local();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    c.put(b"k", b"v").unwrap();
    c.put(b"k2", b"v2").unwrap();
    c.commit().await.unwrap();
    c.delete(b"k");
    c.commit().await.unwrap();
    // re-put after delete in a later segment, then delete in the same batch
    c.put(b"k2", b"v3").unwrap();
    c.delete(b"k2");
    c.commit().await.unwrap();
    let hw = c.high_water();
    drop(c);
    let (c, _) = Container::open_with_recovery(backend, &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(c.get(b"k"), None);
    assert_eq!(c.get(b"k2"), None);
    assert!(c.scan_prefix(b"").is_empty());
}

#[tokio::test]
async fn scan_prefix_ordering() {
    let (mut c, _) = Container::create(MemoryBackend::new()).await.unwrap();
    for k in [&b"ab"[..], b"a\xff", b"b", b"a", b"`z"] {
        c.put(k, b"c").unwrap();
    }
    c.commit().await.unwrap();
    for k in [&b"a\x00"[..], b"aa", b"ac"] {
        c.put(k, b"p").unwrap();
    }
    c.delete(b"ab");
    c.put(b"a", b"p").unwrap();
    let got: Vec<Vec<u8>> = c.scan_prefix(b"a").into_iter().map(|(k, _)| k).collect();
    let want: Vec<Vec<u8>> = [&b"a"[..], b"a\x00", b"aa", b"ac", b"a\xff"]
        .iter()
        .map(|k| k.to_vec())
        .collect();
    assert_eq!(got, want);
    assert_eq!(c.scan_prefix(b"a")[0].1, b"p");
    assert_eq!(c.scan_prefix(b"").len(), 7);
    assert!(c.scan_prefix(b"zz").is_empty());
}

#[tokio::test]
async fn create_on_non_empty_backend_conflicts() {
    let backend = MemoryBackend::new();
    let (c, _) = Container::create(backend.clone()).await.unwrap();
    drop(c);
    assert!(matches!(
        Container::create(backend).await,
        Err(Error::Conflict)
    ));
    // a stray segment alone also makes the backend non-empty
    let other = MemoryBackend::new();
    other
        .put_if_absent("seg/00112233445566778899aabbccddeeff", b"x")
        .await
        .unwrap();
    assert!(matches!(
        Container::create(other).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn open_with_wrong_keys_fails() {
    let backend = MemoryBackend::new();
    let (c, _) = Container::create(backend.clone()).await.unwrap();
    drop(c);
    let (_, other_rk) = Container::create(MemoryBackend::new()).await.unwrap();
    assert!(matches!(
        Container::open_with_recovery(backend.clone(), &other_rk, None).await,
        Err(Error::Crypto)
    ));
    assert!(matches!(
        Container::open_with_crk(backend, vault_container::Crk::generate(), None).await,
        Err(Error::Crypto)
    ));
    assert!(matches!(
        Container::open_with_recovery(MemoryBackend::new(), &other_rk, None).await,
        Err(Error::Format(_))
    ));
}

#[tokio::test]
async fn segment_id_collision_retries_once() {
    let f = Faulty::new(MemoryBackend::new());
    let (mut c, _) = Container::create(f.clone()).await.unwrap();
    f.seg_conflicts.store(1, Ordering::SeqCst);
    c.put(b"k", b"v").unwrap();
    assert_eq!(c.commit().await.unwrap(), 1);
    assert_eq!(f.seg_puts.load(Ordering::SeqCst), 1);

    f.seg_conflicts.store(2, Ordering::SeqCst);
    c.put(b"k", b"w").unwrap();
    assert!(matches!(c.commit().await, Err(Error::SegmentIdCollision)));
    assert_eq!(c.pending_len(), 1);
    assert_eq!(c.commit().await.unwrap(), 2);
    assert_eq!(c.get(b"k").as_deref(), Some(&b"w"[..]));
}

#[tokio::test]
async fn missing_segment_is_an_error() {
    let backend = MemoryBackend::new();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    c.put(b"k", b"v").unwrap();
    c.commit().await.unwrap();
    let name = live_seg_names(&c).await.remove(0);
    drop(c);
    backend.delete(&name).await.unwrap();
    assert!(matches!(
        Container::open_with_recovery(backend, &rk, None).await,
        Err(Error::Format(_))
    ));
}

#[tokio::test]
async fn oversized_record_rejected_without_side_effects() {
    let (mut c, _) = Container::create(MemoryBackend::new()).await.unwrap();
    let huge = vec![0u8; 33 * 1024 * 1024];
    assert!(matches!(c.put(b"k", &huge), Err(Error::Unsupported(_))));
    assert!(matches!(
        c.put_if_absent(b"k", &huge),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(c.pending_len(), 0);
    assert_eq!(c.get(b"k"), None);
}

#[tokio::test]
async fn missing_manifest_is_rollback_or_incomplete_create() {
    let backend = MemoryBackend::new();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    c.put(b"k", b"v").unwrap();
    c.commit().await.unwrap();
    let hw = c.high_water();
    drop(c);
    backend.delete("manifest").await.unwrap();
    // we held a mark: the manifest was there before, so its loss is a rollback
    assert!(matches!(
        Container::open_with_recovery(backend.clone(), &rk, Some(hw)).await,
        Err(Error::Rollback)
    ));
    // never held a mark: indistinguishable from a create that crashed midway
    assert!(matches!(
        Container::open_with_recovery(backend, &rk, None).await,
        Err(Error::IncompleteCreate)
    ));
}

#[tokio::test]
async fn reload_with_missing_manifest_is_rollback() {
    let backend = MemoryBackend::new();
    let (mut c, _) = Container::create(backend.clone()).await.unwrap();
    backend.delete("manifest").await.unwrap();
    assert!(matches!(c.reload().await, Err(Error::Rollback)));
}

#[tokio::test]
async fn successor_not_chained_to_high_water_is_rollback() {
    let backend = MemoryBackend::new();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    c.put(b"k", b"v").unwrap();
    c.commit().await.unwrap();
    let hw1 = c.high_water();
    drop(c);
    let (c, crk) = Container::open_with_recovery(backend.clone(), &rk, Some(hw1))
        .await
        .unwrap();
    let key = crk.subkeys().manifest;
    let (cur, etag) = backend.get("manifest").await.unwrap().unwrap();
    let m = vault_container::manifest::open(&cur, &key).unwrap();
    // A validly sealed and authenticated seq+1 manifest from a fork (another holder of
    // the CRK) that did not build on hw1.
    let auth = crk.subkeys().manifest_auth;
    let mut forked = vault_container::manifest::Manifest {
        seq: hw1.seq + 1,
        prev: [9; 32],
        segments: m.segments.clone(),
        recovery_wrapped_crk: m.recovery_wrapped_crk.clone(),
        auth_tag: [0; 32],
    };
    forked.authenticate(&auth).unwrap();
    let sealed = vault_container::manifest::seal(&forked, &key).unwrap();
    drop(c);
    backend
        .put_if_match("manifest", &sealed, &etag)
        .await
        .unwrap();
    assert!(matches!(
        Container::open_with_recovery(backend.clone(), &rk, Some(hw1)).await,
        Err(Error::Rollback)
    ));
    // a further-ahead manifest cannot be link-checked and is accepted
    let (_, etag) = backend.get("manifest").await.unwrap().unwrap();
    let mut ahead = vault_container::manifest::Manifest {
        seq: hw1.seq + 2,
        ..forked
    };
    ahead.authenticate(&auth).unwrap();
    let sealed = vault_container::manifest::seal(&ahead, &key).unwrap();
    backend
        .put_if_match("manifest", &sealed, &etag)
        .await
        .unwrap();
    assert!(
        Container::open_with_recovery(backend, &rk, Some(hw1))
            .await
            .is_ok()
    );
}

/// A holder of only the manifest subkey (kept for background replication) seals a
/// manifest that drops a segment and moves `seq` ahead: valid AEAD, no valid auth tag.
#[tokio::test]
async fn manifest_forged_with_only_manifest_key_is_rejected() {
    use vault_container::manifest::{self, Manifest};
    let backend = MemoryBackend::new();
    let (mut c, rk) = Container::create(backend.clone()).await.unwrap();
    c.put(b"a", b"1").unwrap();
    c.commit().await.unwrap();
    c.put(b"b", b"2").unwrap();
    c.commit().await.unwrap();
    let hw = c.high_water();
    let mkey = c.crk().subkeys().manifest;
    drop(c);

    let (cur, etag) = backend.get("manifest").await.unwrap().unwrap();
    let m = manifest::open(&cur, &mkey).unwrap();
    let forged = Manifest {
        seq: m.seq + 1,
        prev: manifest::hash(&cur),
        segments: m.segments[..1].to_vec(),
        ..m.clone()
    };
    let sealed = manifest::seal(&forged, &mkey).unwrap();
    backend
        .put_if_match("manifest", &sealed, &etag)
        .await
        .unwrap();
    assert!(matches!(
        Container::open_with_recovery(backend.clone(), &rk, Some(hw)).await,
        Err(Error::Crypto)
    ));
    assert!(matches!(
        Container::open_with_recovery(backend.clone(), &rk, None).await,
        Err(Error::Crypto)
    ));
    // Missing (all-zero) tag on otherwise unchanged contents is rejected too.
    let (_, etag) = backend.get("manifest").await.unwrap().unwrap();
    let untagged = Manifest {
        auth_tag: [0; 32],
        ..m
    };
    let sealed = manifest::seal(&untagged, &mkey).unwrap();
    backend
        .put_if_match("manifest", &sealed, &etag)
        .await
        .unwrap();
    assert!(matches!(
        Container::open_with_recovery(backend, &rk, None).await,
        Err(Error::Crypto)
    ));
}
