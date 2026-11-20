# Getting started

See the vault working in a few minutes: a browser to-do app whose only storage is an
encrypted vault on your own machine. One command builds and starts everything; you
pair the browser with the vault by pasting one line.

```sh
scripts/showcase.sh
```

This is a local demo. Nothing leaves your machine, and the demo vault is a throwaway.

## Prerequisites

The script checks these first and prints the exact fix for anything missing. It never
installs anything unless you pass `--install-deps`.

| Need | Why | Linux | macOS |
|---|---|---|---|
| Rust (via rustup) | builds `vault-relay`, `vault-dev`, `vaultctl` and the browser wasm | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` | same |
| `wasm32-unknown-unknown` target | vault-web compiles to wasm | `rustup target add wasm32-unknown-unknown` | same |
| `wasm-bindgen` CLI, **exactly** the version in `Cargo.lock` | generates the browser glue | `cargo install --locked wasm-bindgen-cli --version <v>` (the script prints `<v>`) | same |
| Node.js 22 or newer, with npm | bundles vault-web and the page | your package manager, [nodejs.org](https://nodejs.org/) or `nvm install 22` | `brew install node@22` or `nvm install 22` |
| `curl`, a C toolchain | health checks; building Rust crates | `apt install curl build-essential` (or your distro's equivalent) | `xcode-select --install` |

`scripts/showcase.sh --install-deps` installs the wasm32 target and `wasm-bindgen-cli`
for you (the two Rust pieces). Rust and Node you install yourself.

**Windows:** run the script inside WSL 2 (Ubuntu) and open the page from the Windows
browser at `http://localhost:8080` (WSL forwards localhost). Or follow the manual steps in
[`examples/todo/README.md`](../examples/todo/README.md#run-it-locally).

## Run the showcase

```sh
scripts/showcase.sh
```

The first run compiles Rust in release mode, so allow a few minutes. Later runs reuse
the build caches; `--no-build` skips the Rust and vault-web builds entirely.

Useful options (all in `--help`):

| Option | Meaning |
|---|---|
| `--web-port N` / `--relay-port N` | change the ports (default 8080 / 8787) |
| `--dir D` | keep the demo files in `D` instead of `.showcase/` |
| `--reset` | delete the demo vault first (asks `y/N`; `--yes` skips the question) |
| `--no-build` | reuse the existing builds |
| `--pair TEXT` | pair once with this pairing text instead of asking |
| `--check` | build, start, health-check, exit 0 (used by CI) |

## What you will see

1. **The terminal** builds, creates the demo vault, starts the relay and the page
   server, and prints a banner with the URL and the steps below.
2. **Open `http://localhost:8080`** (say `localhost`, not `127.0.0.1`). The page shows
   a **Connect to vault** button.
3. **Click Connect to vault.** The page shows a QR code and a **Copy pairing text**
   button. The QR is what a phone vault would scan; on your laptop you copy the text.
4. **Click Copy pairing text** and paste it at the terminal's `Pairing text:` prompt.
   The text is valid for about 60 seconds.
5. **`vault-dev` asks for approval** in the terminal and shows a code, a short number,
   and the page shows a number too. If they are the same, type `y` and the number, for
   example `y 417`, then press Enter. If they differ, answer `n`: someone is in the
   middle.
6. **Click Codes match** in the page. The to-do list appears. It is empty at first.
7. **Add a few items, tick some.** Changes are saved to the vault after 500 ms of
   quiet. Nothing is stored in the browser.
8. **Click Lock** (or stop the session in the terminal). The page clears the list and
   says why the session ended. The terminal asks `Pair again? [Y/n]`. Pair again and
   your items are back: they live in the vault, not in the page.

Stop everything with Ctrl-C. The script stops the relay and the page server too.

## How it works

```
  browser (page + vault-web wasm)              your machine
  ┌──────────────────────────┐        ┌─────────────────────────────┐
  │ to-do page, localhost:8080│        │ vault-dev  ──  .showcase/   │
  └────────────┬─────────────┘        │  (the vault)     vault/     │
               │ WebSocket             └──────────────┬──────────────┘
               │ (ciphertext only)                    │ WebSocket
               ▼                                      ▼
            ┌────────────────────────────────────────────┐
            │ vault-relay  127.0.0.1:8787  (mailbox)      │
            └────────────────────────────────────────────┘
```

- The **page** and **vault-dev** meet in a one-time mailbox on the **relay**. The
  pairing text carries the mailbox and the vault's public key.
- They run a Noise handshake through the relay, then compare the short code on both
  screens. The relay only forwards bytes: it cannot read or change what they say, and
  after pairing it sees ciphertext only. End-to-end encryption is between the page and
  the vault.
- **vault-dev** opens the vault (an encrypted container) and answers the page's
  `list`, `get` and `put` calls. The page itself has no storage.
- The relay only accepts browsers from the page's exact origin
  (`--allow-origin http://localhost:8080`), and the vault is bound to the app id
  `vault-todo` and that origin, so a pairing text for another app is refused.

The design is in `docs/design/vault.md`, the wire protocol in `docs/protocol/vault.md`.

## Where the data lives

Everything is under `.showcase/` in the repository (git-ignored), or `--dir`:

| Path | What |
|---|---|
| `vault/` | the encrypted vault container (segments and a sealed manifest) |
| `passphrase` | random passphrase, mode 0600, never printed or passed on a command line |
| `origin` | the origin the vault is bound to |
| `web/` | the built page |
| `logs/relay.log`, `logs/web.log` | what the background processes wrote |

The items you add are encrypted inside `vault/`. The recovery key that `vaultctl init`
prints once is not kept for the demo vault; if you want a vault you can recover, create
one by hand (see "Next steps").

## Reset

```sh
scripts/showcase.sh --reset
```

asks `y/N`, then deletes `vault/`, `passphrase`, `origin`, `web/` and `logs/` in the
demo directory (and nothing else), and starts over with a fresh vault and passphrase.
Use it after changing `--web-port`: a vault is bound to one origin.

## Troubleshooting

- **A port is busy** (`cannot bind` in the log tail): pick other ports with
  `--web-port` and `--relay-port`, or stop what uses them. Changing `--web-port`
  needs `--reset`, because the vault is bound to the old origin.
- **The page does not connect, or the relay refuses it:** open it as
  `http://localhost:<port>`, never `127.0.0.1`. The vault and the relay are bound to
  the exact origin.
- **`wasm32-unknown-unknown` is missing:** `rustup target add wasm32-unknown-unknown`,
  or run the script with `--install-deps`.
- **wasm-bindgen version mismatch:** it must equal the version in `Cargo.lock`:
  `cargo install --locked wasm-bindgen-cli --version <v>` (the script prints it).
- **"Pairing failed" or the text is rejected:** the pairing text expires after about
  60 seconds. Click **Pair again** in the page and paste the new text.
- **"This vault belongs to a different app or address":** the vault is bound to another
  app id or origin than this page. Use `--reset`, or open the page at the origin the vault
  was created for.
- **`vault-dev` says "wrong passphrase":** the passphrase file and the vault do not
  belong together (a half-deleted `.showcase`). Use `--reset`.
- **The script says a process exited:** read the log tail it prints, or
  `.showcase/logs/relay.log` and `web.log`.

The page is a `--dev` build, which has the **Copy pairing text** button. The pairing
text is a secret while the QR is shown, so use this build only on localhost, for demos.

## Next steps

- **Install signed tooling** instead of building it: [`docs/install.md`](install.md).
- **Use the vault in your own app:** [`docs/integration.md`](integration.md) (what the
  UI must do, the pairing flow, running a relay), with the to-do example in
  [`examples/todo`](../examples/todo/README.md) as a working reference.
- **Replicate the demo vault to WebDAV or S3** with `vaultctl`. From the repository
  root, with the passphrase in the environment (it is read from the file here, not
  typed on the command line):

  ```sh
  export VAULTCTL_PASSPHRASE="$(head -n1 .showcase/passphrase)"
  target/release/vaultctl --dir .showcase/vault replica add webdav --id nextcloud \
    --url https://cloud.example.com/remote.php/dav/files/alice/vault --username alice
  target/release/vaultctl --dir .showcase/vault replicate
  ```

  S3, restore and divergence are covered in [`docs/vault/README.md`](vault/README.md).
