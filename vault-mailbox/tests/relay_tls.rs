// SPDX-License-Identifier: MIT
//! `vault-relay --tls-cert/--tls-key`: wss and HTTPS /healthz with a test CA,
//! flags that must pair, and key files that are refused. Certificates are
//! generated here with rcgen; nothing is checked in.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, SanType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;

const BIN: &str = env!("CARGO_BIN_EXE_vault-relay");
const CERT_ENV: &str = "VAULT_RELAY_TLS_CERT";
const KEY_ENV: &str = "VAULT_RELAY_TLS_KEY";

struct Pki {
    dir: tempfile::TempDir,
    ca_pem: String,
}

impl Pki {
    fn path(&self, n: &str) -> PathBuf {
        self.dir.path().join(n)
    }
}

/// A CA plus a leaf for 127.0.0.1 and localhost, as `ca.pem`, `leaf.pem`, `leaf.key`.
fn make_pki() -> Pki {
    let dir = tempfile::tempdir().unwrap();
    let ca_key = KeyPair::generate().unwrap();
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = p.self_signed(&ca_key).unwrap();
    let ca_params = p;
    let leaf_key = KeyPair::generate().unwrap();
    let mut p = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    p.subject_alt_names
        .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
    let leaf = p
        .signed_by(&leaf_key, &rcgen::Issuer::from_params(&ca_params, &ca_key))
        .unwrap();
    std::fs::write(dir.path().join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(dir.path().join("leaf.pem"), leaf.pem()).unwrap();
    std::fs::write(dir.path().join("leaf.key"), leaf_key.serialize_pem()).unwrap();
    Pki {
        ca_pem: ca.pem(),
        dir,
    }
}

fn relay(args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.args([
        "--listen",
        "127.0.0.1:0",
        "--allow-origin",
        "https://app.example",
    ])
    .args(args);
    c.env_remove(CERT_ENV)
        .env_remove(KEY_ENV)
        .env_remove("VAULT_RELAY_ALLOW_ORIGIN")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info");
    c
}

fn tls_args(pki: &Pki) -> Vec<String> {
    vec![
        "--tls-cert".into(),
        pki.path("leaf.pem").display().to_string(),
        "--tls-key".into(),
        pki.path("leaf.key").display().to_string(),
    ]
}

fn refused(mut c: Command) -> Output {
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            let out = child.wait_with_output().unwrap();
            panic!(
                "vault-relay started: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn started(mut c: Command) -> (Child, String) {
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let addr = loop {
        let line = lines.next().expect("relay exited").unwrap();
        if let Some((_, rest)) = line.split_once("addr=") {
            break rest.split_whitespace().next().unwrap().to_string();
        }
    };
    std::thread::spawn(move || for _ in lines {});
    (child, addr)
}

fn connector(ca_pem: &str) -> TlsConnector {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pemfile_certs(ca_pem) {
        roots.add(c).unwrap();
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    TlsConnector::from(Arc::new(cfg))
}

fn rustls_pemfile_certs(
    pem: &str,
) -> Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>> {
    use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject};
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .map(|c| c.unwrap())
        .collect()
}

async fn dial(
    c: &TlsConnector,
    addr: &str,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    c.connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
}

#[tokio::test]
async fn relay_tls_serves_wss() {
    let pki = make_pki();
    let args = tls_args(&pki);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (mut child, addr) = started(relay(&args));
    let conn = connector(&pki.ca_pem);

    // /healthz over HTTPS.
    let mut tls = dial(&conn, &addr).await.unwrap();
    assert_eq!(
        tls.get_ref().1.protocol_version(),
        Some(tokio_rustls::rustls::ProtocolVersion::TLSv1_3)
    );
    tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut body = String::new();
    let _ = tls.read_to_string(&mut body).await;
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    assert!(body.ends_with("ok"), "{body}");

    // wss:// /mailbox with an allowed Origin and with a foreign one.
    for (origin, want) in [
        ("https://app.example", 101u16),
        ("https://evil.example", 403),
    ] {
        use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
        let tls = dial(&conn, &addr).await.unwrap();
        let mut req = format!(
            "wss://localhost:{}/mailbox/0123456789abcdef0123456789abcdef",
            addr.rsplit(':').next().unwrap()
        )
        .into_client_request()
        .unwrap();
        req.headers_mut().insert("origin", origin.parse().unwrap());
        let got = match tokio_tungstenite::client_async(req, tls).await {
            Ok((_ws, resp)) => resp.status().as_u16(),
            Err(tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            Err(e) => panic!("{e:?}"),
        };
        assert_eq!(got, want, "{origin}");
    }

    // A client that does not trust the CA is refused; plain HTTP gets no answer.
    let none = connector("");
    assert!(dial(&none, &addr).await.is_err());
    let mut tcp = tokio::net::TcpStream::connect(&addr).await.unwrap();
    tcp.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut buf)).await;
    assert!(!String::from_utf8_lossy(&buf).contains("200 OK"));

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn relay_tls_flags_must_pair() {
    let pki = make_pki();
    let cert = pki.path("leaf.pem").display().to_string();
    let key = pki.path("leaf.key").display().to_string();
    let mut cases: Vec<Command> = vec![relay(&["--tls-cert", &cert]), relay(&["--tls-key", &key])];
    let mut c = relay(&[]);
    c.env(CERT_ENV, &cert);
    cases.push(c);
    let mut c = relay(&[]);
    c.env(KEY_ENV, &key);
    cases.push(c);
    for c in cases {
        let out = refused(c);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{err}");
        assert!(
            err.contains("--tls-cert and --tls-key must be given together"),
            "{err}"
        );
    }
}

#[tokio::test]
async fn relay_tls_from_env() {
    let pki = make_pki();
    let mut c = relay(&[]);
    c.env(CERT_ENV, pki.path("leaf.pem"))
        .env(KEY_ENV, pki.path("leaf.key"));
    let (mut child, addr) = started(c);
    assert!(dial(&connector(&pki.ca_pem), &addr).await.is_ok());
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn relay_tls_bad_key_refused() {
    let pki = make_pki();
    let other = make_pki(); // a key that does not belong to the certificate
    let cert = pki.path("leaf.pem").display().to_string();
    let dir = pki.dir.path();
    std::fs::write(dir.join("empty.pem"), "").unwrap();
    std::fs::write(dir.join("junk.pem"), "not a pem").unwrap();
    std::fs::write(
        dir.join("cert-as-key.pem"),
        std::fs::read(pki.path("leaf.pem")).unwrap(),
    )
    .unwrap();
    let wrong = other.path("leaf.key");
    let p = |n: &str| dir.join(n).display().to_string();
    let missing = p("missing.key");
    let cases: Vec<(String, String, &str)> = vec![
        (cert.clone(), wrong.display().to_string(), "does not match"),
        (cert.clone(), p("empty.pem"), "no private key"),
        (cert.clone(), p("junk.pem"), "no private key"),
        (cert.clone(), p("cert-as-key.pem"), "no private key"),
        (cert.clone(), missing, "cannot read"),
        (
            p("empty.pem"),
            pki.path("leaf.key").display().to_string(),
            "no certificate",
        ),
        (
            p("missing.pem"),
            pki.path("leaf.key").display().to_string(),
            "cannot read",
        ),
    ];
    for (c, k, needle) in cases {
        let out = refused(relay(&["--tls-cert", &c, "--tls-key", &k]));
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{needle}: {err}");
        assert!(err.contains(needle), "{needle}: {err}");
        assert!(!err.contains("PRIVATE KEY"), "{err}");
    }
}

#[cfg(unix)]
#[test]
fn relay_tls_warns_on_readable_key() {
    use std::os::unix::fs::PermissionsExt;
    let pki = make_pki();
    let key: &Path = &pki.path("leaf.key");
    let run = |mode: u32| {
        std::fs::set_permissions(key, std::fs::Permissions::from_mode(mode)).unwrap();
        let args = tls_args(&pki);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut c = relay(&args);
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut seen = String::new();
        while let Some(Ok(l)) = lines.next() {
            seen.push_str(&l);
            seen.push('\n');
            if l.contains("listening") {
                break;
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        seen
    };
    assert!(run(0o644).contains("readable by other users"));
    assert!(!run(0o600).contains("readable by other users"));
}

/// A client that never finishes the TLS handshake holds one connection slot (the
/// cap covers TLS) and is cut by the stall timeout; then the next client is served.
#[tokio::test]
async fn tls_handshake_stall_is_cut_inside_the_connection_cap() {
    let pki = make_pki();
    let cfg = vault_mailbox::tls_server_config(
        &std::fs::read(pki.path("leaf.pem")).unwrap(),
        &std::fs::read(pki.path("leaf.key")).unwrap(),
    )
    .unwrap();
    let mailboxes = vault_mailbox::Mailboxes::new(vault_mailbox::MailboxConfig {
        max_connections: 1,
        static_timeout: Duration::from_millis(700),
        ..Default::default()
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(vault_mailbox::serve_tls(
        l,
        mailboxes,
        Some(cfg),
        async move {
            let _ = rx.await;
        },
    ));
    let mut stalled = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stalled.write_all(&[0x16, 0x03]).await.unwrap(); // half a record header, then silence
    let t0 = std::time::Instant::now();
    // The second client has to wait for the slot, then handshakes normally.
    let conn = connector(&pki.ca_pem);
    let mut tls = tokio::time::timeout(Duration::from_secs(5), dial(&conn, &addr))
        .await
        .expect("second client starved")
        .unwrap();
    assert!(
        t0.elapsed() >= Duration::from_millis(500),
        "cap not applied"
    );
    tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut body = String::new();
    let _ = tls.read_to_string(&mut body).await;
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    let mut buf = [0u8; 8];
    assert!(matches!(stalled.read(&mut buf).await, Ok(0) | Err(_)));
    let _ = tx.send(());
}

/// A TLS variable that is set but empty (a typo in a unit file or a compose
/// `VAULT_RELAY_TLS_CERT=`) is a startup error, never a silent fall back to plain
/// HTTP; so is an empty `--tls-cert`/`--tls-key` value.
#[test]
fn empty_tls_env_is_error() {
    let pki = make_pki();
    let cert = pki.path("leaf.pem").display().to_string();
    let key = pki.path("leaf.key").display().to_string();
    let mut cases: Vec<(Command, &str)> = Vec::new();
    for (env, other_env, other) in [(CERT_ENV, KEY_ENV, &key), (KEY_ENV, CERT_ENV, &cert)] {
        let mut c = relay(&[]);
        c.env(env, "");
        cases.push((c, env));
        let mut c = relay(&[]);
        c.env(env, "").env(other_env, other);
        cases.push((c, env));
    }
    let mut c = relay(&[]);
    c.env(CERT_ENV, "").env(KEY_ENV, "");
    cases.push((c, CERT_ENV));
    cases.push((relay(&["--tls-cert", "", "--tls-key", &key]), "--tls-cert"));
    cases.push((relay(&["--tls-cert", &cert, "--tls-key", ""]), "--tls-key"));
    for (c, needle) in cases {
        let out = refused(c);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{needle}: {err}");
        assert!(err.contains(needle), "{needle}: {err}");
        assert!(err.contains("empty"), "{needle}: {err}");
    }
}
