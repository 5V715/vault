// SPDX-License-Identifier: MIT

#[path = "support/conformance.rs"]
mod conformance;
#[path = "support/mock_s3.rs"]
mod mock_s3;

use mock_s3::{MockS3, Mode};
use vault_container::Error;
use vault_container::backend::{ContainerBackend, PutOutcome, S3Backend, S3Config};
use zeroize::Zeroizing;

fn backend_for(mock: &MockS3, prefix: &str) -> S3Backend {
    S3Backend::new(S3Config {
        endpoint: mock.endpoint(),
        region: "us-east-1".into(),
        bucket: "vault".into(),
        prefix: prefix.into(),
        access_key: "AKIAEXAMPLE".into(),
        secret_key: Zeroizing::new("secret".into()),
        path_style: true,
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap()
}

fn fresh(mode: Mode, page: usize) -> (MockS3, S3Backend) {
    let m = MockS3::start("vault", mode, page);
    let b = backend_for(&m, "tenant/one");
    (m, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_against_mock_s3() {
    // Page size 2 forces continuation tokens in the list check.
    conformance::run_all(|| fresh(Mode::Enforce, 2).1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_manifest_commit_conflicts() {
    conformance::concurrent_commit(fresh(Mode::Enforce, 2).1).await;
}

#[tokio::test]
async fn list_paginates_and_is_scoped_to_prefix() {
    let m = MockS3::start("vault", Mode::Enforce, 3);
    let a = backend_for(&m, "a");
    let b = backend_for(&m, "ab");
    let mut want = Vec::new();
    for i in 0..10u8 {
        let n = format!("seg/{i:032x}");
        a.put_if_absent(&n, b"x").await.unwrap();
        want.push(n);
    }
    b.put_if_absent("seg/ffffffffffffffffffffffffffffffff", b"y")
        .await
        .unwrap();
    let mut got = a.list("seg/").await.unwrap();
    got.sort();
    assert_eq!(got, want);
    assert_eq!(b.list("seg/").await.unwrap().len(), 1);
    assert!(m.keys().contains(&format!("a/{}", want[0])));
}

#[tokio::test]
async fn probe_accepts_conforming_provider() {
    let (m, b) = fresh(Mode::Enforce, 10);
    b.probe_conditional_writes().await.unwrap();
    assert!(m.keys().is_empty(), "probe object must be cleaned up");
}

#[tokio::test]
async fn probe_rejects_provider_without_conditional_writes() {
    let (m, b) = fresh(Mode::IgnoreConditionals, 10);
    match b.probe_conditional_writes().await {
        Err(Error::Unsupported(msg)) => assert_eq!(msg, "provider ignores conditional writes"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert!(m.keys().is_empty(), "probe object must be cleaned up");
}

#[tokio::test]
async fn conditional_request_conflict_409_is_conflict() {
    let (_m, b) = fresh(Mode::Conflict409, 10);
    assert!(matches!(
        b.put_if_absent("header", b"x").await.unwrap(),
        PutOutcome::Conflict
    ));
}

#[tokio::test]
async fn server_errors_are_io_errors_without_retry() {
    let (m, b) = fresh(Mode::Fail(500), 10);
    assert!(matches!(b.get("manifest").await, Err(Error::Io(_))));
    assert!(matches!(
        b.put_if_absent("header", b"x").await,
        Err(Error::Io(_))
    ));
    assert_eq!(m.request_count(), 2, "backend must not retry");
}

#[tokio::test]
async fn missing_bucket_is_an_error_not_absence() {
    let m = MockS3::start("other-bucket", Mode::Enforce, 10);
    let b = backend_for(&m, "");
    assert!(matches!(b.get("manifest").await, Err(Error::Io(_))));
    assert!(matches!(b.delete("manifest").await, Err(Error::Io(_))));
}

#[tokio::test]
async fn not_implemented_maps_to_unsupported() {
    let (_m, b) = fresh(Mode::Fail(501), 10);
    assert!(matches!(
        b.put_if_absent("header", b"x").await,
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
async fn mock_rejects_unsigned_requests() {
    // Guards the mock itself: conformance passing must imply signed requests.
    let m = MockS3::start("vault", Mode::Enforce, 10);
    let r = reqwest::Client::new()
        .get(format!("{}/vault/manifest", m.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let (_m, b) = fresh(Mode::Oversize, 10);
    assert!(matches!(b.get("manifest").await, Err(Error::Io(_))));
    assert!(matches!(b.list("seg/").await, Err(Error::Io(_))));
}

#[tokio::test]
async fn list_rejects_stuck_alternating_and_endless_tokens() {
    for mode in [Mode::StuckList, Mode::AlternatingList, Mode::EndlessList] {
        let (m, b) = fresh(mode, 10);
        assert!(
            matches!(b.list("seg/").await, Err(Error::Io(_))),
            "{mode:?}"
        );
        if mode == Mode::EndlessList {
            assert!(m.request_count() <= 10_000, "page cap");
        } else {
            assert!(m.request_count() <= 4, "{mode:?}: repeated token must stop");
        }
    }
}
