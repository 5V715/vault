// SPDX-License-Identifier: MIT

//! `Arc<dyn ContainerBackend>` is itself a backend, and the engine, replication and
//! restore futures are `Send` (the vault drives them on a multi-threaded tokio runtime).

use std::sync::Arc;

use vault_container::backend::{ContainerBackend, MemoryBackend};
use vault_container::engine::Container;
use vault_container::replicate::{ReplicaId, ReplicaSet, ReplicaState, restore};

type Dyn = Arc<dyn ContainerBackend>;

fn assert_send<T: Send>(t: T) -> T {
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arc_dyn_backend_runs_on_spawned_tasks() {
    let mem = MemoryBackend::new();
    let b: Dyn = Arc::new(mem.clone());
    let (mut c, rk) = tokio::spawn(Container::create(
        Arc::clone(&b),
        "test-app",
        "https://app.example",
    ))
    .await
    .unwrap()
    .unwrap();
    c.put(b"k", b"v").unwrap();
    let mut c = tokio::spawn(async move {
        assert_send(c.commit()).await.unwrap();
        assert_send(c.compact()).await.unwrap();
        assert_send(c.gc_orphans()).await.unwrap();
        assert_send(c.reload()).await.unwrap();
        c
    })
    .await
    .unwrap();
    let hw = c.high_water();
    let key = c.crk().subkeys().manifest;
    let (_, crk) = assert_send(Container::open_with_recovery(Arc::clone(&b), &rk, Some(hw)))
        .await
        .unwrap();
    let c2 = assert_send(Container::open_with_crk(Arc::clone(&b), crk, Some(hw)))
        .await
        .unwrap();
    assert_eq!(c2.get(b"k").as_deref(), Some(&b"v"[..]));

    let replica: Dyn = Arc::new(MemoryBackend::new());
    let mut set = ReplicaSet::new(
        Arc::clone(&b),
        vec![(ReplicaId("r".into()), Arc::clone(&replica))],
        ReplicaState::default(),
    );
    let st = tokio::spawn(async move {
        assert_send(set.replicate_once(&key, |_: &ReplicaState| Ok(()))).await
    })
    .await
    .unwrap();
    assert_eq!(st.len(), 1);

    let target = MemoryBackend::new();
    let rep = tokio::spawn(async move {
        let replicas = vec![(ReplicaId("r".into()), replica)];
        assert_send(restore(
            &replicas,
            &rk,
            &ReplicaId("t".into()),
            &target,
            None,
        ))
        .await
        .map(|r| r.seq)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(rep, c.seq());
    c.put(b"x", b"y").unwrap();
}
