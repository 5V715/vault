// SPDX-License-Identifier: MIT

#[path = "support/conformance.rs"]
mod conformance;

use vault_container::backend::MemoryBackend;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_put_if_absent() {
    conformance::run_all(MemoryBackend::new).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_manifest_commit_conflicts() {
    conformance::concurrent_commit(MemoryBackend::new()).await;
}

#[tokio::test]
async fn clones_share_state() {
    use vault_container::backend::ContainerBackend;
    let a = MemoryBackend::new();
    let b = a.clone();
    a.put_if_absent("header", b"x").await.unwrap();
    assert!(b.get("header").await.unwrap().is_some());
}
