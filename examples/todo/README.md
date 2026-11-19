# Vault to-do (example)

A minimal to-do web app whose only storage is the user's vault. It is plain
TypeScript bundled with esbuild, and it uses [`vault-web`](../../vault-web/js) to
pair with the vault through a relay. Nothing is kept in the browser: when the
session ends, the list is gone from the page.

It follows `docs/integration.md`:

- It shows the QR code (drawn from the matrix vault-web computes), then the code
  with **Codes match** and **Cancel**.
- It makes no data call before `active`. The first call is `list("todo/")`.
- Changes are committed after 500 ms of quiet.
- On `closed` it drops edits still waiting to be committed, clears the list, and
  shows why the session ended.

## Data

| Key | Value |
|---|---|
| `todo/<32 hex>` (16 random bytes) | UTF-8 JSON `{"text": string (at most 500 characters), "done": bool, "created": ms since epoch}` |

Items are sorted by `created`. Entries that don't match this format are skipped,
with a console warning that names the key. The vault is created for app id
`vault-todo`.

## Run it locally

Build vault-web first (it needs the `wasm32-unknown-unknown` target and
`wasm-bindgen-cli` at the version in `Cargo.lock`; see `vault-web/js/build.mjs`):

```sh
(cd vault-web/js && npm ci && npm run build && npm run build:wasm)
cargo build --release --bin vault-relay --bin vault-dev --bin vaultctl
(cd examples/todo && npm ci)
```

The binaries are then in `target/release/`; run the commands below from the
repository root, or install prebuilt signed binaries instead (`docs/install.md`)
and drop the `target/release/` prefix. Then:

1. `npm --prefix examples/todo run serve` (in its own terminal) builds a development build (relay
   `http://127.0.0.1:8787`) and serves it at `http://localhost:8080`.
2. `target/release/vault-relay --listen 127.0.0.1:8787 --allow-origin http://localhost:8080`
3. `target/release/vaultctl --dir <d> init --app vault-todo --origin http://localhost:8080`
4. Open `http://localhost:8080`, click "Connect to vault" and then "Copy pairing
   text". Run `target/release/vault-dev serve --dir <d> --pair "<text>"`, answer `y <n>` if the
   codes match, and click **Codes match**.

Open the page as `localhost`, not `127.0.0.1`. The vault and the relay are bound
to the exact origin.

**The copy button exists only in `--dev` builds.** The pairing text is a secret:
anyone who has it while the QR is shown can ask your vault for a session. A
release build (`node build.mjs --relay https://relay.example`) has no copy button
and no clipboard access; you pair it by scanning the QR with your vault.

## Build and serve

- `node build.mjs --relay <origin> [--dev] [--out dist]`
  - `--relay` must be a bare origin: `https://host[:port]`, or `http` for
    loopback only. It is compiled in.
  - The wasm is bundled into `main.js` as bytes, so the page never fetches it.
- `node serve.mjs [--port 8080] [--dir dist]` serves `dist` on 127.0.0.1 with:
  - `Content-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src <relay WebSocket origin>; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`
  - `connect-src` is the relay's WebSocket origin: `http://127.0.0.1:8787` becomes
    `ws://127.0.0.1:8787`, and `https://relay.example` becomes `wss://relay.example`.
    Browsers check the `ws:`/`wss:` URL itself, so listing the `http(s)` origin
    would block the dial (Chrome does).
  - `X-Content-Type-Options: nosniff`
  - It serves regular files of known types only. There are no directory
    listings, no dot-files, no symlinks, and no paths outside `dist`.
  - Any other host should send the same CSP header. `index.html` also carries it
    as a `<meta>` (filled in by build.mjs) without `frame-ancestors`, because a
    meta policy cannot carry that directive.

All DOM writes use `createElement` and `textContent`, never `innerHTML`.
`test/sinks.test.mjs` and a CI grep enforce this.

## Traffic

- An edit is a few hundred bytes.
- The relay's keepalive costs about 15 to 20 KB per hour while a session is
  open.
- Each commit makes the vault write a segment of at least 64 KiB, which is
  uploaded to every replica.
- That is why commits are debounced: a burst of edits within 500 ms becomes one
  commit.

See `docs/integration.md`, "Traffic and commit batching".

## Tests

- `npm run test:unit` covers the store, the view (on a small DOM shim), the app
  wiring with a fake session, the server, and the sink check. It runs anywhere
  with Node 22.
- `npm run test:build` builds release and dev into temp dirs and checks the
  output. It needs vault-web built.
- `npm run test:e2e` runs the app's logic with the real vault-web wasm against
  real `vault-relay`, `vault-dev` and `vaultctl`. It covers persistence across
  sessions, edits dropped on close, wrong app, and cancel. It needs vault-web
  built, and uses `VAULT_BIN_DIR` or builds the binaries with cargo.
- `npm run test:browser` is a smoke test in headless Chrome. It serves the
  built page with `serve.mjs`, starts the real relay, clicks "Connect to vault",
  and fails on any CSP violation or console error. It also fails unless the relay
  saw the page's WebSocket upgrade on `/mailbox`. It needs Chrome (`CHROME_BIN`,
  or `google-chrome`/`chrome`/`chromium` on PATH), vault-web built, and the relay
  binary.
- `npm test` runs all of them. CI runs them in the `todo-example` job.
