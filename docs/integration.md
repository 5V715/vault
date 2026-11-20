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

## 2. Running the relay

Pairing needs a relay: a WebSocket rendezvous at `<relay_origin>/mailbox/<32
hex>` that pairs the browser with the vault and forwards their frames. The
relay is untrusted. Everything it carries is Noise ciphertext between the
browser and the vault (end to end, with the SAS check on both sides); it cannot
read or change data or impersonate either side. What it controls is
availability, and that is what its limits and origin allowlist protect.

You can provide it in one of two ways.

**A. Run `vault-relay`** (the `vault-mailbox` crate's binary, or the image from
`Dockerfile.relay`) behind a TLS-terminating reverse proxy:

```sh
vault-relay --listen 127.0.0.1:8080 --allow-origin https://my-app.example --trust-proxy
# or, with the image:
docker run -p 127.0.0.1:8080:8080 -e VAULT_RELAY_ALLOW_ORIGIN=https://my-app.example IMAGE --trust-proxy
```

- `--allow-origin` (repeatable; or `VAULT_RELAY_ALLOW_ORIGIN`, comma-separated)
  is required: list the page origin of every app that pairs through this relay,
  that is the `origin` you pass to `pair`. The relay refuses to start without
  one (exit status 2) and rejects values that break the origin rules of section
  1 (`https`, `http` only for loopback, no path, query or fragment); values are
  stored in canonical form. A browser whose `Origin` is not listed gets HTTP
  403. Clients that send no `Origin` (native vaults, `vault-dev`) are always
  admitted. There is no "allow any origin" mode: an open relay lets any website
  hold mailboxes and use up the waiting-mailbox cap, which blocks pairing for
  your users.
- TLS is required: browsers and vaults use `wss://` (the `relay_origin` is
  `https://...`, which maps to `wss://`). Plain `ws://` is accepted only for
  loopback hosts, for local development. By default `vault-relay` speaks plain
  HTTP, so terminate TLS in front of it, or give it a certificate (next item).
  A `relay_origin` may include a base path
  (`https://example.com/relay` becomes `wss://example.com/relay/mailbox/<id>`)
  if your proxy strips it before forwarding.
- `--static-dir DIR` (off by default) also serves a page's files read-only on the
  same port, with the page's CSP and a `/config.json` naming the relay; it needs
  `--public-origin` (`PUBLIC_ORIGIN`). The `vault-todo-demo` image uses it: see
  [`deploy-todo-demo.md`](deploy-todo-demo.md).
- `--tls-cert CERT.pem --tls-key KEY.pem` (or `VAULT_RELAY_TLS_CERT` and
  `VAULT_RELAY_TLS_KEY`, for containers) make the relay terminate TLS itself,
  for a LAN or a single host without a reverse proxy. The certificate file is a
  PEM chain (leaf first, intermediates after), the key file PEM (PKCS#8, PKCS#1
  or SEC1). Both are required together; a missing, mismatched or unparsable pair,
  or a variable that is set but empty, stops the relay with exit status 2 and a message that never shows key
  material. It uses rustls with TLS 1.2 or newer (1.3 preferred), no client
  certificates, and the same connection cap and stall timeout cover the
  handshake. On Unix it warns when the key file is readable by group or others
  (use mode 0600). Certificates are read once at start; restart to renew. A
  private CA is trusted by `vault-dev` with `--ca-file` (see
  `docs/vault/README.md`) and has to be installed on the phone or browser.
- `--trust-proxy` makes the relay take the client address from the first
  `X-Forwarded-For` hop. Behind a proxy you need it, or every user shares the
  proxy's address and its per-address limit (`--max-per-ip`, default 8 open
  mailbox connections). Set it only when the proxy overwrites that header (for
  example nginx `proxy_set_header X-Forwarded-For $remote_addr;`); otherwise a
  client can pick its own address and evade the limit. Without a proxy, leave it
  off.
- The proxy must pass the WebSocket upgrade (`Upgrade`/`Connection` headers)
  and must keep idle WebSockets open for longer than the 30 s keepalive
  interval (nginx's default `proxy_read_timeout` of 60 s is enough; the relay's
  own `--idle-timeout-s` defaults to 120 s).
- Connection limits (defaults): `--max-per-ip 8` and `--max-total 64` open
  mailbox connections (each pairing uses two: browser and vault), `--max-waiting
  32` unpaired mailboxes, `--max-connections 256` HTTP connections including
  not-yet-upgraded ones, unpaired mailboxes close after `--pair-timeout-s 60`,
  and `--stall-timeout-s 60` cuts connections that make no progress. Size them
  for your number of concurrent users. `GET /healthz` answers `ok` for health
  checks.

**B. Mount the mailbox in your own server**, as signal-web does. Build
`vault-mailbox` with `default-features = false`, create one
`vault_mailbox::Mailboxes::new(MailboxConfig { allowed_origins: vec![...], .. })`
and route `GET /mailbox/<id>` to

```rust
mailboxes.handle_from(id, req, Some(peer_ip)).await
```

where `peer_ip` is the TCP peer address (the relay replaces it with the first
`X-Forwarded-For` hop only if you set `trust_proxy`). Serve the connection with
hyper's upgrades enabled (`serve_connection(..).with_upgrades()`), and call
`mailboxes.shutdown()` when your server stops so open mailboxes close cleanly
(1001). In a `MailboxConfig` built in code, `allowed_origins` defaults to empty,
which admits **no** browser origin (only Origin-less native clients): list your
app's origin, in canonical form (`vault_proto::binding::normalize_origin`),
since the relay compares it byte for byte with the browser's `Origin` header.
The same TLS, proxy and limit notes as in A apply; `MailboxConfig`'s fields
match the flags above.

Either way, pass the relay's public `https://` origin to `pair` as
`relay_origin` (section 3).

If your page has a Content-Security-Policy, its `connect-src` must list the
relay's WebSocket origin. That is `wss://relay.example` for `https://relay.example`,
or `ws://127.0.0.1:8787` for a loopback `http://` relay. Browsers check the
`wss:`/`ws:` URL itself, and an `https:` source does not match it: Chrome blocks
the dial. `examples/todo` shows a complete policy.

## 3. Pair

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

## 4. What the UI must show

| State | Show |
|---|---|
| `Pairing` | The QR code (from `on_offer`) and a short line: "Scan with your vault". Offer expires after 60 s. A **Cancel** here (`cancel_pairing`) closes the session as `Denied` at once. |
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
  the vault approved: the session closes as `Denied` and `Active` is never
  reported. Until both approvals have happened the client sends no store
  request. While your `on_activate` hook runs the session already has a live
  backend, so the hook's own requests (reads, and even a `flush_commit`) can
  reach the vault before a cancel arrives; a later cancel still ends the
  session as `Closed(Denied)`, drops the backend and runs `on_deactivate`.
  After `Active`, `cancel_pairing()` returns an error; use `lock()`.
- Keep `on_activate` read-only: no writes and no commit there, so a cancel
  never leaves new data behind. Write only after `Active`.

## 5. Commit before you acknowledge or transmit

Anything derived from state must be durable in the vault before it leaves the
app. Call `backend.flush_commit().await` and only then acknowledge a message
to its sender, send a reply, or tell the user something is saved.

Why: writes are buffered in RAM until `flush_commit`, and the session can end
at any time (phone locked, network lost). If you acknowledge first and the
session dies, the other side believes the data arrived and will not send it
again, and it is lost. If a commit fails the backend is dead and
`flush_commit` returns `ClientError::CommitFailed`: treat the writes as not
durable, do not acknowledge or transmit, and let the session close.

### Traffic and commit batching

What a session costs on the network, so you can decide how often to commit:

- **Requests** are small. A `get`, `put` or `list` of a typical app record (a
  to-do, a setting) is a few hundred bytes of Noise frames through the relay.
- **Keepalive.** An open session pings every 30 s at most; through the relay
  that is about 15 to 20 KB per hour, idle or not. `lock()` when the user is done.
- **Commits** are the expensive part. Each `flush_commit` (`commit()` in
  `vault-web`) makes the vault seal a new segment of at least 64 KiB and upload
  it to every replica (S3, WebDAV, ...), however small the change was.

So batch: buffer writes and commit once after a short quiet period instead of
once per keystroke or click. The to-do example (`examples/todo`) commits 500 ms
after the last edit, so a burst of edits becomes one segment. Batching does not
relax section 5: commit, and wait for it, before you acknowledge or transmit
anything derived from the writes. A `Closed` inside the quiet period drops the
buffered writes (section 6), so keep the window short and show the user a
"saving…" state until the commit has returned.

## 6. Handling `Closed(reason)`

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
that is why section 5 matters.

## 7. Unlink and `Clear`

"Unlink" in an app is `backend.clear()` followed by `flush_commit()`. Because a
vault is bound to one app and origin, this erases that app's data in the vault
and nothing else. Replicas receive the empty state at the next replication, but
older replica snapshots keep the old data until the replica's own retention
expires them. The vault does not control that retention; tell users.

## 8. Depend on a pinned revision

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
