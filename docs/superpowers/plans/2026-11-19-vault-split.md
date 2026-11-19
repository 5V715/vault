# Vault split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the vault stack out of `5V715/signal-web` into `5V715/vault` as a standalone, app-neutral MIT project, and make signal-web consume it as a pinned git dependency.

**Architecture:** Approach A. Phase 1 copies the crates unchanged into a new workspace and extracts the mailbox relay and the browser client into their own crates. Phase 2 switches signal-web to pinned git dependencies and deletes its copies. Phase 3 generalizes: neutral labels, versions, app and origin binding, and the Cancel fix. It ends with one signal-web bump.

**Tech Stack:** Rust 2024 (rust-version 1.93), tokio, hyper, tokio-tungstenite, snow (Noise XX), AES-GCM/HKDF, wasm32 (web-sys), GitHub Actions; Python `cryptography` for KATs.

**Spec:** `docs/superpowers/specs/2026-11-19-vault-split-design.md` (this repo). Background: signal-web `docs/superpowers/specs/2026-11-18-vault-design.md` and `docs/protocol/vault.md`.

Repos: `VAULT` = `/home/claude/vault` (branch `main`), `SW` = `/home/claude/signal-web` (branch `vault-design`). Baseline for the copy: SW commit at the start of Task 1 (currently `4324d53`).

## Global Constraints

- License: every file in VAULT is MIT (`LICENSE` = MIT, © 2026 Silas Schwarz; `license = "MIT"` in the workspace). No VAULT crate may depend on anything AGPL, including signal-web's `core`.
- signal-web stays `AGPL-3.0-only` and depends on VAULT only via `git = "https://github.com/5V715/vault"` plus an exact `rev`. Never a branch or tag.
- Do not weaken any security property while moving code: commit before send, acks only after commit, the two-sided SAS confirmation, high-water anti-rollback, manifest-auth tag, zeroization, strict decoding and caps, only `OpenSession`/`Ping`/`Close` before Approved, Ping ≤ 30 s.
- Phase 1–2 are moves: wire format, labels, QR prefix and recovery prefix stay byte-identical until Task 5. Prose is neutralized immediately ("app", "vault"; no "Signal").
- Phase 3 labels (exact): QR prefix `vault-pair:v1:`; recovery prefix `vault-recovery:v1:`; HKDF infos `vault/v1/enc`, `vault/v1/mac`, `vault/v1/manifest`, `vault/v1/manifest-auth`, `vault/v1/manifest-auth-tag`, `vault/v1/replica-id`, `vault/v1/recovery`, `vault/v1/recovery-check`, `vault/v1/crk-wrap`; Noise prologue label `vault/v1`; SAS info `vault/v1/sas`.
- Versions start at 1: protocol version byte in `PairOffer`, container format version in the header. Unknown version → reject. Container without a version (pre-split) → error text `unsupported container format (created before vault v1)`.
- App binding: `app_id` matches `^[a-z0-9][a-z0-9.-]{0,63}$`; `origin` is `https://host[:port]` or `http://` only for `127.0.0.1`, `[::1]`, `localhost`; no path, query or fragment; lower-case host. Mismatch → `Notice::WrongApp` before any approval prompt.
- CI secret in signal-web: `VAULT_GIT_TOKEN` (read-only, fine-grained, owner-created). It is only ever used in a `cargo vendor` step on the runner; never passed to `docker build`, never echoed.
- Never force-push, never amend pushed commits, never commit `*.key` or secrets. Commit trailers:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019sALvTRSTdS4PZJbSXGRqj`.
- Gates per repo: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`, `cargo build --target wasm32-unknown-unknown` for `vault-proto` and `vault-client` (CI only where the target is unavailable locally). In SW additionally `npm run --prefix ui build` and `node --test 'ui/test/*.test.mjs'`.

## Review Focus

- **The move drops a test.** Every test that existed in SW for moved code must exist in VAULT (or remain in SW if it needs Signal) — compare the name lists. Test: Task 1/2/3 each add `docs/MOVED_TESTS.md` rows; Task 4 checks that SW still contains every Signal-dependent vault test (`link_and_receive_through_vault`, `ack_only_after_vault_commit`, `drop_mid_session_discards_pending_and_redelivers`, `cache_dropped_on_lock`, `no_local_persistence`, `commit_failure_prevents_send`, `concurrent_send_during_receive_does_not_commit_partial_envelope`).
- **A user opens an old container after Task 5/6.** They get the clear pre-v1 error, not a decrypt or MAC failure. Test: `pre_v1_container_is_rejected_with_clear_error` (Task 6).
- **The wrong app or origin reaches a vault.** It is refused before the approver is asked, and the browser sees `WrongApp`, not a hang. Tests: `wrong_app_refused_before_approval`, `wrong_origin_refused_before_approval` (Task 6), `wrong_app_maps_to_closed_reason` (Task 8).
- **Cancel after "Codes match" but before Approved.** The session must close as denied, never go Active. Test: `cancel_after_confirm_before_approval_wins` (Task 7).
- **The private dependency leaks into the image.** No token in the image layers or build logs. Test: Task 4's workflow step greps `docker history --no-trunc` and the vendored tree for the token value and fails if found (`image_has_no_vault_token` CI step).

---

## Phase 1 — copy as is (VAULT)

### Task 1: Workspace, container, protocol, server

**Files:**
- Create: `Cargo.toml` (workspace: `vault-container`, `vault-proto`, `vault-server`), `LICENSE` (MIT), `README.md`, `.gitignore` (`/target`, `*.key`), `.github/workflows/ci.yml`, `docs/MOVED_TESTS.md`, `rust-toolchain.toml` if SW has one
- Copy: SW `container/` → `vault-container/` (package name `vault-container`, lib name `vault_container`), SW `vault-proto/` → `vault-proto/`, SW `vault-server/` → `vault-server/`, SW `docs/protocol/vault.md` → `docs/protocol/vault.md`, SW `docs/vault/README.md` → `docs/vault/README.md`, the vault sections of SW `2026-11-18-vault-design.md` → `docs/design/vault.md`
- Modify: SPDX headers `AGPL-3.0-only` → `MIT` in copied files; crate docs and prose neutralized; the argon2 profile overrides from SW's workspace

**Interfaces:**
- Consumes: SW at baseline commit.
- Produces: crates `vault_container`, `vault_proto`, `vault_server` with SW's public APIs unchanged (only the crate name `container` → `vault_container`).

- [ ] **Step 1:** Record baseline: `git -C SW rev-parse HEAD` → write it into README "Origin" line and the commit message.
- [ ] **Step 2:** Copy the crates and docs, rename the package, and fix `path` deps (`vault-server` → `vault-container = { path = "../vault-container", default-features = false }`). Replace SPDX headers and neutralize prose, leaving label strings untouched (`grep -rn '"signal-web' ` must show only label constants, the QR/recovery prefixes and their tests).
- [ ] **Step 3:** Run `cargo test --workspace`. Expected: all tests moved from SW pass. Fill `docs/MOVED_TESTS.md` with every test name from the three SW crates (`cargo test -- --list` in SW vs VAULT, same counts).
- [ ] **Step 4:** CI `ci.yml`: fmt, clippy `-D warnings`, test, `cargo build -p vault-proto --target wasm32-unknown-unknown` (same getrandom/rustls wasm feature handling as SW CI).
- [ ] **Step 5:** Run fmt and clippy locally. Expected: clean.
- [ ] **Step 6:** Commit `vault: workspace with vault-container, vault-proto, vault-server (copied from signal-web <hash>)`.

### Task 2: `vault-mailbox` and `vault-relay`

**Files:**
- Create: `vault-mailbox/src/lib.rs` (from SW `relay/src/mailbox.rs` plus the `mailbox_route` logic and its config fields from SW `relay/src/lib.rs`/`config.rs`), `vault-mailbox/src/bin/vault-relay.rs`, `vault-mailbox/tests/mailbox.rs` (from SW `relay/tests/mailbox.rs`), `Dockerfile.relay`, CI job `relay-image`

**Interfaces:**
- Consumes: SW relay mailbox code.
- Produces:
  - `pub struct MailboxConfig { max_mailboxes_waiting, idle timeout, frame caps, allowed_origins: Vec<String>, ... }` (exact fields = SW's mailbox-related config, carried over).
  - `pub struct Mailboxes` (the registry, `Clone`, `Arc` inside) with `Mailboxes::new(cfg: MailboxConfig)`.
  - `pub async fn handle(&self, id: &str, req: hyper::Request<hyper::body::Incoming>) -> hyper::Response<http_body_util::Full<bytes::Bytes>>`. It performs Origin checking, the WS upgrade and pairing exactly as SW's `mailbox_route`.
  - Binary `vault-relay --listen <addr> --allow-origin <origin>...` serving only `GET /mailbox/<32hex>` and `GET /healthz`.

- [ ] **Step 1:** Move the tests first (`pairs_two_and_pipes_both_ways`, `first_may_send_before_peer_arrives`, `third_connection_rejected`, `bad_id_rejected`, `oversize_frame_closes_pair`, `origin_rules_for_mailbox`, `waiting_mailboxes_capped`, `waiting_client_close_is_echoed`, `text_frames_rejected`), retargeted at a `vault-relay` server started in-process. Run: they fail (crate missing).
- [ ] **Step 2:** Implement the library by moving the code, then the binary. Run the tests. Expected: all 9 pass.
- [ ] **Step 3:** `Dockerfile.relay`: scratch image, static musl binary via cargo-zigbuild; CI builds it for `linux/amd64,linux/arm64,linux/arm/v7` (smoke: `/healthz` 200). This is a reduced matrix by design; SW's image keeps the full matrix.
- [ ] **Step 4:** Update `docs/MOVED_TESTS.md`; fmt/clippy/test. Commit `vault-mailbox: relay mailbox library and vault-relay binary`.

### Task 3: `vault-client`

**Files:**
- Create: `vault-client/src/{lib.rs,transport.rs,rpc.rs,backend.rs,session.rs,wasm.rs,native.rs,error.rs}`, `vault-client/tests/*.rs`
- Source: SW `core/src/remote/{mod,rpc,backend,testkit,tests}.rs`, the pairing lifecycle in SW `core/src/client.rs` (`PairState`, `PairCallbacks`, `pair`, `pair_with`, `confirm_sas`, `cancel_pairing`, `pair_session`, `pair_run`, ping and close handling, constants `PAIR_WAIT_MS`, `PING_TIMEOUT_MS`, `CLOSE_WAIT_MS`), SW `core/src/wasm/transport.rs` (WebSocket mailbox transport only)

**Interfaces:**
- Consumes: `vault_proto`; dev-deps `vault_server`, `vault_mailbox`, `vault_container` (tests).
- Produces (no dependency on SW):
  - `pub enum ClientError` (its own; covers transport, protocol, closed, commit failed, denied).
  - `pub trait Clock { fn now_ms(&self) -> u64; fn sleep(&self, ms: u64) -> LocalBoxFuture<'static, ()>; }` and `pub trait Rng { fn fill(&self, buf: &mut [u8]); }`, replacing SW's `chat::Clock` and `SystemRng`.
  - `#[async_trait(?Send)] pub trait MailboxTransport` (unchanged shape), plus `pub fn mailbox_url(relay_origin, id) -> String`; `wasm::WsTransport` (feature `wasm`) and `native::TokioMailbox` (feature `native`).
  - `#[derive(Clone)] pub struct RemoteBackend` with async `get`, `put`, `put_if_absent`, `delete`, `scan_prefix`, `clear`, `flush_commit`, all returning `Result<_, ClientError>`. Semantics identical to SW's `KvBackend` impl.
  - `pub enum SessionState { Pairing, AwaitingApproval, Active, Closed(CloseReason) }` and `pub enum CloseReason { Denied, Terminated, Locked, Failed(String), ... }` (variants = the reasons SW maps today).
  - `pub struct VaultSession` with `pub fn pair(transport_factory, clock, rng, relay_origin, device_name, callbacks: SessionCallbacks) -> Result<(VaultSession, LocalBoxFuture<'static, ()>), ClientError>`, `confirm_sas()`, `cancel_pairing()`, `lock()`, `backend() -> Option<RemoteBackend>` (Some only while Active), and `on_state`/`on_offer`/`on_sas` callbacks in `SessionCallbacks`.
- signal-web's Signal-specific parts (commit gate held across envelope handling, starting/stopping the Signal loop) stay in SW and hook in through `on_state` and `RemoteBackend::flush_commit`.

- [ ] **Step 1:** Move the tests that need no Signal into `vault-client/tests/`: `writes_are_buffered_until_commit_and_then_visible_to_a_new_session`, `uncommitted_writes_are_lost_with_the_session`, `scan_merges_the_overlay_and_put_if_absent_is_correct`, `clear_removes_everything_after_commit`, `a_failed_commit_kills_the_backend`, `requests_before_approval_not_sent`, `denied_pairing_is_an_error`, `nothing_is_accepted_before_approval_and_denial_closes`, `active_requires_local_sas_confirmation`, `confirmation_before_vault_approval_also_works`, `confirm_before_code_shown_is_refused`, `cancel_aborts_with_denied`, `impostor_vault_with_qr_text_cannot_reach_active_without_confirmation`, `ping_keeps_session_alive`. Run: they fail.
- [ ] **Step 2:** Implement by moving the code; replace `CoreError`, `chat::Clock` and `SystemRng` with the crate's own types; keep the `Zeroizing` buffers. Run the tests. Expected: all 14 pass.
- [ ] **Step 3:** CI: `cargo build -p vault-client --target wasm32-unknown-unknown --features wasm`.
- [ ] **Step 4:** Update `docs/MOVED_TESTS.md` (list the Signal-dependent tests that stay in SW, by name). fmt/clippy/test. Commit `vault-client: browser/app side of the vault protocol`.
- [ ] **Step 5 (stop point):** Push `main` to `5V715/vault` — only after the owner has approved pushing (it is the default branch of the new repo). CI must be green; record the commit hash for Task 4.

## Phase 2 — signal-web consumes (SW)

### Task 4: Pinned git dependencies; delete the copies

**Files:**
- Delete: SW `container/`, `vault-proto/`, `vault-server/`, `relay/src/mailbox.rs`, `relay/tests/mailbox.rs`, `core/src/remote/`, the pairing lifecycle in `core/src/client.rs`, `core/src/wasm/transport.rs` (if fully moved), `docs/protocol/vault.md`, `docs/vault/README.md` (replaced by a pointer to VAULT)
- Modify: SW `Cargo.toml` (workspace members), `core/Cargo.toml`, `relay/Cargo.toml`, `relay/src/lib.rs` (mount `vault_mailbox::Mailboxes::handle` at `/mailbox/<id>`), `relay/src/config.rs` (builds `MailboxConfig`), `core/src/store/*` (`VaultKv` newtype adapter: `impl KvBackend for VaultKv(vault_client::RemoteBackend)`, mapping `ClientError` → `CoreError`; `flush` → `flush_commit`), `core/src/client.rs` (uses `VaultSession`; keeps the commit gate and the Signal loop start/stop on `SessionState`), `core/src/wasm/*` (exports unchanged), `.github/workflows/{ci,release}.yml`, `Dockerfile`, `docs/RELEASE.md`

**Interfaces:**
- Consumes: Task 1–3 public APIs at the pushed VAULT commit.
- Produces: unchanged signal-web behaviour and an unchanged wasm/TS contract (`ui/src/core/signal_web_core.d.ts` byte-identical).

- [ ] **Step 1:** Set the deps: `vault-proto`, `vault-client` (features `wasm` on wasm32, `native` for tests), and dev-deps `vault-server`, `vault-mailbox`, `vault-container`, all `git = "https://github.com/5V715/vault", rev = "<Task 3 hash>"`. `relay` depends on `vault-mailbox`.
- [ ] **Step 2:** Delete the copies and wire the adapter and `VaultSession`. Run `cargo test --workspace`. Expected: all remaining SW tests pass, including the Signal-dependent vault tests listed in Review Focus, now built on the VAULT crates.
- [ ] **Step 3:** Check `git diff --stat ui/src/core/signal_web_core.d.ts` is empty and that the UI build and tests pass.
- [ ] **Step 4:** CI and image build:
  - Before any cargo step, add `git config --global url."https://x-access-token:${VAULT_GIT_TOKEN}@github.com/5V715/vault".insteadOf "https://github.com/5V715/vault"` in a step whose env has the secret, plus `CARGO_NET_GIT_FETCH_WITH_CLI=true`.
  - Image job: `cargo vendor --locked vendor/ > .cargo/config.vendor.toml` on the runner. The Dockerfile copies `vendor/` and builds with `--offline --config .cargo/config.vendor.toml`.
  - Step `image_has_no_vault_token`: `docker history --no-trunc` plus `grep -r` over `vendor/` for the token must find nothing. Remove the `insteadOf` config before `docker build`.
- [ ] **Step 5:** Update `docs/RELEASE.md` (owner: create the fine-grained read-only token for `5V715/vault`, store it as `VAULT_GIT_TOKEN`; how to bump the rev). Commit in steps (`deps:`, `core:`, `relay:`, `ci:`).
- [ ] **Step 6:** Push `vault-design`. Expected: CI green incl. wasm and image-smoke (needs the owner's secret; if absent, report BLOCKED with that reason).

## Phase 3 — generalize (VAULT, then one SW bump)

### Task 5: Neutral labels, versions, KATs

**Files:**
- Modify: `vault-container/src/keys.rs`, `vault-container/src/manifest.rs`, `vault-proto/src/lib.rs`, `vault-proto/src/msg.rs`, tests in all three crates, `docs/protocol/vault.md`, `docs/vault/README.md`
- Create: `tools/kat/kat.py` (Python, `cryptography`), `tools/kat/README.md`

**Interfaces:**
- Produces: the labels from Global Constraints; `PairOffer { version: u8 = 1, ... }` (wire field first); container header `format_version: u8 = 1`. Constants `PROTOCOL_VERSION: u8 = 1`, `CONTAINER_FORMAT_VERSION: u8 = 1`.

- [ ] **Step 1:** `tools/kat/kat.py` prints the KAT values: `sas_from_hash([0;32])` under the new SAS info; the HKDF subkeys from CRK = `[0x11; 32]` for each info; the recovery checksum for CRK `[0x11; 32]`. Run it and record the output.
- [ ] **Step 2:** Write the failing tests with those values (`sas_kat_v1`, `subkey_kats_v1`, `recovery_checksum_kat_v1`, `offer_rejects_unknown_version`, `header_rejects_unknown_format_version`). Run: they fail.
- [ ] **Step 3:** Change the constants, add the version fields, and reject unknown versions. Run the full workspace tests. Expected: pass (old fixtures regenerated, never hand-edited).
- [ ] **Step 4:** Docs: protocol doc and README show the new prefixes. Commit `vault: neutral v1 labels, protocol and format versions, independent KATs`.

### Task 6: App and origin binding

**Files:**
- Modify: `vault-proto/src/msg.rs` (`PairOffer.app_id`, `PairOffer.origin`, `Notice::WrongApp`), `vault-proto/src/noise.rs` (prologue), `vault-container` header plus manifest auth, `vault-container/src/bin/vaultctl.rs` (`init --app <id> --origin <url>`, `info` prints the binding), `vault-server/src/lib.rs` (check before the approver), `vault-server/src/approver.rs` (prompt shows app, origin, relay host), `vault-server/src/bin/vault-dev.rs`, `vault-client` (`pair` takes `app_id`, `origin`; `CloseReason::WrongApp`), docs

**Interfaces:**
- Produces:
  - `PairOffer { version, relay_url, mailbox_id, browser_eph_pub, app_id: String, origin: String }`, validated per Global Constraints on encode and decode.
  - Prologue = `b"vault/v1" ‖ [version] ‖ u16be(len) app_id ‖ u16be(len) origin ‖ mailbox_id ‖ browser_eph_pub`.
  - `Container::binding() -> (&str, &str)`.
  - `VaultSession::pair(..., app_id: &str, origin: &str, ...)`.

- [ ] **Step 1:** Failing tests: `offer_roundtrip_with_app_binding`, `offer_rejects_bad_app_id`, `offer_rejects_non_https_origin`, `prologue_binds_app_and_origin` (changed origin → handshake fails), `wrong_app_refused_before_approval`, `wrong_origin_refused_before_approval` (the approver is never called; the client gets `CloseReason::WrongApp`), `binding_is_covered_by_manifest_auth` (a tampered header fails to open), `pre_v1_container_is_rejected_with_clear_error` (fixture produced by the Task 4 baseline code, checked in under `vault-container/tests/fixtures/pre-v1/`), `vaultctl_init_requires_app_and_origin`. Run: they fail.
- [ ] **Step 2:** Implement. Run the full workspace tests. Expected: pass.
- [ ] **Step 3:** Docs: protocol threat notes state that the origin is a declared, pinned label, not authenticated; the README covers `vaultctl init --app --origin`. Commit `vault: bind a vault to one app and origin`.

### Task 7: Cancel wins, Clear scope, integration guide

**Files:**
- Modify: `vault-client/src/session.rs`, `docs/protocol/vault.md` (Clear = erase the bound app's data), `README.md`
- Create: `docs/integration.md`

- [ ] **Step 1:** Failing test `cancel_after_confirm_before_approval_wins`: confirm, then cancel before Approved → `Closed(Denied)`, never `Active`, no data request sent. Run: it fails.
- [ ] **Step 2:** Implement: `cancel_pairing()` wins at any point before Active. Run the vault-client tests. Expected: pass.
- [ ] **Step 3:** `docs/integration.md`: how an app pairs (`VaultSession::pair` with its `app_id` and origin), what it must show (QR; SAS large with "Codes match" and "Cancel"; never act before Active), commit before acknowledging or transmitting anything derived from state, handling `Closed` reasons, `vaultctl init --app --origin`, pinning a rev. Commit `vault-client: cancel wins until Active; integration guide`.
- [ ] **Step 4 (stop point):** Push `main`; CI green; record the hash.

### Task 8: signal-web bump

**Files:**
- Modify: SW `Cargo.toml` revs, `core/src/client.rs` (pass `app_id = "signal-web"`, origin), `core/src/wasm/*` (`pair` takes `origin` from the worker: `self.location.origin`), `ui/src/worker-logic.ts`, `ui/src/protocol.ts` (closed reason `wrongApp`), `ui/src/renderer/*` (copy: "Open your vault app on your phone and scan"; a wrong-app notice), `ui/src/core/signal_web_core.d.ts`, SW docs pointer, UI tests

- [ ] **Step 1:** Failing tests: `wrong_app_maps_to_closed_reason` (core), `pair_screen_copy_mentions_vault_app` and `wrong_app_notice_rendered_as_text` (ui). Run: they fail.
- [ ] **Step 2:** Bump the revs to the Task 7 hash and implement. Run all SW gates. Expected: pass.
- [ ] **Step 3:** Commit and push `vault-design`. CI green. The owner recreates the test vault with `vaultctl init --app signal-web --origin <their origin>`, documented in SW README.
