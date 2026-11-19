// SPDX-License-Identifier: MIT

//! Replication to several replicas and restore from them.

#[path = "support/mock_s3.rs"]
mod mock_s3;
#[path = "support/mock_webdav.rs"]
mod mock_webdav;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use vault_container::backend::{
    ContainerBackend, Etag, MemoryBackend, PutOutcome, S3Backend, S3Config, WebDavBackend,
    WebDavConfig,
};
use vault_container::engine::{Container, HighWater};
use vault_container::replicate::{
    ObservedMark, ReplicaId, ReplicaOutcome, ReplicaRecord, ReplicaSet, ReplicaState,
    ReplicaStatus, restore,
};
use vault_container::{Error, RecoveryKey, Result};
use zeroize::Zeroizing;

type Dyn = Arc<dyn ContainerBackend>;

fn rng(seed: u64) -> ChaCha20Rng {
    ChaCha20Rng::seed_from_u64(seed)
}

fn ok(_: &ReplicaState) -> Result<()> {
    Ok(())
}

/// Configuration identity of the restore target.
fn tid() -> ReplicaId {
    id("local")
}

fn id(s: &str) -> ReplicaId {
    ReplicaId(s.into())
}

fn s3(mock: &mock_s3::MockS3) -> S3Backend {
    S3Backend::new(S3Config {
        endpoint: mock.endpoint(),
        region: "us-east-1".into(),
        bucket: "vault".into(),
        prefix: "replica".into(),
        access_key: "AKIAEXAMPLE".into(),
        secret_key: Zeroizing::new("secret".into()),
        path_style: true,
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap()
}

fn webdav(mock: &mock_webdav::MockWebDav) -> WebDavBackend {
    WebDavBackend::new(WebDavConfig {
        base_url: mock.base_url(),
        username: mock_webdav::USER.into(),
        password: Zeroizing::new(mock_webdav::PASSWORD.into()),
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap()
}

/// Backend wrapper whose every call fails with an I/O error while `down` is set.
#[derive(Clone)]
struct Flaky<B> {
    inner: B,
    down: Arc<AtomicBool>,
}

impl<B> Flaky<B> {
    fn new(inner: B) -> Self {
        Self {
            inner,
            down: Arc::new(AtomicBool::new(true)),
        }
    }
    fn check(&self) -> Result<()> {
        if self.down.load(Ordering::SeqCst) {
            Err(Error::Io("replica unreachable".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl<B: ContainerBackend> ContainerBackend for Flaky<B> {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        self.check()?;
        self.inner.get(name).await
    }
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        self.check()?;
        self.inner.put_if_absent(name, data).await
    }
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        self.check()?;
        self.inner.put_if_match(name, data, etag).await
    }
    async fn delete(&self, name: &str) -> Result<()> {
        self.check()?;
        self.inner.delete(name).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.check()?;
        self.inner.list(prefix).await
    }
}

/// A primary container with a few committed records.
async fn primary() -> (MemoryBackend, Container<MemoryBackend>, RecoveryKey) {
    let b = MemoryBackend::new();
    let (mut c, rk) = Container::create(b.clone()).await.unwrap();
    c.put(b"alpha", b"1").unwrap();
    c.put(b"beta", b"2").unwrap();
    c.commit().await.unwrap();
    (b, c, rk)
}

fn mkey(c: &Container<MemoryBackend>) -> Zeroizing<[u8; 32]> {
    c.crk().subkeys().manifest
}

/// Every object of `b`: header, manifest and segments.
async fn snapshot(b: &dyn ContainerBackend) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for n in ["header", "manifest"] {
        if let Some((d, _)) = b.get(n).await.unwrap() {
            out.push((n.to_string(), d));
        }
    }
    for n in b.list("seg/").await.unwrap() {
        let (d, _) = b.get(&n).await.unwrap().unwrap();
        out.push((n, d));
    }
    out
}

async fn copy_all(src: &dyn ContainerBackend, dst: &dyn ContainerBackend) {
    for (n, d) in snapshot(src).await {
        assert!(matches!(
            dst.put_if_absent(&n, &d).await.unwrap(),
            PutOutcome::Written(_)
        ));
    }
}

/// Replaces `name` on `b` unconditionally.
async fn overwrite(b: &dyn ContainerBackend, name: &str, data: &[u8]) {
    match b.get(name).await.unwrap() {
        Some((_, e)) => {
            assert!(matches!(
                b.put_if_match(name, data, &e).await.unwrap(),
                PutOutcome::Written(_)
            ));
        }
        None => {
            b.put_if_absent(name, data).await.unwrap();
        }
    }
}

fn outcome<'a>(statuses: &'a [ReplicaStatus], r: &str) -> &'a ReplicaOutcome {
    &statuses.iter().find(|s| s.id.0 == r).unwrap().outcome
}

fn assert_synced(statuses: &[ReplicaStatus], r: &str, seq: u64) {
    match outcome(statuses, r) {
        ReplicaOutcome::Synced { seq: s, .. } => assert_eq!(*s, seq, "{r}"),
        other => panic!("{r}: expected Synced, got {other:?}"),
    }
}

/// Replica holds exactly the primary's header, manifest and live segments.
async fn assert_mirrors(primary: &Container<MemoryBackend>, replica: &dyn ContainerBackend) {
    let want: Vec<String> = {
        let mut v: Vec<String> = primary
            .segment_ids()
            .iter()
            .map(|i| format!("seg/{}", data_encoding::HEXLOWER.encode(i)))
            .collect();
        v.sort();
        v
    };
    let mut got = replica.list("seg/").await.unwrap();
    got.sort();
    assert_eq!(got, want);
    let p = primary.backend();
    for n in ["header", "manifest"] {
        assert_eq!(
            replica.get(n).await.unwrap().unwrap().0,
            p.get(n).await.unwrap().unwrap().0,
            "{n}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replication_converges() {
    let (pb, mut c, rk) = primary().await;
    let ms3 = mock_s3::MockS3::start("vault", mock_s3::Mode::Enforce, 2);
    let mdav = mock_webdav::MockWebDav::start(mock_webdav::Mode::Enforce);
    let mem = MemoryBackend::new();
    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("mem"), Arc::new(mem.clone())),
        (id("s3"), Arc::new(s3(&ms3))),
        (id("dav"), Arc::new(webdav(&mdav))),
    ];
    let mut set = ReplicaSet::new(
        Arc::new(pb.clone()),
        replicas.clone(),
        ReplicaState::default(),
    );

    let st = set.replicate_once(&mkey(&c), ok).await;
    for r in ["mem", "s3", "dav"] {
        assert_synced(&st, r, 1);
    }
    for (_, b) in &replicas {
        assert_mirrors(&c, b.as_ref()).await;
    }

    // More commits and a compaction: old segments must disappear from replicas.
    c.put(b"gamma", b"3").unwrap();
    c.commit().await.unwrap();
    c.delete(b"alpha");
    c.compact().await.unwrap();
    let st = set.replicate_once(&mkey(&c), ok).await;
    for r in ["mem", "s3", "dav"] {
        match outcome(&st, r) {
            ReplicaOutcome::Synced { seq, deleted, .. } => {
                assert_eq!(*seq, 3);
                assert_eq!(*deleted, 1, "{r}: the pre-compaction segment is removed");
            }
            other => panic!("{r}: {other:?}"),
        }
    }
    for (_, b) in &replicas {
        assert_mirrors(&c, b.as_ref()).await;
    }
    let hw = c.high_water();
    for (rid, rec) in &set.state().replicas {
        assert_eq!(rec.last_seen, Some(hw), "{rid:?}");
        assert!(rec.last_ok_at_ms.is_some() && rec.last_error.is_none());
    }

    // Every replica opens with the recovery key and holds the primary's state.
    let want = c.scan_prefix(b"");
    let (m, _) = Container::open_with_recovery(mem, &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(m.scan_prefix(b""), want);
    let (s, _) = Container::open_with_recovery(s3(&ms3), &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(s.scan_prefix(b""), want);
    let (d, _) = Container::open_with_recovery(webdav(&mdav), &rk, Some(hw))
        .await
        .unwrap();
    assert_eq!(d.scan_prefix(b""), want);

    // State round-trips through serde for the caller to persist.
    let bytes = postcard::to_allocvec(set.state()).unwrap();
    let back: ReplicaState = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(&back, set.state());
}

#[tokio::test]
async fn replica_failure_retries_and_reports() {
    let (pb, c, _) = primary().await;
    let flaky = Flaky::new(MemoryBackend::new());
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![(id("nas"), Arc::new(flaky.clone()))],
        ReplicaState::default(),
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    match outcome(&st, "nas") {
        ReplicaOutcome::Failed { error } => assert!(error.contains("unreachable"), "{error}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    let rec = &set.state().replicas[&id("nas")];
    assert!(rec.last_error.as_deref().unwrap().contains("unreachable"));
    assert_eq!(rec.last_seen, None);
    assert_eq!(rec.last_ok_at_ms, None);

    // The caller retries later (backoff is its job); the replica recovered.
    flaky.down.store(false, Ordering::SeqCst);
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert_synced(&st, "nas", 1);
    let rec = &set.state().replicas[&id("nas")];
    assert_eq!(rec.last_error, None);
    assert_eq!(rec.last_seen, Some(c.high_water()));
    assert!(rec.last_ok_at_ms.is_some());
    assert_mirrors(&c, &flaky.inner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_replica_failure_does_not_block_others() {
    let (pb, c, _) = primary().await;
    let ms3 = mock_s3::MockS3::start("vault", mock_s3::Mode::Fail(503), 2);
    let mem = MemoryBackend::new();
    let mdav = mock_webdav::MockWebDav::start(mock_webdav::Mode::Enforce);
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![
            (id("s3"), Arc::new(s3(&ms3))),
            (id("mem"), Arc::new(mem.clone())),
            (id("dav"), Arc::new(webdav(&mdav))),
        ],
        ReplicaState::default(),
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert_eq!(st.len(), 3);
    assert!(matches!(outcome(&st, "s3"), ReplicaOutcome::Failed { .. }));
    assert_synced(&st, "mem", 1);
    assert_synced(&st, "dav", 1);
    assert_mirrors(&c, &mem).await;
    assert_mirrors(&c, &webdav(&mdav)).await;
    assert!(set.state().replicas[&id("s3")].last_error.is_some());
}

#[tokio::test]
async fn replicate_is_idempotent() {
    let (pb, c, _) = primary().await;
    let mem = MemoryBackend::new();
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![(id("mem"), Arc::new(mem.clone()))],
        ReplicaState::default(),
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(matches!(
        outcome(&st, "mem"),
        ReplicaOutcome::Synced {
            seq: 1,
            uploaded: 1,
            deleted: 0
        }
    ));
    let before = snapshot(&mem).await;
    let etag = mem.get("manifest").await.unwrap().unwrap().1;
    let state = set.state().clone();
    for _ in 0..2 {
        let st = set.replicate_once(&mkey(&c), ok).await;
        assert!(matches!(
            outcome(&st, "mem"),
            ReplicaOutcome::Synced {
                seq: 1,
                uploaded: 0,
                deleted: 0
            }
        ));
    }
    assert_eq!(snapshot(&mem).await, before);
    assert_eq!(mem.get("manifest").await.unwrap().unwrap().1, etag);
    assert_eq!(
        set.state().replicas[&id("mem")].last_seen,
        state.replicas[&id("mem")].last_seen
    );

    // Fresh state (e.g. lost) against an up-to-date replica is fine as well.
    let mut again = ReplicaSet::new(
        Arc::new(c.backend().clone()),
        vec![(id("mem"), Arc::new(mem.clone()))],
        ReplicaState::default(),
    );
    assert_synced(&again.replicate_once(&mkey(&c), ok).await, "mem", 1);
    assert_eq!(snapshot(&mem).await, before);
}

#[tokio::test]
async fn diverged_replica_not_overwritten() {
    let (pb, mut c, rk) = primary().await;
    // `ahead`: a fork that moved past the primary. `same`: a fork at the primary's seq.
    let ahead = MemoryBackend::new();
    copy_all(&pb, &ahead).await;
    let (mut f, _) = Container::open_with_recovery(ahead.clone(), &rk, None)
        .await
        .unwrap();
    f.put(b"fork", b"x").unwrap();
    f.commit().await.unwrap();
    f.put(b"fork", b"y").unwrap();
    f.commit().await.unwrap(); // seq 3

    let same = MemoryBackend::new();
    copy_all(&pb, &same).await;
    let (mut g, _) = Container::open_with_recovery(same.clone(), &rk, None)
        .await
        .unwrap();
    g.put(b"other", b"z").unwrap();
    g.commit().await.unwrap(); // seq 2
    c.put(b"mine", b"m").unwrap();
    c.commit().await.unwrap(); // primary seq 2, different hash

    let foreign = MemoryBackend::new();
    let (_other, _) = Container::create(foreign.clone()).await.unwrap();

    let snaps = [
        snapshot(&ahead).await,
        snapshot(&same).await,
        snapshot(&foreign).await,
    ];
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![
            (id("ahead"), Arc::new(ahead.clone())),
            (id("same"), Arc::new(same.clone())),
            (id("foreign"), Arc::new(foreign.clone())),
        ],
        ReplicaState::default(),
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    for r in ["ahead", "same", "foreign"] {
        assert!(
            matches!(outcome(&st, r), ReplicaOutcome::Diverged { .. }),
            "{r}: {:?}",
            outcome(&st, r)
        );
        assert!(set.state().replicas[&id(r)].last_error.is_some());
        assert_eq!(set.state().replicas[&id(r)].last_seen, None);
    }
    match outcome(&st, "ahead") {
        ReplicaOutcome::Diverged {
            replica_seq,
            primary_seq,
            ..
        } => {
            assert_eq!((*replica_seq, *primary_seq), (Some(3), 2));
        }
        _ => unreachable!(),
    }
    // Nothing was written, overwritten or deleted.
    for (b, snap) in [&ahead, &same, &foreign].into_iter().zip(&snaps) {
        assert_eq!(&snapshot(b).await, snap);
    }

    // A replica that the vault itself synced, then forked by another writer at a
    // seq the vault has seen, is diverged as well.
    let mem = MemoryBackend::new();
    let mut set = ReplicaSet::new(
        Arc::new(c.backend().clone()),
        vec![(id("mem"), Arc::new(mem.clone()))],
        ReplicaState::default(),
    );
    assert_synced(&set.replicate_once(&mkey(&c), ok).await, "mem", 2);
    let (mut h, _) = Container::open_with_recovery(mem.clone(), &rk, None)
        .await
        .unwrap();
    h.put(b"intruder", b"i").unwrap();
    h.commit().await.unwrap(); // replica seq 3 on a branch
    c.put(b"mine2", b"m").unwrap();
    c.commit().await.unwrap();
    c.put(b"mine3", b"m").unwrap();
    c.commit().await.unwrap(); // primary seq 4
    let man = mem.get("manifest").await.unwrap().unwrap().0;
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(
        matches!(outcome(&st, "mem"), ReplicaOutcome::Diverged { .. }),
        "{st:?}"
    );
    assert_eq!(mem.get("manifest").await.unwrap().unwrap().0, man);
}

#[tokio::test]
async fn replica_rollback_is_flagged_and_repaired() {
    let (pb, mut c, _) = primary().await;
    let mem = MemoryBackend::new();
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![(id("mem"), Arc::new(mem.clone()))],
        ReplicaState::default(),
    );
    set.replicate_once(&mkey(&c), ok).await;
    let old = mem.get("manifest").await.unwrap().unwrap().0;
    c.put(b"x", b"1").unwrap();
    c.commit().await.unwrap();
    assert_synced(&set.replicate_once(&mkey(&c), ok).await, "mem", 2);
    // The replica serves an old manifest again.
    overwrite(&mem, "manifest", &old).await;
    let st = set.replicate_once(&mkey(&c), ok).await;
    match outcome(&st, "mem") {
        ReplicaOutcome::Rollback {
            found_seq,
            last_seen_seq,
            seq,
        } => {
            assert_eq!((*found_seq, *last_seen_seq, *seq), (Some(1), 2, 2));
        }
        other => panic!("expected Rollback, got {other:?}"),
    }
    assert_mirrors(&c, &mem).await;
    // A deleted manifest is a rollback too, repaired the same way.
    mem.delete("manifest").await.unwrap();
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(matches!(
        outcome(&st, "mem"),
        ReplicaOutcome::Rollback {
            found_seq: None,
            last_seen_seq: 2,
            seq: 2
        }
    ));
    assert_mirrors(&c, &mem).await;
}

/// Sets up replicas at different seqs: `stale` at 1, `current` at 2.
async fn staged() -> (
    Container<MemoryBackend>,
    RecoveryKey,
    MemoryBackend,
    MemoryBackend,
) {
    let (pb, mut c, rk) = primary().await;
    let stale = MemoryBackend::new();
    let current = MemoryBackend::new();
    let mut set = ReplicaSet::new(
        Arc::new(pb.clone()),
        vec![(id("stale"), Arc::new(stale.clone()))],
        ReplicaState::default(),
    );
    set.replicate_once(&mkey(&c), ok).await;
    c.put(b"gamma", b"3").unwrap();
    c.commit().await.unwrap();
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![(id("current"), Arc::new(current.clone()))],
        ReplicaState::default(),
    );
    assert_synced(&set.replicate_once(&mkey(&c), ok).await, "current", 2);
    (c, rk, stale, current)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_picks_highest_and_flags_rollback() {
    let (c, rk, stale, current) = staged().await;
    let ms3 = mock_s3::MockS3::start("vault", mock_s3::Mode::Enforce, 2);
    let s3b = s3(&ms3);
    copy_all(&current, &s3b).await;
    let mdav = mock_webdav::MockWebDav::start(mock_webdav::Mode::Enforce);
    let dav = webdav(&mdav);
    copy_all(&stale, &dav).await;
    let down = Flaky::new(MemoryBackend::new());

    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("dav-stale"), Arc::new(dav)),
        (id("s3-current"), Arc::new(s3b)),
        (id("down"), Arc::new(down)),
        (id("mem-stale"), Arc::new(stale)),
    ];
    let target = MemoryBackend::new();
    let rep = restore(&replicas, &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!(rep.chosen, id("s3-current"));
    assert_eq!(rep.seq, 2);
    let mut rb = rep.rollbacks.clone();
    rb.sort();
    assert_eq!(rb, vec![id("dav-stale"), id("mem-stale")]);
    assert_eq!(rep.unreachable, vec![id("down")]);
    assert!(rep.diverged.is_empty() && rep.broken.is_empty());

    // The target is a byte copy of the chosen replica and opens with the CRK.
    assert_eq!(snapshot(&target).await, snapshot(&current).await);
    let r = Container::open_with_crk(target, rep.crk, Some(c.high_water()))
        .await
        .unwrap();
    assert_eq!(r.scan_prefix(b""), c.scan_prefix(b""));

    // A non-empty target is refused.
    let busy = MemoryBackend::new();
    copy_all(&current, &busy).await;
    assert!(matches!(
        restore(&replicas, &rk, &tid(), &busy, None).await,
        Err(Error::Conflict)
    ));
    // A wrong recovery key unlocks nothing.
    let wrong = RecoveryKey::generate_with(&mut rng(1234));
    assert!(matches!(
        restore(&replicas, &wrong, &tid(), &MemoryBackend::new(), None).await,
        Err(Error::Crypto)
    ));
}

#[tokio::test]
async fn restore_rejects_broken_chain() {
    let (c, rk, stale, current) = staged().await;
    // `current` lost the segment it got at seq 2; `tampered` has a corrupted one.
    let tampered = MemoryBackend::new();
    copy_all(&current, &tampered).await;
    let last = c.segment_ids().last().unwrap();
    let name = format!("seg/{}", data_encoding::HEXLOWER.encode(last));
    current.delete(&name).await.unwrap();
    let mut bytes = tampered.get(&name).await.unwrap().unwrap().0;
    bytes[100] ^= 1;
    overwrite(&tampered, &name, &bytes).await;

    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("current"), Arc::new(current.clone())),
        (id("tampered"), Arc::new(tampered.clone())),
        (id("stale"), Arc::new(stale.clone())),
    ];
    let target = MemoryBackend::new();
    let rep = restore(&replicas, &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!((rep.chosen.clone(), rep.seq), (id("stale"), 1));
    let mut broken = rep.broken.clone();
    broken.sort();
    assert_eq!(broken, vec![id("current"), id("tampered")]);
    assert!(
        rep.rollbacks.is_empty(),
        "nothing valid is newer than the choice"
    );
    assert_eq!(snapshot(&target).await, snapshot(&stale).await);

    // Only broken replicas: refused, and the target is left empty.
    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("current"), Arc::new(current)),
        (id("tampered"), Arc::new(tampered)),
    ];
    let target = MemoryBackend::new();
    assert!(matches!(
        restore(&replicas, &rk, &tid(), &target, None).await,
        Err(Error::Format(_))
    ));
    assert!(snapshot(&target).await.is_empty());
}

#[tokio::test]
async fn restore_refuses_below_known_high_water() {
    let (c, rk, stale, _current) = staged().await;
    let replicas: Vec<(ReplicaId, Dyn)> = vec![(id("stale"), Arc::new(stale))];
    let target = MemoryBackend::new();
    assert!(matches!(
        restore(&replicas, &rk, &tid(), &target, Some(c.high_water())).await,
        Err(Error::Rollback)
    ));
    assert!(snapshot(&target).await.is_empty());
    // At the mark itself, but with another hash: a fork, refused too.
    let fake = HighWater {
        seq: 1,
        hash: [9; 32],
    };
    assert!(matches!(
        restore(&replicas, &rk, &tid(), &target, Some(fake)).await,
        Err(Error::Rollback)
    ));
    assert!(snapshot(&target).await.is_empty());
}

#[tokio::test]
async fn restore_same_seq_fork_needs_user_choice() {
    let (pb, _c, rk) = primary().await;
    let a = MemoryBackend::new();
    let b = MemoryBackend::new();
    copy_all(&pb, &a).await;
    copy_all(&pb, &b).await;
    for (backend, v) in [(&a, b"a"), (&b, b"b")] {
        let (mut f, _) = Container::open_with_recovery(backend.clone(), &rk, None)
            .await
            .unwrap();
        f.put(b"k", v).unwrap();
        f.commit().await.unwrap();
    }
    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("a"), Arc::new(a.clone())),
        (id("b"), Arc::new(b.clone())),
    ];
    let target = MemoryBackend::new();
    match restore(&replicas, &rk, &tid(), &target, None).await {
        Err(Error::DivergedReplicas(ids)) => {
            let mut ids = ids;
            ids.sort();
            assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        }
        other => panic!(
            "expected DivergedReplicas, got {:?}",
            other.map(|r| r.chosen)
        ),
    }
    assert!(snapshot(&target).await.is_empty());
    // The user picks one by passing only that replica.
    let rep = restore(&replicas[1..], &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!((rep.chosen, rep.seq), (id("b"), 2));

    // When one side of the fork does not verify, the user still has to choose:
    // silently taking the other branch could drop the newer history.
    let seg = b.list("seg/").await.unwrap();
    for n in seg {
        b.delete(&n).await.unwrap();
    }
    let target = MemoryBackend::new();
    assert!(matches!(
        restore(&replicas, &rk, &tid(), &target, None).await,
        Err(Error::DivergedReplicas(_))
    ));
    assert!(snapshot(&target).await.is_empty());
    let rep = restore(&replicas[..1], &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!(rep.chosen, id("a"));
}

#[tokio::test]
async fn restore_then_newer_unreachable_replica_is_not_overwritten() {
    let (_c, rk, stale, current) = staged().await;
    let newer = Flaky::new(current.clone());
    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("stale"), Arc::new(stale.clone())),
        (id("newer"), Arc::new(newer.clone())),
    ];
    let target = MemoryBackend::new();
    let rep = restore(&replicas, &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!((rep.chosen.clone(), rep.seq), (id("stale"), 1));
    assert_eq!(rep.unreachable, vec![id("newer")]);
    let mark = rep.state.restored_floor.unwrap();
    assert_eq!(mark.seq, 1);
    assert_eq!(rep.state.replicas[&id("stale")].last_seen, Some(mark));

    // The user carries on with the restored container; the newer replica returns.
    let mut r = Container::open_with_crk(target.clone(), rep.crk, Some(mark))
        .await
        .unwrap();
    r.put(b"after", b"1").unwrap();
    r.commit().await.unwrap();
    r.put(b"after", b"2").unwrap();
    r.commit().await.unwrap(); // seq 3, past the unreachable replica's seq 2
    newer.down.store(false, Ordering::SeqCst);
    let before = snapshot(&current).await;
    let mut set = ReplicaSet::new(Arc::new(target), replicas, rep.state);
    let st = set.replicate_once(&mkey(&r), ok).await;
    assert_synced(&st, "stale", 3);
    assert!(
        matches!(
            outcome(&st, "newer"),
            ReplicaOutcome::Diverged {
                replica_seq: Some(2),
                ..
            }
        ),
        "{st:?}"
    );
    let now = snapshot(&current).await;
    for obj in &before {
        assert!(now.contains(obj), "{} changed", obj.0);
    }
    assert_eq!(
        current.get("manifest").await.unwrap().unwrap().0,
        before.iter().find(|o| o.0 == "manifest").unwrap().1
    );
}

/// An object another writer stores at the next `list`.
type PendingWrite = Arc<std::sync::Mutex<Option<(String, Vec<u8>)>>>;

/// Memory backend that, on its next `list`, lets "another writer" replace an object.
#[derive(Clone)]
struct Racy {
    inner: MemoryBackend,
    on_list: PendingWrite,
}

#[async_trait::async_trait]
impl ContainerBackend for Racy {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        self.inner.get(name).await
    }
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        self.inner.put_if_absent(name, data).await
    }
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        self.inner.put_if_match(name, data, etag).await
    }
    async fn delete(&self, name: &str) -> Result<()> {
        self.inner.delete(name).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let out = self.inner.list(prefix).await;
        let pending = self.on_list.lock().unwrap().take();
        if let Some((name, data)) = pending {
            overwrite(&self.inner, &name, &data).await;
        }
        out
    }
}

#[tokio::test]
async fn deletes_recheck_the_replica_manifest() {
    let (pb, c, rk) = primary().await;
    let mem = MemoryBackend::new();
    copy_all(&pb, &mem).await; // replica in sync with the primary
    // Another writer prepares seq 2 with a new segment on a copy.
    let other = MemoryBackend::new();
    copy_all(&pb, &other).await;
    let (mut o, _) = Container::open_with_recovery(other.clone(), &rk, None)
        .await
        .unwrap();
    o.put(b"theirs", b"t").unwrap();
    o.commit().await.unwrap();
    let theirs = format!(
        "seg/{}",
        data_encoding::HEXLOWER.encode(o.segment_ids().last().unwrap())
    );
    let seg = other.get(&theirs).await.unwrap().unwrap().0;
    mem.put_if_absent(&theirs, &seg).await.unwrap();
    let their_manifest = other.get("manifest").await.unwrap().unwrap().0;

    let racy = Racy {
        inner: mem.clone(),
        on_list: Arc::new(std::sync::Mutex::new(Some((
            "manifest".into(),
            their_manifest,
        )))),
    };
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![(id("r"), Arc::new(racy))],
        ReplicaState::default(),
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(
        matches!(outcome(&st, "r"), ReplicaOutcome::Failed { .. }),
        "{st:?}"
    );
    assert!(
        mem.get(&theirs).await.unwrap().is_some(),
        "referenced segment deleted"
    );
}

#[tokio::test]
async fn crash_after_manifest_write_is_recognised() {
    let (pb, mut c, _) = primary().await;
    let mem = MemoryBackend::new();
    let replicas: Vec<(ReplicaId, Dyn)> = vec![(id("mem"), Arc::new(mem.clone()))];
    let mut set = ReplicaSet::new(
        Arc::new(pb.clone()),
        replicas.clone(),
        ReplicaState::default(),
    );
    set.replicate_once(&mkey(&c), ok).await;
    c.put(b"x", b"1").unwrap();
    c.commit().await.unwrap(); // seq 2
    // The last state persisted before the crash is what the hook saw.
    let mut persisted = None;
    let st = set
        .replicate_once(&mkey(&c), |s: &ReplicaState| {
            persisted = Some(s.clone());
            Ok(())
        })
        .await;
    assert_synced(&st, "mem", 2);
    let persisted = persisted.expect("state persisted before the manifest write");
    assert_eq!(persisted.replicas[&id("mem")].last_seen.unwrap().seq, 1);
    assert_eq!(
        persisted.replicas[&id("mem")].in_flight,
        Some(c.high_water())
    );
    // Crash: the final state is lost. The primary moves on.
    c.put(b"x", b"2").unwrap();
    c.commit().await.unwrap(); // seq 3
    let mut set = ReplicaSet::new(Arc::new(pb.clone()), replicas.clone(), persisted);
    assert_synced(&set.replicate_once(&mkey(&c), ok).await, "mem", 3);
    assert_mirrors(&c, &mem).await;

    // A failing persist hook stops the manifest write.
    c.put(b"x", b"3").unwrap();
    c.commit().await.unwrap();
    let man = mem.get("manifest").await.unwrap().unwrap().0;
    let st = set
        .replicate_once(&mkey(&c), |_: &ReplicaState| {
            Err(Error::Io("disk full".into()))
        })
        .await;
    assert!(matches!(outcome(&st, "mem"), ReplicaOutcome::Failed { .. }));
    assert_eq!(mem.get("manifest").await.unwrap().unwrap().0, man);
}

#[tokio::test]
async fn adopt_diverged_overwrites_after_confirmation() {
    let (pb, c, rk) = primary().await;
    let ahead = MemoryBackend::new();
    copy_all(&pb, &ahead).await;
    let (mut f, _) = Container::open_with_recovery(ahead.clone(), &rk, None)
        .await
        .unwrap();
    f.put(b"fork", b"x").unwrap();
    f.commit().await.unwrap();
    let foreign = MemoryBackend::new();
    Container::create(foreign.clone()).await.unwrap();
    let mut state = ReplicaState::default();
    state.replicas.insert(id("gone"), Default::default());
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![
            (id("ahead"), Arc::new(ahead.clone())),
            (id("foreign"), Arc::new(foreign.clone())),
        ],
        state,
    );
    assert!(
        set.state().replicas.contains_key(&id("gone")),
        "unconfigured entry kept"
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    let observed = |r: &str| match outcome(&st, r) {
        ReplicaOutcome::Diverged { observed, .. } => *observed,
        other => panic!("{r}: {other:?}"),
    };
    let (seen_ahead, seen_foreign) = (observed("ahead"), observed("foreign"));
    assert!(matches!(seen_foreign, ObservedMark::Header { .. }));
    assert!(set.adopt_diverged(&id("nope"), seen_ahead).is_err());

    // The confirmation is bound to what the user saw: the fork moved on since.
    set.adopt_diverged(&id("ahead"), seen_ahead).unwrap();
    // Likewise a header mark other than the one reported.
    set.adopt_diverged(&id("foreign"), ObservedMark::Header { hash: [0; 32] })
        .unwrap();
    let foreign_snap = snapshot(&foreign).await;
    f.put(b"fork", b"y").unwrap();
    f.commit().await.unwrap();
    let man = ahead.get("manifest").await.unwrap().unwrap().0;
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(matches!(
        outcome(&st, "ahead"),
        ReplicaOutcome::Diverged { .. }
    ));
    assert_eq!(ahead.get("manifest").await.unwrap().unwrap().0, man);
    assert_eq!(
        set.state().replicas[&id("ahead")].adopt,
        None,
        "stale confirmation dropped"
    );
    assert!(matches!(
        outcome(&st, "foreign"),
        ReplicaOutcome::Diverged { .. }
    ));
    assert_eq!(set.state().replicas[&id("foreign")].adopt, None);
    assert_eq!(snapshot(&foreign).await, foreign_snap);

    // Confirmed again with the current mark.
    let ReplicaOutcome::Diverged { observed: now, .. } = outcome(&st, "ahead").clone() else {
        unreachable!()
    };
    set.adopt_diverged(&id("ahead"), now).unwrap();
    set.adopt_diverged(&id("foreign"), seen_foreign).unwrap();
    // The persist hook runs before anything is overwritten; a failing one stops it.
    let foreign_before = snapshot(&foreign).await;
    let ahead_man = ahead.get("manifest").await.unwrap().unwrap().0;
    let st = set
        .replicate_once(&mkey(&c), |_: &ReplicaState| {
            Err(Error::Io("disk full".into()))
        })
        .await;
    assert!(matches!(
        outcome(&st, "foreign"),
        ReplicaOutcome::Failed { .. }
    ));
    assert!(matches!(
        outcome(&st, "ahead"),
        ReplicaOutcome::Failed { .. }
    ));
    let after = snapshot(&foreign).await;
    for obj in &foreign_before {
        assert!(after.contains(obj), "{} overwritten before persist", obj.0);
    }
    assert_eq!(ahead.get("manifest").await.unwrap().unwrap().0, ahead_man);
    let st = set.replicate_once(&mkey(&c), ok).await;
    for (r, replaced) in [("ahead", now), ("foreign", seen_foreign)] {
        match outcome(&st, r) {
            ReplicaOutcome::Adopted {
                seq,
                replaced: got,
                rollback,
            } => assert_eq!((*seq, *got, *rollback), (1, replaced, false)),
            other => panic!("{r}: {other:?}"),
        }
    }
    assert_mirrors(&c, &ahead).await;
    assert_mirrors(&c, &foreign).await;
    assert_eq!(set.state().replicas[&id("ahead")].adopt, None);
}

#[tokio::test]
async fn adopting_an_older_manifest_reports_rollback() {
    let (pb, mut c, rk) = primary().await;
    let mem = MemoryBackend::new();
    let mut set = ReplicaSet::new(
        Arc::new(pb.clone()),
        vec![(id("mem"), Arc::new(mem.clone()))],
        ReplicaState::default(),
    );
    // A fork of seq 1 made elsewhere (not a manifest this vault wrote).
    let fork = MemoryBackend::new();
    copy_all(&pb, &fork).await;
    let (mut f, _) = Container::open_with_recovery(fork.clone(), &rk, None)
        .await
        .unwrap();
    f.put(b"fork", b"x").unwrap();
    f.commit().await.unwrap(); // fork seq 2
    for v in [b"1", b"2"] {
        c.put(b"x", v).unwrap();
        c.commit().await.unwrap();
    }
    assert_synced(&set.replicate_once(&mkey(&c), ok).await, "mem", 3);
    c.put(b"x", b"3").unwrap();
    c.commit().await.unwrap(); // primary seq 4, last_seen 3
    // The replica serves the forked seq-2 manifest: below last_seen, i.e. a
    // rollback, which is repaired without confirmation.
    overwrite(
        &mem,
        "manifest",
        &fork.get("manifest").await.unwrap().unwrap().0,
    )
    .await;
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(
        matches!(
            outcome(&st, "mem"),
            ReplicaOutcome::Rollback {
                found_seq: Some(2),
                ..
            }
        ),
        "{st:?}"
    );

    // Unopenable garbage while last_seen is 4: diverged; once adopted, the
    // rollback signal is kept only for older openable manifests.
    overwrite(&mem, "manifest", &[1u8; 4096]).await;
    let st = set.replicate_once(&mkey(&c), ok).await;
    let ReplicaOutcome::Diverged { observed, .. } = outcome(&st, "mem").clone() else {
        panic!("{st:?}")
    };
    assert!(matches!(observed, ObservedMark::Manifest { seq: None, .. }));
    set.adopt_diverged(&id("mem"), observed).unwrap();
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(
        matches!(
            outcome(&st, "mem"),
            ReplicaOutcome::Adopted {
                seq: 4,
                rollback: false,
                ..
            }
        ),
        "{st:?}"
    );
    assert_mirrors(&c, &mem).await;
}

/// Target whose header writes and deletes fail while `broken` is set.
#[derive(Clone)]
struct BadTarget {
    inner: MemoryBackend,
    broken: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ContainerBackend for BadTarget {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        self.inner.get(name).await
    }
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        if name == "header" && self.broken.load(Ordering::SeqCst) {
            return Err(Error::Io("disk error".into()));
        }
        self.inner.put_if_absent(name, data).await
    }
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        self.inner.put_if_match(name, data, etag).await
    }
    async fn delete(&self, name: &str) -> Result<()> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(Error::Io("disk error".into()));
        }
        self.inner.delete(name).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn restore_retries_after_interrupted_target() {
    let (c, rk, _stale, current) = staged().await;
    let replicas: Vec<(ReplicaId, Dyn)> = vec![(id("current"), Arc::new(current.clone()))];
    let target = BadTarget {
        inner: MemoryBackend::new(),
        broken: Arc::new(AtomicBool::new(true)),
    };
    assert!(matches!(
        restore(&replicas, &rk, &tid(), &target, None).await,
        Err(Error::Io(_))
    ));
    assert!(
        !target.inner.list("seg/").await.unwrap().is_empty(),
        "leftovers expected"
    );
    target.broken.store(false, Ordering::SeqCst);
    let rep = restore(&replicas, &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!(rep.seq, 2);
    assert_eq!(snapshot(&target.inner).await, snapshot(&current).await);
    let r = Container::open_with_crk(target.inner.clone(), rep.crk, Some(c.high_water()))
        .await
        .unwrap();
    assert_eq!(r.scan_prefix(b""), c.scan_prefix(b""));
}

#[tokio::test]
async fn restore_never_wipes_a_replica_or_unverified_leftovers() {
    let (_c, rk, stale, current) = staged().await;
    // A replica that lost header and manifest still holds its segments.
    let hurt = MemoryBackend::new();
    copy_all(&current, &hurt).await;
    hurt.delete("header").await.unwrap();
    hurt.delete("manifest").await.unwrap();
    let segs = snapshot(&hurt).await;
    assert!(!segs.is_empty());

    // The target aliases that replica: refused before anything is touched.
    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("stale"), Arc::new(stale.clone())),
        (id("hurt"), Arc::new(hurt.clone())),
    ];
    assert!(matches!(
        restore(&replicas, &rk, &id("hurt"), &hurt, None).await,
        Err(Error::Conflict)
    ));
    assert_eq!(snapshot(&hurt).await, segs);

    // Leftovers stay put when no chain verifies.
    let only_hurt: Vec<(ReplicaId, Dyn)> = vec![(id("hurt"), Arc::new(hurt.clone()))];
    let target = MemoryBackend::new();
    copy_all(&hurt, &target).await;
    assert!(
        restore(&only_hurt, &rk, &tid(), &target, None)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&target).await, segs);
    // With a verifiable chain they are replaced by it.
    let rep = restore(&replicas, &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!(rep.chosen, id("stale"));
    assert_eq!(snapshot(&target).await, snapshot(&stale).await);
}

/// A replica whose manifest was resealed by a holder of only the manifest subkey
/// (higher `seq`, a segment dropped) is broken, not the newest history.
#[tokio::test]
async fn restore_rejects_manifest_forged_with_only_manifest_key() {
    use vault_container::manifest::{self, Manifest};
    let (c, rk, _stale, current) = staged().await;
    let forged_b = MemoryBackend::new();
    copy_all(&current, &forged_b).await;
    let key = mkey(&c);
    let (cur, _) = forged_b.get("manifest").await.unwrap().unwrap();
    let m = manifest::open(&cur, &key).unwrap();
    let forged = Manifest {
        seq: m.seq + 5,
        prev: manifest::hash(&cur),
        segments: m.segments[..1].to_vec(),
        ..m.clone()
    };
    overwrite(
        &forged_b,
        "manifest",
        &manifest::seal(&forged, &key).unwrap(),
    )
    .await;

    let replicas: Vec<(ReplicaId, Dyn)> = vec![
        (id("forged"), Arc::new(forged_b)),
        (id("current"), Arc::new(current.clone())),
    ];
    let target = MemoryBackend::new();
    let rep = restore(&replicas, &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!((rep.chosen.clone(), rep.seq), (id("current"), 2));
    assert_eq!(rep.broken, vec![id("forged")]);
    assert_eq!(snapshot(&target).await, snapshot(&current).await);
}

/// A known mark at `u64::MAX` must not overflow while checking a candidate at it.
#[tokio::test]
async fn restore_with_max_seq_mark_does_not_overflow() {
    use vault_container::manifest::{self, Manifest};
    let (pb, c, rk) = primary().await;
    let rep_b = MemoryBackend::new();
    copy_all(&pb, &rep_b).await;
    let sub = c.crk().subkeys();
    let (cur, _) = rep_b.get("manifest").await.unwrap().unwrap();
    let m = manifest::open(&cur, &sub.manifest).unwrap();
    let mut max = Manifest {
        seq: u64::MAX,
        prev: manifest::hash(&cur),
        ..m
    };
    max.authenticate(&sub.manifest_auth).unwrap();
    let sealed = manifest::seal(&max, &sub.manifest).unwrap();
    overwrite(&rep_b, "manifest", &sealed).await;
    let hw = HighWater {
        seq: u64::MAX,
        hash: manifest::hash(&sealed),
    };
    let replicas: Vec<(ReplicaId, Dyn)> = vec![(id("r"), Arc::new(rep_b))];
    let rep = restore(&replicas, &rk, &tid(), &MemoryBackend::new(), Some(hw))
        .await
        .unwrap();
    assert_eq!(rep.seq, u64::MAX);
}

/// A primary whose header does not carry the manifest's wrapped CRK is refused
/// before anything reaches a replica.
#[tokio::test]
async fn primary_header_must_match_manifest() {
    let (pb, c, _) = primary().await;
    let foreign = MemoryBackend::new();
    Container::create(foreign.clone()).await.unwrap();
    let fh = foreign.get("header").await.unwrap().unwrap().0;
    overwrite(&pb, "header", &fh).await;
    let rep = MemoryBackend::new();
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![(id("r"), Arc::new(rep.clone()))],
        ReplicaState::default(),
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(
        matches!(outcome(&st, "r"), ReplicaOutcome::Failed { .. }),
        "{st:?}"
    );
    assert!(snapshot(&rep).await.is_empty());
}

/// A replica holding a manifest but no header is classified first: if it is
/// diverged, it does not get our header.
#[tokio::test]
async fn diverged_replica_without_header_gets_no_header() {
    let (pb, c, _) = primary().await;
    let foreign = MemoryBackend::new();
    Container::create(foreign.clone()).await.unwrap();
    let rep = MemoryBackend::new();
    let fm = foreign.get("manifest").await.unwrap().unwrap().0;
    rep.put_if_absent("manifest", &fm).await.unwrap();
    let mut set = ReplicaSet::new(
        Arc::new(pb),
        vec![(id("r"), Arc::new(rep.clone()))],
        ReplicaState::default(),
    );
    let st = set.replicate_once(&mkey(&c), ok).await;
    assert!(
        matches!(outcome(&st, "r"), ReplicaOutcome::Diverged { .. }),
        "{st:?}"
    );
    assert!(rep.get("header").await.unwrap().is_none());
    assert_eq!(snapshot(&rep).await, vec![("manifest".to_string(), fm)]);
}

/// A restore interrupted after the header but before the manifest left the
/// candidate's header and some segments: retried, it is wiped and copied again.
/// A target with any other header is refused and left alone.
#[tokio::test]
async fn restore_resumes_after_header_written_without_manifest() {
    let (_c, rk, stale, current) = staged().await;
    let replicas: Vec<(ReplicaId, Dyn)> = vec![(id("current"), Arc::new(current.clone()))];
    let target = MemoryBackend::new();
    copy_all(&current, &target).await;
    target.delete("manifest").await.unwrap();
    let rep = restore(&replicas, &rk, &tid(), &target, None)
        .await
        .unwrap();
    assert_eq!(rep.seq, 2);
    assert_eq!(snapshot(&target).await, snapshot(&current).await);

    // Header of another container, no manifest: not ours to wipe.
    let foreign = MemoryBackend::new();
    Container::create(foreign.clone()).await.unwrap();
    foreign.delete("manifest").await.unwrap();
    let before = snapshot(&foreign).await;
    assert!(matches!(
        restore(&replicas, &rk, &tid(), &foreign, None).await,
        Err(Error::Conflict)
    ));
    assert_eq!(snapshot(&foreign).await, before);

    // Our header but no chain verifies: nothing is wiped.
    let t2 = MemoryBackend::new();
    copy_all(&stale, &t2).await;
    t2.delete("manifest").await.unwrap();
    let before = snapshot(&t2).await;
    let broken = MemoryBackend::new();
    copy_all(&current, &broken).await;
    for name in broken.list("seg/").await.unwrap() {
        broken.delete(&name).await.unwrap();
    }
    let only_broken: Vec<(ReplicaId, Dyn)> = vec![(id("broken"), Arc::new(broken))];
    assert!(restore(&only_broken, &rk, &tid(), &t2, None).await.is_err());
    assert_eq!(snapshot(&t2).await, before);
}

/// State of replicas not passed to `ReplicaSet::new` is kept (e.g. one that is
/// temporarily not configured); only `forget_replica` drops it.
#[tokio::test]
async fn replica_set_keeps_state_of_absent_replicas() {
    let (pb, c, _) = primary().await;
    let mut state = ReplicaState::default();
    let gone = ReplicaRecord {
        last_seen: Some(c.high_water()),
        ..ReplicaRecord::default()
    };
    state.replicas.insert(id("gone"), gone.clone());
    let rep = MemoryBackend::new();
    let mut set = ReplicaSet::new(Arc::new(pb), vec![(id("r"), Arc::new(rep))], state);
    assert_synced(&set.replicate_once(&mkey(&c), ok).await, "r", 1);
    assert_eq!(set.state().replicas.get(&id("gone")), Some(&gone));
    assert!(set.forget_replica(&id("gone")));
    assert!(!set.state().replicas.contains_key(&id("gone")));
    assert!(!set.forget_replica(&id("gone")));
    assert!(set.state().replicas.contains_key(&id("r")));
}
