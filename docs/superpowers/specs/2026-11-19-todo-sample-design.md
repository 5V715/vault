# Sample to-do web app and `vault-web` — short design

Status: approved by owner 2026-11-19.

## Intent

Show that the vault works for an app other than signal-web: a minimal to-do web
app whose only persistence is the vault. Building it also yields the first
browser package for third-party apps, which the split design deferred until a
second app existed.

## Decisions

- **`vault-web`** is a new crate, `vault-web/`, with `wasm-bindgen` exports
  over `vault-client`, plus a typed TypeScript wrapper in
  `vault-web/js/index.ts`. Build it with a pinned `wasm-bindgen-cli`, version
  equal to the `wasm-bindgen` crate in `Cargo.lock`, as `--target web` for
  browsers and `--target nodejs` for tests.
- **API:**
  `pair({ relayOrigin, appId, deviceName, onOffer, onSas, onState }) → Session`.
  A `Session` has `confirmSas()`, `cancel()`, `lock()`, `get(key)`,
  `put(key, bytes)`, `delete(key)`, `list(prefix)` and `commit()`. Keys are
  strings and values are `Uint8Array`.
  - The origin is taken from `globalThis.location.origin` in browsers. Node
    tests pass it explicitly, through a test-only option documented as such.
  - `onOffer` receives the pairing text and a QR module matrix generated in
    Rust, so no JS QR library is needed.
  - Data methods reject before the session is Active.
- **`examples/todo/`** is plain TypeScript, bundled with esbuild (the only npm
  dependency, pinned with a lockfile).
  - The page has a strict CSP (`default-src 'none'`, scripts from self plus
    `'wasm-unsafe-eval'`, `connect-src` set to the relay origin) and uses
    `textContent` only.
  - Screens: connect (QR, plus a "copy pairing text" button only when built
    with `--dev`, with a warning), compare code ("Codes match" / "Cancel"), the
    list (add, tick, delete, with a "saving… / saved" indicator), and
    disconnected.
  - Data: key `todo/<id>`, where the id is 16 random bytes as hex, and the
    value is JSON `{ "text", "done", "created" }`. Items are sorted by
    `created`.
  - Changes are committed after 500 ms of quiet; `Closed` discards pending
    edits and clears the list.
- **Local run:**
  1. Serve the static build: `npm run serve` (a tiny static server in
     the example, port 8080).
  2. Start the relay: `vault-relay --listen 127.0.0.1:8787 --allow-origin http://localhost:8080`.
  3. Create the vault: `vaultctl init --app vault-todo --origin http://localhost:8080`.
  4. Pair it: `vault-dev serve --dir <d> --pair "<text>"`.
- **Tests:** a Node 22 end-to-end test, which has a global WebSocket. It starts
  the real `vault-relay` and `vault-dev` binaries, pairs (answering `y 1` on
  `vault-dev`'s stdin and confirming the SAS in the client), adds, ticks and
  deletes to-dos, reconnects and checks they persisted. It also covers the
  wrong-app and cancel cases. The todo app's state logic is a pure module with
  unit tests. CI builds `vault-web` for wasm and runs both test suites.
- **Traffic:** edits are a few hundred bytes. The relay-side ping costs about
  15–20 KB per hour. Each commit uploads a segment of at least 64 KiB to
  replicas, which is why commits are debounced. `docs/integration.md` gets a
  "traffic and commit batching" note.

## Non-goals

A polished UI, multi-device sync semantics beyond what the vault does, offline
mode, and publishing to npm.
