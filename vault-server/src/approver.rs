// SPDX-License-Identifier: MIT
//! User approval of a new browser session.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::oneshot;

/// What an approval prompt shows. All of it except `sas` and `mailbox_taken`
/// comes from the browser or the pairing text: display it as untrusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalRequest<'a> {
    /// Browser-supplied name (at most 64 bytes, no control characters, but
    /// possibly bidi or look-alike characters). Empty when `mailbox_taken`.
    pub device_name: &'a str,
    /// The 6-digit code the browser also shows. Empty when `mailbox_taken`.
    pub sas: &'a str,
    /// The app id of the offer (equal to the vault's binding: a mismatch never
    /// reaches the approver).
    pub app_id: &'a str,
    /// The web origin the page declared (equal to the vault's binding). Not
    /// authenticated: the user should still check that it is the page they opened.
    pub origin: &'a str,
    /// Host (and port) of the relay named in the pairing text.
    pub relay_host: &'a str,
    /// The relay refused the vault because two parties already hold the
    /// mailbox (HTTP 409): someone else may have read the pairing text and may
    /// be posing as this vault. There is no session and no code. Show
    /// [`MAILBOX_TAKEN_WARNING`] (or its equivalent) prominently; the server
    /// treats any answer as [`ApprovalDecision::Deny`], so return `Deny`
    /// without unlocking anything.
    pub mailbox_taken: bool,
}

/// What the user should be told when the mailbox was already taken.
pub const MAILBOX_TAKEN_WARNING: &str = "\
!!! WARNING !!!
This pairing code was already used by another device: the relay mailbox is taken.
Someone else may have read the pairing text and may be posing as your vault.
Do NOT press \"Codes match\" in the browser. Press Cancel, close the tab and
start over with a new code (and do not share the pairing text).";

/// What opens the container when an approval arrives while the vault is locked.
///
/// Produce it only at approval time, after the user's explicit confirmation
/// (on Android: the BiometricPrompt that unwraps the CRK). The server opens the
/// container with it before it sends `Approved`, then drops it.
pub enum UnlockMaterial {
    /// Nothing to open: for hosts that keep the vault unlocked (a container
    /// given to [`crate::VaultServer::new`] or [`crate::VaultServer::unlock`]).
    /// On a locked vault this approval fails ([`crate::EndReason::UnlockFailed`]).
    Unlocked,
    /// The container root key, e.g. unwrapped from the device keystore.
    Crk {
        crk: vault_container::Crk,
        /// The anti-rollback mark the host persisted last.
        high_water: Option<vault_container::engine::HighWater>,
    },
    /// The recovery key (development hosts such as `vault-dev`).
    RecoveryKey {
        key: vault_container::RecoveryKey,
        high_water: Option<vault_container::engine::HighWater>,
    },
}

impl std::fmt::Debug for UnlockMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unlocked => "Unlocked",
            Self::Crk { .. } => "Crk(<redacted>)",
            Self::RecoveryKey { .. } => "RecoveryKey(<redacted>)",
        })
    }
}

/// The user's answer to an [`ApprovalRequest`].
#[derive(Debug)]
pub enum ApprovalDecision {
    Deny,
    /// Approve, with what unlocks the vault if it is locked.
    Approve(UnlockMaterial),
}

impl ApprovalDecision {
    pub fn is_approve(&self) -> bool {
        matches!(self, Self::Approve(_))
    }
}

/// `true` is an approval for a vault that is already unlocked
/// ([`UnlockMaterial::Unlocked`]); `false` is a denial.
impl From<bool> for ApprovalDecision {
    fn from(yes: bool) -> Self {
        if yes {
            Self::Approve(UnlockMaterial::Unlocked)
        } else {
            Self::Deny
        }
    }
}

/// Asks the user whether a browser may open a session.
///
/// Implementations show the [`ApprovalRequest`]: the app id, origin, relay host,
/// device name and the 6-digit SAS, which the user must compare with the code the
/// browser shows, and return [`ApprovalDecision::Approve`] only on an explicit
/// confirmation (biometric prompt on Android, a terminal prompt in development),
/// with the [`UnlockMaterial`] obtained at that moment.
/// The server calls `approve` for one session at a time and cancels the call
/// (drops the future) after its approval timeout or when the session ends;
/// implementations must treat that as a denial and must not let an answer given
/// for a cancelled prompt apply to a later one.
///
/// The server checks the offer's app id and origin against the vault's binding
/// (read from the container header, which needs no key) before it calls
/// `approve`; a mismatch ends the session with `Notice::WrongApp` and never
/// prompts. A request with `mailbox_taken` set is a warning only: its answer is
/// always treated as a denial.
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, req: &ApprovalRequest<'_>) -> ApprovalDecision;
}

/// The answer the server acts on: never an approval for a taken mailbox.
pub(crate) fn effective(req: &ApprovalRequest<'_>, d: ApprovalDecision) -> ApprovalDecision {
    if req.mailbox_taken {
        // Dropping the decision drops (and zeroizes) any unlock material unused.
        ApprovalDecision::Deny
    } else {
        d
    }
}

/// Development approver: prompts on a terminal and reads answers from one
/// long-lived reader thread.
///
/// Every prompt carries a number; approving requires typing `y <number>`, so a
/// late answer to an earlier (timed-out) prompt cannot approve a later one.
/// `n` denies the current prompt. Lines typed while no prompt is open are
/// dropped. At end of input every prompt is denied.
///
/// An approval carries [`UnlockMaterial::Unlocked`] unless
/// [`TerminalApprover::with_unlock`] supplies material, which is then produced
/// at the moment of each approval. A mailbox-taken request prints
/// [`MAILBOX_TAKEN_WARNING`] and is denied without a prompt.
pub struct TerminalApprover {
    shared: Arc<Shared>,
    unlock: Option<UnlockFn>,
}

type UnlockFn = Box<dyn Fn() -> UnlockMaterial + Send + Sync>;

struct Shared {
    next: AtomicU64,
    closed: AtomicBool,
    current: Mutex<Option<(u64, oneshot::Sender<bool>)>>,
    out: Mutex<Box<dyn Write + Send>>,
}

impl Shared {
    fn current(&self) -> MutexGuard<'_, Option<(u64, oneshot::Sender<bool>)>> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn say(&self, s: &str) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }

    fn deliver(&self, line: &str) {
        let mut cur = self.current();
        let Some((id, _)) = cur.as_ref() else {
            return; // stale: no prompt open
        };
        let id = *id;
        let answer = match parse(line) {
            Some((true, Some(n))) if n == id => Some(true),
            Some((false, None)) => Some(false),
            Some((false, Some(n))) if n == id => Some(false),
            _ => None,
        };
        match answer {
            Some(a) => {
                if let Some((_, tx)) = cur.take() {
                    let _ = tx.send(a);
                }
            }
            None => {
                drop(cur);
                self.say(&format!("Type 'y {id}' to allow or 'n' to deny.\n"));
            }
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some((_, tx)) = self.current().take() {
            let _ = tx.send(false);
        }
    }
}

/// `y <n>` / `yes <n>` / `n` / `no` (optionally followed by the prompt number).
fn parse(line: &str) -> Option<(bool, Option<u64>)> {
    let mut it = line.split_whitespace();
    let yes = match it.next()?.to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => return None,
    };
    let n = match it.next() {
        Some(t) => Some(t.trim_start_matches('#').parse().ok()?),
        None => None,
    };
    if it.next().is_some() {
        return None;
    }
    Some((yes, n))
}

/// Clears the open prompt if it is still ours (cancelled or answered).
struct ClearOnDrop<'a>(&'a Shared, u64);

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        let mut cur = self.0.current();
        if cur.as_ref().is_some_and(|(id, _)| *id == self.1) {
            *cur = None;
        }
    }
}

impl Default for TerminalApprover {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalApprover {
    /// Prompts on stderr, reads stdin.
    pub fn new() -> Self {
        Self::with_io(BufReader::new(std::io::stdin()), std::io::stderr())
    }

    /// Prompts on `output`, reads answers from `input` on a dedicated thread.
    pub fn with_io(
        input: impl BufRead + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            next: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            current: Mutex::new(None),
            out: Mutex::new(Box::new(output)),
        });
        let reader = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("vault-approver-input".into())
            .spawn(move || {
                for line in input.lines() {
                    match line {
                        Ok(l) => reader.deliver(&l),
                        Err(_) => break,
                    }
                }
                reader.close();
            });
        if spawned.is_err() {
            shared.close();
        }
        Self {
            shared,
            unlock: None,
        }
    }

    /// Approvals carry the material `unlock` returns, called once per
    /// approval, after the user's answer.
    pub fn with_unlock(
        mut self,
        unlock: impl Fn() -> UnlockMaterial + Send + Sync + 'static,
    ) -> Self {
        self.unlock = Some(Box::new(unlock));
        self
    }
}

#[async_trait::async_trait]
impl Approver for TerminalApprover {
    async fn approve(&self, req: &ApprovalRequest<'_>) -> ApprovalDecision {
        let s = &*self.shared;
        if req.mailbox_taken {
            s.say(&format!("\n{MAILBOX_TAKEN_WARNING}\n"));
            return ApprovalDecision::Deny;
        }
        if !self.ask(req).await {
            return ApprovalDecision::Deny;
        }
        ApprovalDecision::Approve(match &self.unlock {
            Some(f) => f(),
            None => UnlockMaterial::Unlocked,
        })
    }
}

impl TerminalApprover {
    async fn ask(&self, req: &ApprovalRequest<'_>) -> bool {
        let s = &*self.shared;
        let id = s.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        {
            let mut cur = s.current();
            if s.closed.load(Ordering::SeqCst) {
                return false;
            }
            // Approvals are serialized by the server; a leftover prompt is denied.
            if let Some((_, old)) = cur.replace((id, tx)) {
                let _ = old.send(false);
            }
        }
        let _clear = ClearOnDrop(s, id);
        s.say(&format!(
            "\n[#{id}] Allow \"{device}\" to open a vault session?\n\
             [#{id}] App: {app}\n\
             [#{id}] Origin: {origin}  (must be the web app page you opened)\n\
             [#{id}] Relay: {relay}  (must be the relay of that page)\n\
             [#{id}] Code: {sas}  (must match the browser)\n\
             [#{id}] Type 'y {id}' to allow, 'n' to deny: ",
            device = req.device_name.escape_debug(),
            app = req.app_id.escape_debug(),
            origin = req.origin.escape_debug(),
            relay = req.relay_host.escape_debug(),
            sas = req.sas.escape_debug(),
        ));
        rx.await.unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
        async fn wait_for(&self, s: &str) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !self.text().contains(s) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("prompt not shown");
        }
    }

    fn setup() -> (Arc<TerminalApprover>, std::io::PipeWriter, Buf) {
        let (r, w) = std::io::pipe().unwrap();
        let out = Buf::default();
        let a = TerminalApprover::with_io(BufReader::new(r), out.clone());
        (Arc::new(a), w, out)
    }

    fn req<'a>(device_name: &'a str, sas: &'a str) -> ApprovalRequest<'a> {
        ApprovalRequest {
            device_name,
            sas,
            app_id: "test-app",
            origin: "https://app.example",
            relay_host: "relay.example",
            mailbox_taken: false,
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[test]
    fn parses_answers() {
        assert_eq!(parse("y 3"), Some((true, Some(3))));
        assert_eq!(parse(" YES #3 "), Some((true, Some(3))));
        assert_eq!(parse("y"), Some((true, None)));
        assert_eq!(parse("n"), Some((false, None)));
        assert_eq!(parse("no 2"), Some((false, Some(2))));
        assert_eq!(parse("y x"), None);
        assert_eq!(parse("y 1 2"), None);
        assert_eq!(parse("maybe"), None);
        assert_eq!(parse(""), None);
    }

    #[tokio::test]
    async fn stale_and_mismatched_lines_are_ignored() {
        let (a, mut w, out) = setup();
        writeln!(w, "y 1").unwrap(); // before any prompt: dropped
        settle().await;
        let t = tokio::spawn({
            let a = a.clone();
            async move {
                a.approve(&req("Fire\u{202e}fox", "123456"))
                    .await
                    .is_approve()
            }
        });
        out.wait_for("Type 'y 1'").await;
        assert!(out.text().contains("123456"));
        assert!(out.text().contains("\\u{202e}"), "{}", out.text());
        writeln!(w, "y").unwrap(); // no number: not an approval
        writeln!(w, "y 7").unwrap(); // other prompt
        settle().await;
        assert!(!t.is_finished());
        writeln!(w, "y 1").unwrap();
        assert!(t.await.unwrap());
    }

    #[tokio::test]
    async fn late_answer_does_not_approve_next_prompt() {
        let (a, mut w, out) = setup();
        let r =
            tokio::time::timeout(Duration::from_millis(50), a.approve(&req("A", "111111"))).await;
        assert!(r.is_err()); // cancelled, like a server approval timeout
        let t = tokio::spawn({
            let a = a.clone();
            async move { a.approve(&req("B", "222222")).await.is_approve() }
        });
        out.wait_for("Type 'y 2'").await;
        writeln!(w, "y 1").unwrap(); // late answer to the cancelled prompt
        settle().await;
        assert!(!t.is_finished());
        writeln!(w, "n").unwrap();
        assert!(!t.await.unwrap());
    }

    #[tokio::test]
    async fn prompt_shows_relay_host() {
        let (a, mut w, out) = setup();
        let t = tokio::spawn({
            let a = a.clone();
            async move {
                a.approve(&ApprovalRequest {
                    device_name: "Firefox",
                    sas: "123456",
                    app_id: "org.example.notes",
                    origin: "https://notes.example:8443",
                    relay_host: "relay.example\u{202e}",
                    mailbox_taken: false,
                })
                .await
                .is_approve()
            }
        });
        out.wait_for("Type 'y 1'").await;
        let text = out.text();
        for want in [
            "Allow \"Firefox\"",
            "App: org.example.notes",
            "Origin: https://notes.example:8443",
            "Relay: relay.example\\u{202e}",
            "Code: 123456",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
        writeln!(w, "n").unwrap();
        assert!(!t.await.unwrap());
    }

    #[tokio::test]
    async fn end_of_input_denies() {
        let (a, w, out) = setup();
        let t = tokio::spawn({
            let a = a.clone();
            async move { a.approve(&req("A", "111111")).await.is_approve() }
        });
        out.wait_for("Type 'y 1'").await;
        drop(w);
        assert!(!t.await.unwrap());
        assert!(!a.approve(&req("B", "222222")).await.is_approve());
    }

    #[tokio::test]
    async fn mailbox_taken_warns_and_denies_without_prompt() {
        let (a, _w, out) = setup();
        let a = Arc::try_unwrap(a).ok().unwrap().with_unlock(|| {
            panic!("no unlock material for a taken mailbox");
        });
        let d = a
            .approve(&ApprovalRequest {
                mailbox_taken: true,
                ..req("", "")
            })
            .await;
        assert!(!d.is_approve());
        let text = out.text();
        assert!(text.contains("already used by another device"), "{text}");
        assert!(!text.contains("Type 'y"), "{text}");
    }

    #[tokio::test]
    async fn approval_carries_unlock_material() {
        let (a, mut w, out) = setup();
        let a = Arc::new(
            Arc::try_unwrap(a)
                .ok()
                .unwrap()
                .with_unlock(|| UnlockMaterial::Crk {
                    crk: vault_container::Crk::generate(),
                    high_water: None,
                }),
        );
        let t = tokio::spawn({
            let a = a.clone();
            async move { a.approve(&req("A", "111111")).await }
        });
        out.wait_for("Type 'y 1'").await;
        writeln!(w, "y 1").unwrap();
        assert!(matches!(
            t.await.unwrap(),
            ApprovalDecision::Approve(UnlockMaterial::Crk { .. })
        ));
    }
}
