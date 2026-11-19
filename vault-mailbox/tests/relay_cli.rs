// SPDX-License-Identifier: MIT
//! The `vault-relay` binary: it must not start without a browser origin
//! allowlist, and each entry follows the app-binding origin rules.

use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

const BIN: &str = env!("CARGO_BIN_EXE_vault-relay");
const ENV: &str = "VAULT_RELAY_ALLOW_ORIGIN";

fn relay(args: &[&str], env: Option<&str>) -> Command {
    let mut c = Command::new(BIN);
    c.args(["--listen", "127.0.0.1:0"]).args(args);
    c.env_remove(ENV)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info");
    if let Some(v) = env {
        c.env(ENV, v);
    }
    c
}

/// Runs a relay that is expected to refuse to start (killed after 5 s otherwise).
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

#[test]
fn vault_relay_requires_allow_origin() {
    for env in [None, Some(""), Some(" , ")] {
        let out = refused(relay(&[], env));
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{env:?}: {err}");
        assert!(
            err.contains("no browser origin is allowed")
                && err.contains("--allow-origin")
                && err.contains(ENV),
            "{env:?}: {err}"
        );
    }
}

#[test]
fn vault_relay_rejects_invalid_allow_origin() {
    for bad in [
        "app.example",
        "http://app.example",
        "https://app.example/",
        "https://app.example/path",
        "https://app.example?q",
        "https://user@app.example",
        "wss://app.example",
        "*",
    ] {
        for (args, env) in [
            (
                vec![
                    "--allow-origin",
                    "https://ok.example",
                    "--allow-origin",
                    bad,
                ],
                None,
            ),
            (vec![], Some(format!("https://ok.example,{bad}"))),
        ] {
            let out = refused(relay(&args, env.as_deref()));
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(2), "{bad}: {err}");
            assert!(
                err.contains("invalid allowed origin") && err.contains(&format!("{bad:?}")),
                "{bad}: {err}"
            );
        }
    }
}

/// Spawns a relay that should start; returns it and its listen address.
fn started(mut c: Command) -> (std::process::Child, String) {
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

async fn status(addr: &str, origin: Option<&str>) -> u16 {
    let mut req = format!("ws://{addr}/mailbox/0123456789abcdef0123456789abcdef")
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
async fn vault_relay_allow_origin_is_normalized() {
    for (args, env) in [
        (vec!["--allow-origin", "HTTPS://App.Example:443"], None),
        (
            vec![],
            Some(" HTTPS://App.Example:443 , https://other.example"),
        ),
    ] {
        let (mut child, addr) = started(relay(&args, env));
        assert_eq!(status(&addr, Some("https://app.example")).await, 101);
        assert_eq!(status(&addr, Some("https://evil.example")).await, 403);
        assert_eq!(status(&addr, None).await, 101, "native client");
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
