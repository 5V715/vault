# Vault as a standalone project — design

Status: draft for owner approval · 2026-11-19 · repo `5V715/vault`

## Intent

The vault is currently built inside `5V715/signal-web`. That includes the encrypted
container, the pairing/session protocol, the session server, `vaultctl`/`vault-dev`,
the relay mailbox and the browser-side client. Nothing in it understands Signal, but
its names and labels tie it to signal-web (`signal-web-pair:v1:`,
`signal-web-recovery:v1:`, `signal-web/vault/v1/*`).

The owner wants the vault, its protocol and all of its tooling to stand on their own
in `5V715/vault`. Other (web) apps should be able to use it as their persistence.
signal-web becomes one consumer among others.

## Goals

- `5V715/vault` holds the whole vault stack, builds and tests on its own, and has
  its own CI, protocol document, README and MIT license.
- signal-web depends on the vault crates as git dependencies pinned to an exact
  commit, and no vault code remains in signal-web.
- The vault is generic: no Signal names, labels or copy anywhere in it.
- A vault is bound to one app and one web origin ("for now"). The protocol and
  container carry the app identity and a version number, so serving several apps
  from one vault later is an extension, not another breaking change.
- signal-web keeps working at every step (approach A below).

## Non-goals

- Several apps per vault (later; the format must not rule it out).
- Publishing to crates.io or npm.
- The Android vault app (V2b). It will live in this repo, but it is not part of
  this split.
- A JavaScript/npm SDK for third-party web apps. *My call:* this split delivers the
  Rust `vault-client` crate, which compiles to wasm32 and runs inside an app's own
  wasm core, as signal-web does. A wasm-bindgen/TypeScript package is a follow-up
  once a second app exists. Building it now would mean guessing at its API.
- A reusable UI kit for the pairing screens. The screens depend on each app's
  security model (signal-web's sandboxed renderer). The protocol doc specifies what
  an app must show instead.

## Anti-goals

- Weakening any V1/V2a security property while moving code. These include the
  commit-before-send rule, acks only after commit, two-sided SAS confirmation,
  anti-rollback (high water), manifest authentication and zeroization.
- A vault that silently accepts a session from a different app or origin than the
  one it is bound to.
- Floating dependencies (branch or tag) from signal-web on the vault.

## Constraints

- Owner: maximum security; if the vault is unreachable, the app does not work; all
  persistent data lives only in the vault container.
- License MIT for `5V715/vault`; signal-web stays AGPL-3.0-only (libsignal). The
  vault crates must not depend on anything AGPL.
- Fresh history in `5V715/vault` (the owner does not need it preserved). The first
  commit records the signal-web commit it was copied from.
- Product name: "vault".
- Private repository. signal-web CI and its OCI image build fetch the vault through
  a read-only deploy key (secret `VAULT_DEPLOY_KEY`), with
  `CARGO_NET_GIT_FETCH_WITH_CLI=true`. The owner creates the key; the
  workflows read it from a secret and never log it.
- Toolchain and policy as in signal-web: Rust edition 2024, rust-version 1.93,
  `cargo fmt`, `clippy -D warnings`, wasm32 build in CI for the client crates.
- Existing containers (the owner's local test vault and its WebDAV replica) stop
  opening after phase 3. The owner recreates them; nothing is released, so there is
  no migration.

## Approaches (owner picked A)

- **A — move first, then generalize (picked).** Copy the crates into `5V715/vault`
  unchanged, get CI green, and switch signal-web to pinned git dependencies. Then
  make every breaking or generalizing change in the vault repo and bump signal-web
  once. signal-web works throughout, each step is reviewable on its own, and
  containers break exactly once.
- B — generalize inside signal-web, then copy. Fewer cross-repo bumps, but the
  vault's design gets shaped inside the Signal repo, which is the coupling to remove.
- C — clean-room rebuild. Cleanest, but it discards tested and reviewed code and
  its tests.

## Target layout of `5V715/vault`

| Crate | From | Purpose |
|---|---|---|
| `vault-container` | signal-web `container/` (renamed) | Encrypted container format, engine, backends (local, S3, WebDAV, memory), replication, restore, `vaultctl` binary, `secrets` module |
| `vault-proto` | `vault-proto/` | Wire messages, pair offer, chunking, Noise XX, SAS |
| `vault-server` | `vault-server/` | Vault side of a session: `VaultServer`, `Approver`, compaction policy, `vault-dev` binary |
| `vault-mailbox` | signal-web `relay/src/mailbox.rs` plus the `/mailbox` route | Relay mailbox as a library (registry plus a hyper upgrade handler and its limits) and a small `vault-relay` binary that serves only `/mailbox/<id>` |
| `vault-client` | signal-web `core/src/remote/*` plus the pairing lifecycle in `core/src/client.rs` (`pair`, `confirm_sas`, `cancel_pairing`, ping, close handling) | App side: `MailboxTransport` (wasm web-sys and native tokio), `Rpc`, `RemoteBackend` with write-back cache, overlay, `flush_commit` and commit gate, `VaultSession` state machine (pairing → awaitingApproval → active → closed) with two-sided confirmation |

Docs: `docs/protocol/vault.md` (wire protocol, now app-neutral);
`docs/vault/README.md` (vaultctl, vault-dev, replication, recovery);
`docs/design/vault.md` (the vault parts of signal-web's
`2026-11-18-vault-design.md`, moved and de-Signal-ed); an **integration guide**
saying what an app must do: show the QR, show the SAS with Confirm/Cancel, never
treat the session as active before both confirmations, and commit before
acknowledging or transmitting anything derived from state.

signal-web keeps: libsignal core (it uses `vault-client` as its `KvBackend`), its
relay (tunnel plus static files; it mounts `vault-mailbox`'s handler at `/mailbox`),
UI, signer and image.

## Phases

### Phase 1 — copy as is (vault repo)

- Workspace with the crates above. `container` is renamed to `vault-container`, and
  `vault-mailbox` and `vault-client` are extracted. *My call:* the extraction of
  `vault-mailbox` and `vault-client` happens in phase 1, not phase 3, because
  signal-web cannot depend on code that still sits inside its own crates. The
  extraction is a move; behaviour, labels and wire format stay unchanged.
- MIT `LICENSE`, README, CI (fmt, clippy, tests, wasm32 build of `vault-client` and
  `vault-proto`; a `vault-relay` and `vault-dev` build for the common architectures
  is my call, mirroring signal-web's multi-arch matrix only for `vault-relay`).
- The tests move with their code. signal-web's core tests that need an in-process
  relay plus vault use `vault-mailbox` and `vault-server` as dev-dependencies.

### Phase 2 — signal-web consumes (signal-web repo)

- Delete `container/`, `vault-proto/`, `vault-server/`, `relay/src/mailbox.rs` and
  `core/src/remote/`, and the pairing lifecycle moves out of `core/src/client.rs`.
  Depend on the vault crates by `git` plus `rev`.
- Relay mounts `vault_mailbox` at `/mailbox`; the OCI image is unchanged in shape.
- CI and image builds fetch the private repo via the secret. All signal-web tests
  pass, and the wasm build stays green.

### Phase 3 — generalize (vault repo, then one bump in signal-web)

1. **Neutral labels:** `vault-pair:v1:`, `vault-recovery:v1:`, `vault/v1/*` for
   every HKDF info, the Noise prologue label and the SAS info; signal-web copy is
   removed from crate docs, the approver prompt and errors. New known-answer test
   (KAT) values are computed independently and recorded.
2. **App binding:**
   - `PairOffer` gains `app_id` (1–64 chars of `[a-z0-9.-]`) and `origin` (an
     `https://` origin, or `http://` only for loopback).
   - The Noise prologue binds `label ‖ version ‖ app_id ‖ origin ‖ mailbox_id ‖
     browser_eph_pub` (length-prefixed fields).
   - The container header stores the bound `(app_id, origin)`, set at creation
     (`vaultctl init --app <id> --origin <url>`) and covered by manifest
     authentication.
   - `VaultServer` refuses a session whose offer does not match, with a distinct
     notice (`WrongApp`) before any approval prompt.
   - Approvers show the app ID, origin and relay host.
3. **Versioning:** a protocol version byte in the offer, and a format version in
   the container header. Unknown versions are rejected.
4. **Cancel fix:** Cancel always wins until the session is Active (open residual
   from V2a).
5. **Clear scope:** `Clear` erases the bound app's data. With one app per vault
   that is the whole container (unchanged for now); the protocol doc defines it as
   app-scoped so multi-app does not change its meaning.
6. signal-web bumps to the new commit, passes `app_id = "signal-web"` and its page
   origin, and updates its UI copy ("Open your vault app on your phone and scan").

## Settled details (builder check)

- **Baseline:** phase 1 copies signal-web `vault-design` at the commit current when
  phase 1 starts. The vault repo's first commit message records that hash. signal-web
  does not change vault code in the meantime.
- **Trait ownership (my call):** `KvBackend` stays in signal-web's AGPL core. The
  MIT `vault-client` must not depend on it. `vault-client` exposes `RemoteBackend`
  with its own async API (`get`, `scan_prefix`, `put`, `put_if_absent`, `delete`,
  `flush_commit`, `commit_gate`). signal-web implements `KvBackend` for a thin
  newtype adapter. The same rule applies to error types: `vault-client` has its own,
  and signal-web maps them.
- **wasm glue (my call):** the `wasm-bindgen` exports and the JS/TS contract stay in
  signal-web, which wraps `vault-client`. Phase 2 keeps the TS contract and UI
  byte-identical. UI copy changes only in phase 3.
- **What `origin` means:** a declared, pinned label, not an authenticated claim. The
  page supplies it (`location.origin` in wasm; native and test clients pass any
  origin they like). The vault cannot verify it. The vault pins it at container
  creation and refuses mismatches (`WrongApp`), which keeps apps apart and catches
  mistakes. Protection against a malicious page still rests on the user checking the
  origin and the SAS on the phone. The protocol doc's threat notes say so explicitly.
- **Private repo in the image build (my call):** CI runs `cargo vendor` on the
  runner, using the deploy key, before `docker build`. The
  Dockerfile builds offline from the vendored sources, so no secret enters the build
  context or any image layer. Local developer builds use the developer's own git
  credentials.
- **Phase 1 docs (my call):** prose is neutralized right away ("the app", "vault").
  Label values, the QR prefix and the recovery prefix stay exactly as they are
  until phase 3, documented with a note that phase 3 renames them.
- **Versions and old containers (my call):** the protocol version and the container
  format version both start at 1. Opening a container from before the split (no
  version field) fails with a clear "unsupported container format (created before
  vault v1)" error, not a generic decrypt failure. There is no compatibility shim.
- **KATs (my call):** new label KATs (SAS, HKDF subkeys, recovery checksum) are
  computed by a small Python script using the `cryptography` package under
  `tools/kat/` in the vault repo. The values are pasted into Rust tests, as was
  done for the V2a SAS KAT.

## What the builder decides

Internal module layout within each crate, constants such as limits and compaction
thresholds (carried over unless a reason appears), the CI matrix details, and how
`vault-client` exposes its callbacks (keep the current `PairCallbacks` shape unless
a cleaner Rust API falls out of the extraction).

## Owner decisions

- 2026-11-19: approach A; MIT; fresh history; name "vault"; one vault per app and
  origin for now.
- 2026-11-19: **unlink erases the app's vault** (current behaviour kept). A removed
  linked device cannot be recovered; relinking creates a new device, and Signal's
  link-time history transfer from the phone restores history. Receiving that
  transfer in signal-web belongs to its history milestone. Older replica snapshots
  keep old data until they expire; this is documented, not fixed.
