// SPDX-License-Identifier: MIT
//! Runs the `vault-dev` binary against an in-process relay with a `vault-proto`
//! browser-side client and answers its approval prompt through piped stdin.
#![cfg(feature = "dev")]

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use vault_container::backend::LocalBackend;
use vault_container::engine::Container;
use vault_container::replicate::ReplicaState;
use vault_container::secrets::{Secrets, load_secrets, save_secrets};
use vault_proto::msg::VaultMessage;
use vault_proto::noise::{PairKeys, Session};
use vault_proto::{Chunk, Chunker, Reassembler, Request, RequestBody, ResponseBody};

const T: Duration = Duration::from_secs(20);
const PASS: &str = "correct horse";
const BIN: &str = env!("CARGO_BIN_EXE_vault-dev");
/// The binding of the test vault directories.
const APP: &str = "test-app";
const ORIGIN: &str = "https://app.example";

struct Relay {
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

async fn relay() -> Relay {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let mailboxes = vault_mailbox::Mailboxes::new(vault_mailbox::MailboxConfig::default());
    tokio::spawn(vault_mailbox::serve(l, mailboxes, async move {
        let _ = rx.await;
    }));
    Relay { addr, _stop: tx }
}

/// A container directory as `vaultctl init --app test-app --origin https://app.example` leaves it.
async fn make_dir(pass: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let backend = LocalBackend::open(dir.path()).await.unwrap();
    let (c, rk) = Container::create(backend, APP, ORIGIN).await.unwrap();
    let secrets = Secrets {
        recovery_key: (*rk.to_display()).clone(),
        high_water: Some(c.high_water()),
        state: ReplicaState::default(),
        replicas: Vec::new(),
        last_observed: Default::default(),
    };
    save_secrets(dir.path(), &secrets, pass).unwrap();
    dir
}

/// Reads the persisted value of `key` back from the container directory.
async fn read_back(dir: &Path, key: &[u8]) -> Option<Vec<u8>> {
    let s = load_secrets(dir, PASS).unwrap();
    let rk = vault_container::RecoveryKey::parse(&s.recovery_key).unwrap();
    let backend = LocalBackend::open(dir).await.unwrap();
    let (c, _) = Container::open_with_recovery(backend, &rk, None)
        .await
        .unwrap();
    c.get(key)
}

struct Vault {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    err: Arc<Mutex<String>>,
    out: Arc<Mutex<String>>,
}

fn pump(mut r: impl std::io::Read + Send + 'static) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let b = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 512];
        // Prompts end without a newline, so collect raw bytes rather than lines.
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            b.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    buf
}

fn spawn_vault(dir: &Path, pass: &str, offer: &str, passfile: &Path) -> Vault {
    std::fs::write(passfile, format!("{pass}\n")).unwrap();
    let mut child = Command::new(BIN)
        .args(["serve", "--dir"])
        .arg(dir)
        .args(["--pair", offer, "--passphrase-file"])
        .arg(passfile)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    Vault {
        stdin: child.stdin.take(),
        err: pump(child.stderr.take().unwrap()),
        out: pump(child.stdout.take().unwrap()),
        child,
    }
}

impl Vault {
    async fn wait_err(&self, needle: &str) {
        tokio::time::timeout(T, async {
            while !self.err.lock().unwrap().contains(needle) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never saw {needle:?}; stderr: {}", self.err.lock().unwrap()));
    }
    fn say(&mut self, line: &str) {
        let s = self.stdin.as_mut().unwrap();
        s.write_all(line.as_bytes()).unwrap();
        s.flush().unwrap();
    }
    async fn exit(&mut self) -> std::process::ExitStatus {
        tokio::time::timeout(T, async {
            loop {
                if let Some(st) = self.child.try_wait().unwrap() {
                    return st;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("vault-dev did not exit")
    }
    async fn settled_out(&self) -> String {
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.out.lock().unwrap().clone()
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

struct Browser {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    noise: Session,
    chunker: Chunker,
    reasm: Reassembler,
    next_id: u64,
}

impl Browser {
    async fn send(&mut self, body: RequestBody) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        for c in self.chunker.split(&Request { id, body }.encode()).unwrap() {
            let f = self.noise.encrypt(&c.encode()).unwrap();
            self.ws.send(Message::Binary(f.into())).await.unwrap();
        }
        id
    }
    async fn recv(&mut self) -> Option<VaultMessage> {
        tokio::time::timeout(T, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Binary(b))) => {
                        let pt = self.noise.decrypt(&b).unwrap();
                        if let Some(m) = self.reasm.push(Chunk::decode(&pt).unwrap()).unwrap() {
                            return Some(VaultMessage::decode(&m).unwrap());
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .expect("no answer from vault-dev")
    }
    async fn call(&mut self, body: RequestBody) -> ResponseBody {
        let id = self.send(body).await;
        match self.recv().await {
            Some(VaultMessage::Response(r)) => {
                assert_eq!(r.id, id);
                r.body
            }
            other => panic!("expected response, got {other:?}"),
        }
    }
}

/// Browser side: offer on the relay, the vault dials it, handshake done; returns the
/// offer text for the vault's command line and a future-ish continuation.
struct Offer {
    keys: PairKeys,
    text: String,
}

fn offer(relay: &Relay, tag: u8) -> Offer {
    offer_for(relay, tag, APP, ORIGIN)
}

fn offer_for(relay: &Relay, tag: u8, app: &str, origin: &str) -> Offer {
    let keys = PairKeys::generate(
        format!("http://{}", relay.addr),
        [tag; 16],
        app.into(),
        origin.into(),
    )
    .unwrap();
    let text = keys.offer().to_qr().unwrap();
    Offer { keys, text }
}

async fn handshake(relay: &Relay, o: Offer) -> Browser {
    let id: String = o
        .keys
        .offer()
        .mailbox_id
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/mailbox/{id}", relay.addr))
        .await
        .unwrap();
    let (init, m1) = o.keys.into_initiator().unwrap();
    ws.send(Message::Binary(m1.into())).await.unwrap();
    let m2 = match tokio::time::timeout(T, ws.next()).await.unwrap() {
        Some(Ok(Message::Binary(b))) => b.to_vec(),
        other => panic!("expected msg2, got {other:?}"),
    };
    let (m3, noise) = init.handle_msg2(&m2).unwrap();
    ws.send(Message::Binary(m3.into())).await.unwrap();
    Browser {
        ws,
        noise,
        chunker: Chunker::new(),
        reasm: Reassembler::new(),
        next_id: 1,
    }
}

fn prompt_number(stderr: &str) -> String {
    let i = stderr.find("Type 'y ").expect("no prompt") + "Type 'y ".len();
    stderr[i..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_persists_put_and_commit() {
    let relay = relay().await;
    let dir = make_dir(PASS).await;
    let tmp = tempfile::tempdir().unwrap();
    let o = offer(&relay, 1);
    let mut v = spawn_vault(dir.path(), PASS, &o.text, &tmp.path().join("pw"));
    let mut b = handshake(&relay, o).await;
    let sas = b.noise.sas().to_string();
    let open = b
        .send(RequestBody::OpenSession {
            device_name: "Test Laptop".into(),
        })
        .await;
    v.wait_err("Type 'y ").await;
    let err = v.err.lock().unwrap().clone();
    assert!(err.contains("Test Laptop"), "{err}");
    assert!(err.contains(&sas), "SAS shown: {err}");
    assert!(
        err.contains(&format!("Relay: {}", relay.addr)),
        "relay host shown: {err}"
    );
    assert!(err.contains(&format!("App: {APP}")), "app shown: {err}");
    assert!(
        err.contains(&format!("Origin: {ORIGIN}")),
        "origin shown: {err}"
    );
    assert!(!err.contains(PASS));
    let n = prompt_number(&err);
    v.say(&format!("y {n}\n"));
    match b.recv().await {
        Some(VaultMessage::Response(r)) => {
            assert_eq!(r.id, open);
            assert_eq!(r.body, ResponseBody::Approved);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        b.call(RequestBody::Put {
            key: b"k".to_vec(),
            value: b"v".to_vec()
        })
        .await,
        ResponseBody::Ok
    );
    assert!(matches!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { .. }
    ));
    let _ = b.send(RequestBody::Close).await;
    let st = v.exit().await;
    assert!(st.success(), "{st:?}; stderr {}", v.err.lock().unwrap());
    assert!(v.settled_out().await.contains("session ended: closed"));
    assert_eq!(
        read_back(dir.path(), b"k").await.as_deref(),
        Some(&b"v"[..])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_and_eof_end_session_without_persisting() {
    for answer in [Some("n\n"), None] {
        let relay = relay().await;
        let dir = make_dir(PASS).await;
        let tmp = tempfile::tempdir().unwrap();
        let o = offer(&relay, 2);
        let mut v = spawn_vault(dir.path(), PASS, &o.text, &tmp.path().join("pw"));
        let mut b = handshake(&relay, o).await;
        b.send(RequestBody::OpenSession {
            device_name: "Laptop".into(),
        })
        .await;
        v.wait_err("Type 'y ").await;
        match answer {
            Some(a) => v.say(a),
            None => drop(v.stdin.take()),
        }
        match b.recv().await {
            Some(VaultMessage::Response(r)) => assert_eq!(r.body, ResponseBody::Denied),
            other => panic!("{other:?}"),
        }
        let st = v.exit().await;
        assert!(st.success(), "{st:?}");
        assert!(v.settled_out().await.contains("session ended: denied"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passphrase_exits_nonzero_without_dialing() {
    let relay = relay().await;
    let dir = make_dir(PASS).await;
    let tmp = tempfile::tempdir().unwrap();
    let o = offer(&relay, 3);
    let mut v = spawn_vault(dir.path(), "wrong", &o.text, &tmp.path().join("pw"));
    let st = v.exit().await;
    assert!(!st.success());
    v.wait_err("wrong passphrase").await;
    assert!(!v.err.lock().unwrap().contains("wrong\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_offer_and_unreachable_relay_exit_nonzero() {
    let dir = make_dir(PASS).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut v = spawn_vault(dir.path(), PASS, "not-an-offer", &tmp.path().join("pw"));
    assert!(!v.exit().await.success());
    v.wait_err("pair").await;

    // Valid offer, nothing listening.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let keys =
        PairKeys::generate(format!("http://{addr}"), [4; 16], APP.into(), ORIGIN.into()).unwrap();
    let text = keys.offer().to_qr().unwrap();
    let mut v = spawn_vault(dir.path(), PASS, &text, &tmp.path().join("pw"));
    assert!(!v.exit().await.success());
    v.wait_err("relay").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mailbox_taken_warns_not_to_confirm_the_browser() {
    let relay = relay().await;
    let dir = make_dir(PASS).await;
    let tmp = tempfile::tempdir().unwrap();
    let o = offer(&relay, 5);
    let id: String = o
        .keys
        .offer()
        .mailbox_id
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let url = format!("ws://{}/mailbox/{id}", relay.addr);
    // The browser and someone who read the pairing text hold both places.
    let (_browser, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_impostor, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let mut v = spawn_vault(dir.path(), PASS, &o.text, &tmp.path().join("pw"));
    assert!(!v.exit().await.success());
    v.wait_err("WARNING").await;
    let err = v.err.lock().unwrap().clone();
    assert!(err.contains("already used by another device"), "{err}");
    assert!(err.contains("Do NOT press \"Codes match\""), "{err}");
}

async fn approved_session(relay: &Relay, dir: &Path, tmp: &Path, tag: u8) -> (Vault, Browser) {
    let o = offer(relay, tag);
    let mut v = spawn_vault(dir, PASS, &o.text, &tmp.join("pw"));
    let mut b = handshake(relay, o).await;
    b.send(RequestBody::OpenSession {
        device_name: "Laptop".into(),
    })
    .await;
    v.wait_err("Type 'y ").await;
    let n = prompt_number(&v.err.lock().unwrap().clone());
    v.say(&format!("y {n}\n"));
    match b.recv().await {
        Some(VaultMessage::Response(r)) => assert_eq!(r.body, ResponseBody::Approved),
        other => panic!("{other:?}"),
    }
    (v, b)
}

fn segment_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("seg")).map_or(0, |d| d.count())
}

#[tokio::test(flavor = "multi_thread")]
async fn many_commits_are_compacted_at_exit_and_high_water_saved() {
    let relay = relay().await;
    let dir = make_dir(PASS).await;
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, mut b) = approved_session(&relay, dir.path(), tmp.path(), 6).await;
    let n = vault_server::COMPACT_MAX_SEGMENTS as u32;
    for i in 0..n {
        assert_eq!(
            b.call(RequestBody::Put {
                key: format!("k{i:03}").into_bytes(),
                value: vec![1; 10]
            })
            .await,
            ResponseBody::Ok
        );
        assert!(matches!(
            b.call(RequestBody::Commit).await,
            ResponseBody::Committed { .. }
        ));
    }
    assert_eq!(segment_files(dir.path()), n as usize);
    let _ = b.send(RequestBody::Close).await;
    assert!(v.exit().await.success());
    assert_eq!(segment_files(dir.path()), 1, "compacted");
    let hw = load_secrets(dir.path(), PASS).unwrap().high_water.unwrap();
    assert_eq!(hw.seq, n as u64 + 1, "high-water of the compacted manifest");
    assert_eq!(
        read_back(dir.path(), format!("k{:03}", n - 1).as_bytes())
            .await
            .as_deref(),
        Some(&[1u8; 10][..])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_saves_high_water() {
    let relay = relay().await;
    let dir = make_dir(PASS).await;
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, mut b) = approved_session(&relay, dir.path(), tmp.path(), 7).await;
    assert_eq!(
        b.call(RequestBody::Put {
            key: b"k".to_vec(),
            value: b"v".to_vec()
        })
        .await,
        ResponseBody::Ok
    );
    assert!(matches!(
        b.call(RequestBody::Commit).await,
        ResponseBody::Committed { seq: 1 }
    ));
    let st = Command::new("kill")
        .args(["-INT", &v.child.id().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    assert!(v.exit().await.success());
    assert!(v.settled_out().await.contains("session ended: interrupted"));
    let hw = load_secrets(dir.path(), PASS).unwrap().high_water.unwrap();
    assert_eq!(hw.seq, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_app_pairing_text_is_refused_without_a_prompt() {
    for (app, origin) in [("other-app", ORIGIN), (APP, "https://evil.example")] {
        let relay = relay().await;
        let dir = make_dir(PASS).await;
        let tmp = tempfile::tempdir().unwrap();
        let o = offer_for(&relay, 6, app, origin);
        let mut v = spawn_vault(dir.path(), PASS, &o.text, &tmp.path().join("pw"));
        let mut b = handshake(&relay, o).await;
        // The browser is told before it even asks.
        assert_eq!(
            b.recv().await,
            Some(VaultMessage::Notice(vault_proto::Notice::WrongApp))
        );
        let st = v.exit().await;
        assert!(st.success(), "{st:?}; stderr {}", v.err.lock().unwrap());
        assert!(v.settled_out().await.contains("session ended: wrong app"));
        let err = v.err.lock().unwrap().clone();
        assert!(
            err.contains("WARNING: this vault is bound to app test-app at https://app.example"),
            "{err}"
        );
        assert!(!err.contains("Type 'y "), "no prompt: {err}");
    }
}
