# Integrating an app with a vault

How a browser (or native) app stores its persistent state in a vault instead of
on the device. Wire details: `docs/protocol/vault.md`. Container and CLI:
`docs/vault/README.md`.

The app uses `vault-client`. It never holds the vault's keys: it pairs with the
user's vault (a phone or a home server running `vault-server`) through a relay
mailbox, and reads and writes through a RAM cache that is dropped when the
session ends. Nothing is persisted locally.

## 1. Create the vault for your app

A vault serves one app at one web origin, fixed at creation:

```sh
vaultctl --dir ~/vault init --app my-app --origin https://my-app.example
```

`--app` matches `^[a-z0-9][a-z0-9.-]{0,63}$`. `--origin` is `https://host[:port]`
(`http://` only for `localhost`, `127.0.0.1`, `[::1]`), without path, query or
fragment; it is stored in canonical form (lower-case host, default port
dropped). The app passes the same values to `pair`; a mismatch ends the session
with `Closed(WrongApp)` before the user is asked anything.

Threat note: the origin is a declared, pinned label, not an authenticated
claim. The page supplies it and the vault cannot verify it, so a malicious page
can declare another app's origin. The pin keeps apps apart and catches
misconfiguration; protection against a malicious page rests on the user
checking the app, origin, relay host and code on the phone before approving.

## 2. Pair

```rust
use std::rc::Rc;
use vault_client::{SessionCallbacks, SessionState, SystemRng, VaultSession};
// browser: vault_client::wasm::{WsFactory, WebClock}; native: native::{TokioFactory, TokioClock}

let callbacks = SessionCallbacks::new(
    Rc::new(|qr: &str| show_qr(qr)),            // render as a QR code
    Rc::new(|sas: &str| show_code(sas)),         // 6-digit code, shown large
    Rc::new(|s: SessionState| render(s)),        // drive the UI from the state
)
.with_activate(Rc::new(|backend| Box::pin(async move {
    load_account(backend).await.map_err(|e| e.to_string()) // before Active
})))
.with_deactivate(Rc::new(|| Box::pin(async { stop_background_work().await })));

let (session, run) = VaultSession::pair(
    WsFactory, WebClock, &SystemRng,
    "https://relay.example",   // relay_origin
    "my-app",                  // app_id
    &location_origin,          // origin: location.origin
    "My Browser",              // device_name shown on the phone
    callbacks,
)?;
spawn_local(run); // the whole session runs in this future
```

`pair` returns the handle and a future that you must spawn and let run to
completion. `session.confirm_sas()` is the "Codes match" button,
`session.cancel_pairing()` is "Cancel", `session.lock()` ends an active
session, and `session.backend()` is the store (`Some` only while `Active`).
`pair_with` additionally takes `SessionOptions` (keepalive interval, at most
30 s).

Hooks run inside the session future and must not await `lock()`.
`on_activate` runs once both sides approved, before `Active` is reported: load
your account there; an `Err(text)` closes the session with that text.
`on_deactivate` runs after the session ended, before `Closed`: stop anything
that uses the store.

## 3. What the UI must show

| State | Show |
|---|---|
| `Pairing` | The QR code (from `on_offer`) and a short line: "Scan with your vault". Offer expires after 60 s. |
| `AwaitingApproval` | The code (from `on_sas`) large, "Compare it with the code on your phone", buttons **Codes match** (`confirm_sas`) and **Cancel** (`cancel_pairing`). |
| `Active` | The app. This is the only state in which it may read, write or act on vault data. |
| `Closed(reason)` | Say why (table below) and offer to pair again with a new session. |

Rules:

- Never act before `Active`: no store access, no account load outside
  `on_activate`, no "connected" indication.
- "Codes match" is the user's half of a two-sided check. Show it only with the
  code visible, and never click it for the user. The session goes `Active` only
  after both the vault's approval and this click.
- Cancel wins at any point before `Active`, even after "Codes match" or after
  the vault approved: the session closes as `Denied` and no data request is
  sent. After `Active`, `cancel_pairing()` returns an error; use `lock()`.

## 4. Commit before you acknowledge or transmit

Anything derived from state must be durable in the vault before it leaves the
app. Call `backend.flush_commit().await` and only then acknowledge a message
to its sender, send a reply, or tell the user something is saved.

Why: writes are buffered in RAM until `flush_commit`, and the session can end
at any time (phone locked, network lost). If you acknowledge first and the
session dies, the other side believes the data arrived and will not send it
again, and it is lost. If a commit fails the backend is dead and
`flush_commit` returns `ClientError::CommitFailed`: treat the writes as not
durable, do not acknowledge or transmit, and let the session close.

## 5. Handling `Closed(reason)`

`reason.to_string()` gives the text in the second column.

| Reason | Text | Meaning and suggested copy |
|---|---|---|
| `Denied` | `denied` | The user denied it on the phone, or cancelled the comparison. "Pairing was cancelled." |
| `WrongApp` | `wrong app` | The vault is bound to another app or origin. "This vault belongs to a different app or address." |
| `Locked` | `locked` | `lock()` was called, or the vault locked. "Vault locked." |
| `Terminated(r)` | `terminated: r` | The vault ended the session (for example the phone revoked it or timed out). |
| `Disconnected` | `disconnected` | The mailbox closed. "Connection to the vault was lost." |
| `Cancelled` | `cancelled` | `lock()` before the handshake finished. Nothing to report. |
| `CommitFailed` | `commit failed` | A commit failed; the vault discarded what it staged. "Your last change was not saved." |
| `SendFailed`, `ProtocolError`, `Closed` | `send failed`, `protocol error`, `closed` | Transport or protocol failure. Offer to pair again. |
| `RelayUnreachable(e)` | `relay unreachable: e` | Could not reach the relay. |
| `PairingFailed(e)` | `pairing failed: e` | Handshake or `OpenSession` failed (for example a stale QR). Pair again. |
| `KeepaliveFailed(e)`, `KeepaliveTimedOut` | `keepalive failed: e`, `keepalive timed out` | The vault stopped answering. |
| `Failed(text)` | the text | Your `on_activate` hook returned an error. |

Every `Closed` drops the cache and buffered writes. Unsaved changes are lost;
that is why section 4 matters.

## 6. Unlink and `Clear`

"Unlink" in an app is `backend.clear()` followed by `flush_commit()`. Because a
vault is bound to one app and origin, this erases that app's data in the vault
and nothing else. Replicas receive the empty state at the next replication, but
older replica snapshots keep the old data until the replica's own retention
expires them. The vault does not control that retention; tell users.

## 7. Depend on a pinned revision

`vault-client` is a git dependency; always pin an exact commit, never a branch
or tag:

```toml
[workspace.dependencies]
vault-client = { git = "https://github.com/5V715/vault", rev = "<full commit hash>", default-features = false, features = ["wasm"] }  # or "native"
vault-proto  = { git = "https://github.com/5V715/vault", rev = "<full commit hash>" }
```

Use the same `rev` for every vault crate you depend on and commit `Cargo.lock`
(build with `--locked`). Bump it deliberately: push or pick a commit whose CI
is green, change the `rev`, run your tests.

If the repository is private (or your fork is), CI needs a read-only deploy
key: create an ed25519 key, add the public half under the vault repository's
*Settings, Deploy keys* with "Allow write access" off, and store the private
half as a secret of the consuming repository. Load it into an `ssh-agent` only
for the fetch steps, map `https://github.com/5V715/vault` to
`ssh://git@github.com/5V715/vault` with `git config url.<ssh>.insteadOf`, set
`CARGO_NET_GIT_FETCH_WITH_CLI=true`, and keep the key out of the repository,
logs and any container image build (vendor the dependencies on the runner and
build the image offline).
