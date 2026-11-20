// SPDX-License-Identifier: MIT
//! `vault-relay --static-dir`: the release to-do page served next to `/mailbox` on
//! one port (the `vault-todo-demo` image). Read-only GET/HEAD of regular files
//! under the directory, the page's exact CSP computed from the public and relay
//! origins, and `/config.json` telling the page its relay. Off unless asked for.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use vault_mailbox::{MailboxConfig, Mailboxes, ServeOptions, StaticSite};

const BIN: &str = env!("CARGO_BIN_EXE_vault-relay");
const ID: &str = "0123456789abcdef0123456789abcdef";
const INDEX: &str = "<!doctype html><title>to-do</title>";
const SECRET: &str = "SECRET-OUTSIDE-THE-ROOT";

/// The exact policy for a page at https://todo.example with its relay on the same host.
const TODO_CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
style-src 'self'; img-src 'self' data:; connect-src 'self' wss://todo.example; \
base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// tmp/site (the root: index.html, main.js, styles.css, app.wasm, sub/a.css,
/// .build.json, notes.txt) and tmp/secret.html next to it (must never be served).
fn site() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("index.html"), INDEX).unwrap();
    std::fs::write(root.join("main.js"), "export {};").unwrap();
    std::fs::write(root.join("styles.css"), "body{}").unwrap();
    std::fs::write(root.join("app.wasm"), b"\0asm").unwrap();
    std::fs::write(root.join("sub/a.css"), "a{}").unwrap();
    std::fs::write(root.join(".build.json"), "{}").unwrap();
    std::fs::write(root.join("notes.txt"), "no type").unwrap();
    std::fs::write(tmp.path().join("secret.html"), SECRET).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("../secret.html", root.join("leak.html")).unwrap();
        symlink("..", root.join("up")).unwrap();
        symlink("/etc", root.join("etc")).unwrap();
        symlink("index.html", root.join("alias.html")).unwrap();
    }
    tmp
}

struct Resp {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One raw HTTP/1.1 request (the target is sent byte for byte, unnormalized).
async fn request(addr: &str, method: &str, target: &str, extra: &[(&str, &str)]) -> Resp {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req =
        format!("{method} {target} HTTP/1.1\r\nHost: todo.example\r\nConnection: close\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut raw))
        .await
        .expect("response timed out")
        .unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end: {:?}", String::from_utf8_lossy(&raw)));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Resp {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

async fn get(addr: &str, target: &str) -> Resp {
    request(addr, "GET", target, &[]).await
}

/// An in-process relay with the static site at `root` (or none).
async fn start(
    root: Option<&Path>,
    cfg: MailboxConfig,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let static_site = root.map(|r| {
        Arc::new(StaticSite::new(r, "https://todo.example", "wss://todo.example").unwrap())
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let opts = ServeOptions {
        static_site,
        ..Default::default()
    };
    tokio::spawn(vault_mailbox::serve_with(
        l,
        Mailboxes::new(cfg),
        opts,
        async move {
            let _ = rx.await;
        },
    ));
    (addr, tx)
}

fn todo_cfg() -> MailboxConfig {
    MailboxConfig {
        allowed_origins: vec!["https://todo.example".into()],
        ..Default::default()
    }
}

async fn ws_status(addr: &str, origin: Option<&str>) -> u16 {
    let mut req = format!("ws://{addr}/mailbox/{ID}")
        .into_client_request()
        .unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((_ws, resp)) => resp.status().as_u16(),
        Err(tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("{e:?}"),
    }
}

#[tokio::test]
async fn static_dir_refuses_traversal() {
    let tmp = site();
    let (addr, _stop) = start(Some(&tmp.path().join("site")), todo_cfg()).await;
    let mut targets = vec![
        "/../secret.html",
        "/sub/../../secret.html",
        "/%2e%2e/secret.html",
        "/%2E%2E/secret.html",
        "/.%2e/secret.html",
        "/%2e%2e%2fsecret.html",
        "/sub%2f..%2f..%2fsecret.html",
        "/..%5csecret.html",
        "/..\\secret.html",
        "/sub\\..\\..\\secret.html",
        "//secret.html",
        "/./index.html",
        "/index.html%00.js",
        "/.build.json",
        "/notes.txt",
        "/sub",
        "/sub/",
        "/etc/passwd",
        "http://todo.example/../secret.html",
        "/%69ndex.html",
    ];
    if cfg!(unix) {
        targets.extend([
            "/leak.html",
            "/up/secret.html",
            "/etc/hostname",
            "/alias.html",
        ]);
    }
    for t in targets {
        let r = get(&addr, t).await;
        assert!(
            r.status == 404 || r.status == 400,
            "{t}: status {} body {:?}",
            r.status,
            r.text()
        );
        assert!(
            !r.text().contains(SECRET),
            "{t}: leaked the file outside the root"
        );
        assert!(
            !r.text().contains(INDEX),
            "{t}: served a file for a refused path"
        );
    }
    // The plain names still work.
    assert_eq!(get(&addr, "/sub/a.css").await.status, 200);
    assert_eq!(get(&addr, "/index.html").await.status, 200);
}

#[tokio::test]
async fn static_dir_serves_csp() {
    let tmp = site();
    let (addr, _stop) = start(Some(&tmp.path().join("site")), todo_cfg()).await;
    let page_headers = |r: &Resp, what: &str| {
        assert_eq!(
            r.header("content-security-policy"),
            Some(TODO_CSP),
            "{what}"
        );
        assert_eq!(
            r.header("x-content-type-options"),
            Some("nosniff"),
            "{what}"
        );
        assert_eq!(r.header("referrer-policy"), Some("no-referrer"), "{what}");
        assert_eq!(
            r.header("cross-origin-opener-policy"),
            Some("same-origin"),
            "{what}"
        );
    };
    let r = get(&addr, "/").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), INDEX);
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    page_headers(&r, "/");
    for (path, ty) in [
        ("/main.js", "text/javascript; charset=utf-8"),
        ("/styles.css", "text/css; charset=utf-8"),
        ("/app.wasm", "application/wasm"),
        ("/config.json", "application/json"),
    ] {
        let r = get(&addr, path).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("content-type"), Some(ty), "{path}");
        page_headers(&r, path);
    }
    let head = request(&addr, "HEAD", "/", &[]).await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty(), "HEAD has no body");
    assert_eq!(
        head.header("content-length"),
        Some(INDEX.len().to_string().as_str())
    );
    page_headers(&head, "HEAD /");
    let r = get(&addr, "/missing.js").await;
    assert_eq!(r.status, 404);
    page_headers(&r, "404");
    for m in ["POST", "PUT", "DELETE", "OPTIONS"] {
        let r = request(&addr, m, "/", &[("Content-Length", "0")]).await;
        assert_eq!(r.status, 405, "{m}");
        assert_eq!(r.header("allow"), Some("GET, HEAD"), "{m}");
        page_headers(&r, m);
    }
}

#[tokio::test]
async fn mailbox_and_static_share_port() {
    let tmp = site();
    let (addr, _stop) = start(Some(&tmp.path().join("site")), todo_cfg()).await;
    assert_eq!(get(&addr, "/").await.status, 200);
    assert_eq!(get(&addr, "/healthz").await.status, 200);
    assert_eq!(
        get(&addr, "/healthz")
            .await
            .header("content-security-policy"),
        None,
        "relay responses are unchanged"
    );
    assert_eq!(ws_status(&addr, Some("https://todo.example")).await, 101);
    assert_eq!(ws_status(&addr, Some("https://evil.example")).await, 403);
    // A plain GET of a mailbox path is still the relay's (no upgrade: 400), not a file.
    assert_eq!(get(&addr, &format!("/mailbox/{ID}")).await.status, 400);
    assert_eq!(
        request(&addr, "POST", "/healthz", &[("Content-Length", "0")])
            .await
            .status,
        405
    );
}

#[test]
fn static_site_rejects_unsafe_origins() {
    let tmp = site();
    let root = tmp.path().join("site");
    for (page, relay) in [
        ("https://todo.example", "wss://todo.example; script-src *"),
        ("https://todo.example", "wss://todo.example/mailbox"),
        ("https://todo.example", "https://todo.example"),
        ("https://todo.example 'unsafe-inline'", "wss://todo.example"),
        ("HTTPS://todo.example", "wss://todo.example"),
        ("https://todo.example", "wss://Todo.example"),
        ("ftp://todo.example", "wss://todo.example"),
    ] {
        assert!(
            StaticSite::new(&root, page, relay).is_err(),
            "{page} {relay}"
        );
    }
    assert!(
        StaticSite::new(
            &tmp.path().join("missing"),
            "https://a.example",
            "wss://a.example"
        )
        .is_err()
    );
    assert!(
        StaticSite::new(
            &root.join("index.html"),
            "https://a.example",
            "wss://a.example"
        )
        .is_err()
    );
}

// ---- the binary

const ENVS: [&str; 5] = [
    "VAULT_RELAY_ALLOW_ORIGIN",
    "VAULT_RELAY_STATIC_DIR",
    "PUBLIC_ORIGIN",
    "RELAY_ORIGIN",
    "VAULT_RELAY_TLS_CERT",
];

fn relay(args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(BIN);
    c.args(["--listen", "127.0.0.1:0"]).args(args);
    for e in ENVS {
        c.env_remove(e);
    }
    c.env_remove("VAULT_RELAY_TLS_KEY")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info");
    for (k, v) in env {
        c.env(k, v);
    }
    c
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
            panic!("vault-relay started");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn started(mut c: Command) -> (Running, String) {
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
    (Running(child), addr)
}

#[tokio::test]
async fn static_dir_off_by_default() {
    let tmp = site();
    // PUBLIC_ORIGIN alone (no --static-dir) serves no files.
    for env in [
        vec![("VAULT_RELAY_ALLOW_ORIGIN", "https://todo.example")],
        vec![("PUBLIC_ORIGIN", "https://todo.example")],
    ] {
        let (_relay, addr) = started(relay(&[], &env));
        for path in ["/", "/index.html", "/config.json"] {
            let r = get(&addr, path).await;
            assert_eq!(r.status, 404, "{path}");
            assert_eq!(r.header("content-security-policy"), None, "{path}");
        }
        assert_eq!(get(&addr, "/healthz").await.status, 200);
    }
    // --static-dir needs the public origin, and says so.
    let dir = tmp.path().join("site").display().to_string();
    for c in [
        relay(
            &["--static-dir", &dir],
            &[("VAULT_RELAY_ALLOW_ORIGIN", "https://todo.example")],
        ),
        relay(&[], &[("VAULT_RELAY_STATIC_DIR", &dir)]),
    ] {
        let out = refused(c);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{err}");
        assert!(
            err.contains("PUBLIC_ORIGIN") && err.contains("--public-origin"),
            "{err}"
        );
    }
    // Set but empty is an error, not "off".
    for (k, needle) in [
        ("VAULT_RELAY_STATIC_DIR", "--static-dir"),
        ("PUBLIC_ORIGIN", "--public-origin"),
        ("RELAY_ORIGIN", "--relay-origin"),
    ] {
        let out = refused(relay(
            &[],
            &[
                ("VAULT_RELAY_ALLOW_ORIGIN", "https://todo.example"),
                (k, ""),
            ],
        ));
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{k}: {err}");
        assert!(err.contains(needle) && err.contains("empty"), "{k}: {err}");
    }
    // A missing directory fails at startup.
    let missing = tmp.path().join("nope").display().to_string();
    let out = refused(relay(
        &["--static-dir", &missing],
        &[("PUBLIC_ORIGIN", "https://todo.example")],
    ));
    assert_eq!(out.status.code(), Some(2));
}

#[tokio::test]
async fn config_json_matches_relay_origin() {
    let tmp = site();
    let dir = tmp.path().join("site").display().to_string();
    let csp_for = |relay: &str| TODO_CSP.replace("wss://todo.example", relay);
    for (env, want) in [
        (
            vec![("PUBLIC_ORIGIN", "https://todo.example")],
            "wss://todo.example",
        ),
        (
            vec![("PUBLIC_ORIGIN", "HTTPS://Todo.Example:443")],
            "wss://todo.example",
        ),
        (
            vec![
                ("PUBLIC_ORIGIN", "https://todo.example"),
                ("RELAY_ORIGIN", "wss://relay.example:8443"),
            ],
            "wss://relay.example:8443",
        ),
        (
            vec![
                ("PUBLIC_ORIGIN", "https://todo.example"),
                ("RELAY_ORIGIN", "https://Relay.Example"),
            ],
            "wss://relay.example",
        ),
        (
            vec![("PUBLIC_ORIGIN", "http://localhost:8080")],
            "ws://localhost:8080",
        ),
    ] {
        let (_relay, addr) = started(relay(&["--static-dir", &dir], &env));
        let r = get(&addr, "/config.json").await;
        assert_eq!(r.status, 200, "{env:?}");
        assert_eq!(r.text(), format!("{{\"relay\":\"{want}\"}}\n"), "{env:?}");
        assert_eq!(r.header("cache-control"), Some("no-store"));
        let page = get(&addr, "/").await;
        let csp = csp_for(want);
        assert_eq!(
            page.header("content-security-policy"),
            Some(csp.as_str()),
            "{env:?}"
        );
    }
    // The page origin is the default allowlist.
    let (_relay, addr) = started(relay(
        &["--static-dir", &dir],
        &[("PUBLIC_ORIGIN", "https://todo.example")],
    ));
    assert_eq!(ws_status(&addr, Some("https://todo.example")).await, 101);
    assert_eq!(ws_status(&addr, Some("https://evil.example")).await, 403);
    // Mixed content and bad origins are refused at startup.
    for (page, relay_origin) in [
        ("https://todo.example", "ws://localhost:8787"),
        ("https://todo.example", "wss://relay.example/mailbox"),
        ("http://localhost:8080", "wss://relay.example"),
        ("http://todo.example", "ws://todo.example"),
        ("https://todo.example/app", "wss://todo.example"),
    ] {
        let out = refused(relay(
            &["--static-dir", &dir],
            &[("PUBLIC_ORIGIN", page), ("RELAY_ORIGIN", relay_origin)],
        ));
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{page} {relay_origin}: {err}");
    }
}

/// Files are streamed with their exact length; a file above the cap is not served.
#[tokio::test]
async fn static_dir_streams_large_files_with_a_cap() {
    let tmp = site();
    let root = tmp.path().join("site");
    let data: Vec<u8> = (0..(1024 * 1024 + 7)).map(|i| (i % 253) as u8).collect();
    std::fs::write(root.join("big.js"), &data).unwrap();
    // Sparse: 32 MiB + 1 byte without writing them.
    let huge = std::fs::File::create(root.join("huge.js")).unwrap();
    huge.set_len(32 * 1024 * 1024 + 1).unwrap();
    let (addr, _stop) = start(Some(&root), todo_cfg()).await;
    let r = get(&addr, "/big.js").await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("content-length"),
        Some(data.len().to_string().as_str())
    );
    assert!(r.body == data, "body differs");
    assert_eq!(get(&addr, "/huge.js").await.status, 404);
}
