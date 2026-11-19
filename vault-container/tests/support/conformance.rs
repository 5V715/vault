// SPDX-License-Identifier: MIT

//! Backend conformance suite shared by every `ContainerBackend` implementation.

use std::sync::Arc;

use vault_container::Error;
use vault_container::backend::{ContainerBackend, PutOutcome};

const SEG_A: &str = "seg/00112233445566778899aabbccddeeff";
const SEG_B: &str = "seg/ffeeddccbbaa99887766554433221100";

fn written(o: PutOutcome) -> vault_container::backend::Etag {
    match o {
        PutOutcome::Written(e) => e,
        PutOutcome::Conflict => panic!("expected Written, got Conflict"),
    }
}

/// Runs every conformance check; `make` must return a fresh, empty backend each call.
pub async fn run_all<B: ContainerBackend + 'static>(make: impl Fn() -> B) {
    get_missing(&make()).await;
    put_if_absent(&make()).await;
    put_if_match(&make()).await;
    delete(&make()).await;
    list_only_segs(&make()).await;
    invalid_names(&make()).await;
    concurrent_commit(make()).await;
}

async fn get_missing<B: ContainerBackend>(b: &B) {
    assert!(b.get("manifest").await.unwrap().is_none());
    assert!(b.get(SEG_A).await.unwrap().is_none());
}

async fn put_if_absent<B: ContainerBackend>(b: &B) {
    let e = written(b.put_if_absent("header", b"one").await.unwrap());
    assert!(matches!(
        b.put_if_absent("header", b"two").await.unwrap(),
        PutOutcome::Conflict
    ));
    let (data, etag) = b.get("header").await.unwrap().unwrap();
    assert_eq!(data, b"one");
    assert_eq!(etag, e);
}

async fn put_if_match<B: ContainerBackend>(b: &B) {
    let e1 = written(b.put_if_absent("manifest", b"v1").await.unwrap());
    let e2 = written(b.put_if_match("manifest", b"v2", &e1).await.unwrap());
    assert_ne!(e1, e2);
    // stale etag
    assert!(matches!(
        b.put_if_match("manifest", b"v3", &e1).await.unwrap(),
        PutOutcome::Conflict
    ));
    let (data, etag) = b.get("manifest").await.unwrap().unwrap();
    assert_eq!(data, b"v2");
    assert_eq!(etag, e2);
    // put_if_match on a missing object conflicts
    assert!(matches!(
        b.put_if_match("header", b"x", &e1).await.unwrap(),
        PutOutcome::Conflict
    ));
}

async fn delete<B: ContainerBackend>(b: &B) {
    written(b.put_if_absent(SEG_A, b"s").await.unwrap());
    b.delete(SEG_A).await.unwrap();
    assert!(b.get(SEG_A).await.unwrap().is_none());
    // deleting a missing object is fine
    b.delete(SEG_A).await.unwrap();
    // and the name is free again
    written(b.put_if_absent(SEG_A, b"s2").await.unwrap());
}

async fn list_only_segs<B: ContainerBackend>(b: &B) {
    written(b.put_if_absent("header", b"h").await.unwrap());
    written(b.put_if_absent("manifest", b"m").await.unwrap());
    written(b.put_if_absent(SEG_B, b"b").await.unwrap());
    written(b.put_if_absent(SEG_A, b"a").await.unwrap());
    let mut names = b.list("seg/").await.unwrap();
    names.sort();
    assert_eq!(names, vec![SEG_A.to_string(), SEG_B.to_string()]);
    assert!(matches!(b.list("").await, Err(Error::Format(_))));
    assert!(matches!(b.list("probe/").await, Err(Error::Format(_))));
}

async fn invalid_names<B: ContainerBackend>(b: &B) {
    for bad in [
        "",
        "other",
        "../manifest",
        "seg/",
        "seg/ABCDEF",
        "seg/00112233445566778899AABBCCDDEEFF",
        "seg/0011",
        "seg/00112233445566778899aabbccddeeff/x",
        "/header",
        "header/",
        "probe/",
        "probe/zz",
    ] {
        assert!(
            matches!(b.get(bad).await, Err(Error::Format(_))),
            "get {bad:?}"
        );
        assert!(
            matches!(b.put_if_absent(bad, b"x").await, Err(Error::Format(_))),
            "put {bad:?}"
        );
        assert!(
            matches!(b.delete(bad).await, Err(Error::Format(_))),
            "delete {bad:?}"
        );
    }
    written(b.put_if_absent("probe/abcd01", b"p").await.unwrap());
}

/// Two writers race a manifest update from the same etag: exactly one wins.
pub async fn concurrent_commit<B: ContainerBackend + 'static>(b: B) {
    let b = Arc::new(b);
    for round in 0..20u8 {
        let base = match b.get("manifest").await.unwrap() {
            Some((_, e)) => e,
            None => written(b.put_if_absent("manifest", b"base").await.unwrap()),
        };
        let mk = |tag: u8| {
            let b = Arc::clone(&b);
            let base = base.clone();
            tokio::spawn(async move {
                b.put_if_match("manifest", &[round, tag], &base)
                    .await
                    .unwrap()
            })
        };
        let (x, y) = (mk(1), mk(2));
        let (x, y) = (x.await.unwrap(), y.await.unwrap());
        let wins = [&x, &y]
            .iter()
            .filter(|o| matches!(o, PutOutcome::Written(_)))
            .count();
        assert_eq!(wins, 1, "round {round}: exactly one writer must win");
    }
}
