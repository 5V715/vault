// SPDX-License-Identifier: MIT

#[path = "support/conformance.rs"]
mod conformance;

use std::sync::Arc;
use vault_container::backend::{ContainerBackend, LocalBackend, PutOutcome};

/// Yields a fresh backend over a fresh temp dir; dirs live until the process ends.
fn make() -> LocalBackend {
    let dir = tempfile::tempdir().unwrap().keep();
    LocalBackend::open_blocking(dir).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_put_if_absent() {
    conformance::run_all(make).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_manifest_commit_conflicts() {
    conformance::concurrent_commit(make()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_commit_across_instances() {
    // Two independent handles on the same directory (stand-in for two processes).
    let dir = tempfile::tempdir().unwrap();
    let a = Arc::new(LocalBackend::open(dir.path()).await.unwrap());
    let b = Arc::new(LocalBackend::open(dir.path()).await.unwrap());
    let PutOutcome::Written(base) = a.put_if_absent("manifest", b"base").await.unwrap() else {
        panic!()
    };
    let (ta, tb) = {
        let (a, base) = (a.clone(), base.clone());
        let (b, base2) = (b.clone(), base.clone());
        (
            tokio::spawn(async move { a.put_if_match("manifest", b"A", &base).await.unwrap() }),
            tokio::spawn(async move { b.put_if_match("manifest", b"B", &base2).await.unwrap() }),
        )
    };
    let wins = [ta.await.unwrap(), tb.await.unwrap()]
        .iter()
        .filter(|o| matches!(o, PutOutcome::Written(_)))
        .count();
    assert_eq!(wins, 1);
}

#[tokio::test]
async fn local_atomic_write_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    let backend = LocalBackend::open(dir.path()).await.unwrap();
    let PutOutcome::Written(e1) = backend
        .put_if_absent("manifest", b"committed")
        .await
        .unwrap()
    else {
        panic!()
    };
    drop(backend);

    // Simulate a crash between temp write and rename: stray temp files, no rename.
    std::fs::write(dir.path().join(".tmp-deadbeef"), b"partial manifest").unwrap();
    std::fs::create_dir_all(dir.path().join("seg")).unwrap();
    std::fs::write(dir.path().join("seg").join(".tmp-cafe"), b"partial seg").unwrap();

    let backend = LocalBackend::open(dir.path()).await.unwrap();
    // old content intact, temp files neither visible nor left behind
    let (data, etag) = backend.get("manifest").await.unwrap().unwrap();
    assert_eq!(data, b"committed");
    assert_eq!(etag, e1);
    assert!(backend.list("seg/").await.unwrap().is_empty());
    assert!(!dir.path().join(".tmp-deadbeef").exists());
    assert!(!dir.path().join("seg").join(".tmp-cafe").exists());
    // and the store is still writable
    assert!(matches!(
        backend
            .put_if_match("manifest", b"next", &e1)
            .await
            .unwrap(),
        PutOutcome::Written(_)
    ));
}

#[tokio::test]
async fn local_etag_is_sha256_hex() {
    let dir = tempfile::tempdir().unwrap();
    let backend = LocalBackend::open(dir.path()).await.unwrap();
    let PutOutcome::Written(e) = backend.put_if_absent("header", b"abc").await.unwrap() else {
        panic!()
    };
    assert_eq!(
        e.0,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[tokio::test]
async fn local_open_cleans_probe_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("probe")).unwrap();
    std::fs::write(dir.path().join("probe").join(".tmp-beef"), b"partial probe").unwrap();
    std::fs::write(dir.path().join("probe").join("abcd"), b"probe object").unwrap();
    let backend = LocalBackend::open(dir.path()).await.unwrap();
    assert!(!dir.path().join("probe").join(".tmp-beef").exists());
    // real probe objects are left alone
    assert_eq!(
        backend.get("probe/abcd").await.unwrap().unwrap().0,
        b"probe object"
    );
}

#[tokio::test]
async fn local_open_creates_seg_and_probe_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("fresh");
    LocalBackend::open(&root).await.unwrap();
    assert!(root.join("seg").is_dir());
    assert!(root.join("probe").is_dir());
}
