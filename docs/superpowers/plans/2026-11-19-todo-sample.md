# To-do sample and vault-web Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A browser package `vault-web` over `vault-client`, and a minimal to-do web app that persists only to the vault, both tested end to end against the real `vault-relay` and `vault-dev`.

**Architecture:** A new crate `vault-web` exports a small JS API via `wasm-bindgen`, with a typed TS wrapper. `examples/todo` is a plain-TS single page bundled with esbuild. A Node 22 e2e suite drives real binaries.

**Tech Stack:** Rust (wasm32), wasm-bindgen(-cli, pinned), TypeScript, esbuild, Node 22 test runner.

**Spec:** `docs/superpowers/specs/2026-11-19-todo-sample-design.md`. App contract: `docs/integration.md`.

## Global Constraints

- MIT, like the rest of the repo. JS dependencies are pinned with a committed lockfile, the fewest possible: `esbuild` and `typescript` only. The `wasm-bindgen-cli` version equals the `wasm-bindgen` crate version in `Cargo.lock`.
- The integration guide is binding:
  - no data request before Active;
  - the SAS is shown with "Codes match" and "Cancel";
  - `on_activate`-equivalent work is read-only;
  - closing discards pending edits;
  - the origin comes from `location.origin` in browsers.
- The page CSP is exactly: `default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src <relay origin>; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`. All DOM writes use `textContent` or `createElement` only, never `innerHTML`.
- App id is `vault-todo`. Keys are `todo/<32 hex>` with JSON values `{"text":string≤500 chars,"done":bool,"created":number}`. Commits are debounced by 500 ms.
- The "copy pairing text" button exists only in `--dev` builds and carries a visible warning.
- Gates: `cargo fmt --check`, `clippy --workspace --all-targets -D warnings`, `cargo test --workspace --locked`, `cargo build -p vault-web --target wasm32-unknown-unknown` (CI), `npm ci && npm test` in `vault-web/js` and `examples/todo`.
- Never force-push, never amend. Commit trailers:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019sALvTRSTdS4PZJbSXGRqj`.

## Review Focus

- **Data call before Active, or after Closed.** It must reject, with nothing sent. Test: `data_calls_rejected_until_active`.
- **Disconnect with edits pending in the debounce window.** Edits are dropped, the list is cleared, and they don't reappear after re-pair. Test: `pending_edits_dropped_on_close`.
- **Hostile to-do text** (HTML or script-like, 10 000 characters). It is rendered as text and capped at 500 characters on input. Test: `hostile_text_rendered_as_text`.
- **Malformed stored value** (a non-JSON value or wrong types under `todo/`). It is skipped with a console warning, and the app doesn't crash. Test: `malformed_value_skipped`.
- **Wrong app.** A vault initialised for another app id refuses, and the UI shows the wrong-app notice. Test: `wrong_app_refused_e2e`.

---

### Task 1: `vault-web` crate and TS wrapper

**Files:** Create `vault-web/{Cargo.toml,src/lib.rs}`, `vault-web/js/{package.json,package-lock.json,tsconfig.json,index.ts,build.mjs}`, `vault-web/js/test/e2e.test.mjs`, `vault-web/js/test/helpers.mjs` (spawns the binaries). Modify the workspace `Cargo.toml` and `.github/workflows/ci.yml`.

**Interfaces:**
- Rust exports, via `#[wasm_bindgen]`:
  - `pair(opts: JsValue) -> Result<Session, JsError>`;
  - `Session::{confirm_sas, cancel, lock, get, put, delete, list, commit, state}`, async methods returning Promises.
- `onOffer({ text, qr: { size, modules: Uint8Array } })`, `onSas(sas: string)`, `onState(state: "pairing"|"awaitingApproval"|"active"|"closed", reason?: string)`. Close reasons use vault-client's names in camelCase (`wrongApp`, `denied`, ...).
- TS: `export async function pair(opts: PairOptions): Promise<Session>`, with `Session` typed as above. `list(prefix)` returns `Array<{ key: string, value: Uint8Array }>`.

- [ ] **Step 1:** Write the e2e tests first in Node 22 (`node --test`). Helpers build or locate `vault-relay`, `vault-dev` and `vaultctl` (`cargo build --locked`), init a temp vault with `--app vault-todo --origin http://localhost:8080` (`--passphrase-file`), and start the relay on a free port. The client gets `pair` with an explicit test origin, `onOffer` hands the text to `vault-dev serve --pair`, stdin answers `y 1`, then `confirmSas`.
  Tests: `pair_put_commit_reconnect_get`, `data_calls_rejected_until_active`, `cancel_before_active_closes_denied`, `wrong_app_refused_e2e`, `lock_then_data_call_rejected`, `qr_matrix_matches_text` (decode check: size consistent with the text, plus a known QR library vector computed in Rust tests). Run: they fail.
- [ ] **Step 2:** Implement the crate. Transport: vault-client `wasm` feature (web-sys WebSocket) in browsers. Node 22's global WebSocket satisfies web-sys only if globals line up; if not, the crate offers a JS-supplied transport (`WebSocket` constructor passed in options). Pick whichever works and document it. QR: a Rust `qrcode` crate (pinned, MIT/Apache).
- [ ] **Step 3:** CI job `vault-web`: install the pinned wasm-bindgen-cli, build the wasm (`--target nodejs`), and run the e2e on ubuntu.
- [ ] **Step 4:** Commit.

### Task 2: `examples/todo`

**Files:** Create `examples/todo/{package.json,package-lock.json,tsconfig.json,build.mjs,serve.mjs,index.html,src/main.ts,src/store.ts,src/view.ts,styles.css,test/store.test.mjs,test/view.test.mjs,README.md}`.

**Interfaces:**
- `store.ts` is pure:
  - `type Todo = { id, text, done, created }`;
  - `parseEntries(entries) -> { todos: Todo[], skipped: number }`;
  - `applyEdit(state, edit) -> { state, writes: Array<{op:"put"|"delete", key, value?}> }`;
  - `class Debouncer(ms, flush)`.
- `view.ts` renders with `createElement` and `textContent` only.

- [ ] **Step 1:** Unit tests first:
  - `malformed_value_skipped`, `text_capped_at_500`, `sorted_by_created`, `debounce_coalesces_edits`, `pending_edits_dropped_on_close` (store and debouncer level);
  - `hostile_text_rendered_as_text` (view test with a tiny DOM shim, or by checking that the generated node tree has no `innerHTML` use: grep test plus a behaviour test).
  Run: they fail.
- [ ] **Step 2:** Implement the store, view and main.
  - main wires `pair` to the screens; the dev build adds the copy button.
  - `serve.mjs` serves the dist directory on 127.0.0.1:8080 with the CSP header from Global Constraints plus `X-Content-Type-Options: nosniff` and the correct wasm MIME type.
  - `build.mjs` takes `--relay <origin>` and `--dev`.
- [ ] **Step 3:** Extend the Task 1 e2e, or add `examples/todo/test/e2e.test.mjs`. It drives the store plus a real session: add 3 to-dos, tick one, delete one, wait for the debounce, lock, re-pair, and check that 2 remain with the correct `done` state.
- [ ] **Step 4:** README with the 4-command local run from the spec and the traffic note. Add a "traffic and commit batching" section to `docs/integration.md`. Commit.

### Task 3: CI and docs polish

- [ ] Add a CI job `todo-example` (`npm ci`, build, unit tests, e2e), plus a grep check that `innerHTML` appears in neither the vault-web JS nor the example. Link the example from the README. Commit.
