// SPDX-License-Identifier: MIT

#[path = "support/conformance.rs"]
mod conformance;
#[path = "support/mock_webdav.rs"]
mod mock_webdav;

use mock_webdav::{MockWebDav, Mode, PASSWORD, USER};
use vault_container::Error;
use vault_container::backend::{ContainerBackend, PutOutcome, WebDavBackend, WebDavConfig};
use zeroize::Zeroizing;

fn backend_for(mock: &MockWebDav) -> WebDavBackend {
    WebDavBackend::new(WebDavConfig {
        base_url: mock.base_url(),
        username: USER.into(),
        password: Zeroizing::new(PASSWORD.into()),
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap()
}

fn fresh(mode: Mode) -> (MockWebDav, WebDavBackend) {
    let m = MockWebDav::start(mode);
    let b = backend_for(&m);
    (m, b)
}

const SEG: &str = "seg/00112233445566778899aabbccddeeff";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_against_mock_webdav() {
    conformance::run_all(|| fresh(Mode::Enforce).1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_manifest_commit_conflicts() {
    conformance::concurrent_commit(fresh(Mode::Enforce).1).await;
}

#[tokio::test]
async fn probe_accepts_conforming_server() {
    let (m, b) = fresh(Mode::Enforce);
    b.probe_conditional_writes().await.unwrap();
    assert!(m.files().is_empty(), "probe object must be cleaned up");
}

#[tokio::test]
async fn probe_rejects_server_without_etag_preconditions() {
    let (m, b) = fresh(Mode::IgnoreConditions);
    match b.probe_conditional_writes().await {
        Err(Error::Unsupported(msg)) => assert_eq!(msg, "server ignores conditional writes"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert!(m.files().is_empty(), "probe object must be cleaned up");
}

#[tokio::test]
async fn probe_rejects_weak_etags() {
    let (m, b) = fresh(Mode::WeakEtags);
    assert!(matches!(
        b.probe_conditional_writes().await,
        Err(Error::Unsupported(_))
    ));
    assert!(m.files().is_empty(), "probe object must be cleaned up");
    assert!(matches!(
        b.put_if_absent("header", b"x").await,
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
async fn missing_etag_on_put_is_fetched_with_follow_up_get() {
    let (m, b) = fresh(Mode::NoEtagOnPut);
    let PutOutcome::Written(e) = b.put_if_absent("manifest", b"v1").await.unwrap() else {
        panic!("conflict")
    };
    let (data, etag) = b.get("manifest").await.unwrap().unwrap();
    assert_eq!((data.as_slice(), &etag), (&b"v1"[..], &e));
    // The etag obtained that way works for compare-and-swap, and the probe passes.
    assert!(matches!(
        b.put_if_match("manifest", b"v2", &e).await.unwrap(),
        PutOutcome::Written(_)
    ));
    b.probe_conditional_writes().await.unwrap();
    assert!(m.methods().iter().any(|x| x == "GET"));
}

#[tokio::test]
async fn follow_up_that_shows_other_content_is_an_error_not_an_etag() {
    // Guards against adopting another writer's etag as our own.
    let (_m, b) = fresh(Mode::NoEtagOnPutRacy);
    assert!(matches!(
        b.put_if_absent("manifest", b"mine").await,
        Err(Error::Io(_))
    ));
}

#[tokio::test]
async fn requests_carry_basic_auth_and_wrong_password_fails() {
    let (m, b) = fresh(Mode::Enforce);
    b.put_if_absent("header", b"x").await.unwrap();
    b.get("header").await.unwrap();
    b.list("seg/").await.unwrap();
    let want = format!(
        "Basic {}",
        data_encoding::BASE64.encode(format!("{USER}:{PASSWORD}").as_bytes())
    );
    let seen = m.authorizations();
    assert_eq!(
        seen.len(),
        m.request_count(),
        "every request is authenticated"
    );
    assert!(seen.iter().all(|a| *a == want));

    let bad = WebDavBackend::new(WebDavConfig {
        base_url: m.base_url(),
        username: USER.into(),
        password: Zeroizing::new("wrong".into()),
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap();
    assert!(matches!(bad.get("header").await, Err(Error::Io(_))));
    assert!(matches!(bad.list("seg/").await, Err(Error::Io(_))));
}

#[tokio::test]
async fn collections_are_created_lazily_and_recreated_if_removed() {
    let (m, b) = fresh(Mode::Enforce);
    assert_eq!(m.mkcol_count(), 0);
    // Reads and lists never create anything.
    assert!(b.get(SEG).await.unwrap().is_none());
    assert!(b.list("seg/").await.unwrap().is_empty());
    assert_eq!(m.mkcol_count(), 0);
    b.put_if_absent(SEG, b"a").await.unwrap();
    assert_eq!(m.mkcol_count(), 1);
    b.put_if_absent("seg/ffeeddccbbaa99887766554433221100", b"b")
        .await
        .unwrap();
    assert_eq!(m.mkcol_count(), 1, "existing collection is not re-created");
    // Someone removes seg/: the next write gets 409, re-creates it and retries.
    m.remove_collection("seg");
    b.put_if_absent(SEG, b"a").await.unwrap();
    assert_eq!(m.mkcol_count(), 2);
    assert_eq!(b.list("seg/").await.unwrap(), vec![SEG.to_string()]);
}

#[tokio::test]
async fn mkcol_405_means_already_exists() {
    let (m, b) = fresh(Mode::Enforce);
    m.plant("seg/ffeeddccbbaa99887766554433221100", b"b");
    // The collection exists already: MKCOL answers 405, which is fine.
    b.put_if_absent(SEG, b"a").await.unwrap();
    assert_eq!(m.mkcol_count(), 1);
    assert_eq!(b.list("seg/").await.unwrap().len(), 2);
}

#[tokio::test]
async fn list_decodes_hrefs_and_ignores_foreign_entries() {
    let (m, b) = fresh(Mode::Enforce);
    for i in 0..5u8 {
        b.put_if_absent(&format!("seg/{i:032x}"), b"x")
            .await
            .unwrap();
    }
    m.plant("seg/notes.txt", b"junk");
    m.plant("seg/00112233445566778899AABBCCDDEEFF", b"upper");
    m.plant("seg/sub/00112233445566778899aabbccddeeff", b"nested");
    let mut got = b.list("seg/").await.unwrap();
    got.sort();
    let want: Vec<String> = (0..5u8).map(|i| format!("seg/{i:032x}")).collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn hostile_propfind_responses_are_rejected() {
    let cases = [
        (Mode::ListHuge, "too large"),
        (Mode::ListTooMany, "too many"),
        (Mode::ListDoctype, "DOCTYPE"),
        (Mode::ListNotMultistatus, "multistatus"),
    ];
    for (mode, needle) in cases {
        let m = MockWebDav::start(mode);
        m.plant("seg/00112233445566778899aabbccddeeff", b"x");
        let b = backend_for(&m);
        match b.list("seg/").await {
            Err(Error::Io(s) | Error::Format(s)) => {
                assert!(s.contains(needle), "{mode:?}: {s}")
            }
            other => panic!("{mode:?}: expected error, got {other:?}"),
        }
        assert_eq!(m.request_count(), 1, "{mode:?}: single request");
    }
}

#[tokio::test]
async fn oversized_get_is_refused_and_missing_is_none() {
    let (m, b) = fresh(Mode::Enforce);
    assert!(b.get("manifest").await.unwrap().is_none());
    m.plant("manifest", &vec![0u8; (64 << 20) + 1]);
    assert!(matches!(b.get("manifest").await, Err(Error::Io(_))));
}

#[tokio::test]
async fn delete_missing_is_ok_and_names_are_validated() {
    let (_m, b) = fresh(Mode::Enforce);
    b.delete(SEG).await.unwrap();
    assert!(matches!(b.delete("../x").await, Err(Error::Format(_))));
    assert!(matches!(b.list("probe/").await, Err(Error::Format(_))));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    // A tiny raw server that answers every request with a redirect.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let _ = s
                .write_all(b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/x\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    let b = WebDavBackend::new(WebDavConfig {
        base_url: format!("http://{addr}/dav"),
        username: "u".into(),
        password: Zeroizing::new("p".into()),
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap();
    assert!(matches!(b.get("manifest").await, Err(Error::Io(_))));
}

#[tokio::test]
async fn debug_output_hides_credentials() {
    let (_m, b) = fresh(Mode::Enforce);
    let s = format!("{b:?}");
    assert!(!s.contains(PASSWORD) && !s.contains("Basic"), "{s}");
    let c = WebDavConfig {
        base_url: "https://x.example/dav".into(),
        username: "alice".into(),
        password: Zeroizing::new("hunter2".into()),
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    };
    let s = format!("{c:?}");
    assert!(!s.contains("alice") && !s.contains("hunter2"), "{s}");
}
