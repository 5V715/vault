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
| `--pair TEXT` | pair once with this pairing text instead of asking (it ends up in your shell history; prefer pasting at the prompt) |
| `--check` | build, start, health-check, exit 0 (used by CI) |
| `--lan` [`--lan-ip IP`] [`--dev`] | serve on your LAN IP over https/wss for a phone: see [Test with your phone (LAN)](#test-with-your-phone-lan) |

## What you will see

1. **The terminal** builds, creates the demo vault, starts the relay and the page
   server, and prints a banner with the URL and the steps below.
2. **Open `http://localhost:8080`** (say `localhost`, not `127.0.0.1`). The page shows
   a **Connect to vault** button.
3. **Click Connect to vault.** The page shows a QR code and a **Copy pairing text**
   button. The QR is what a phone vault would scan; on your laptop you copy the text.
4. **Click Copy pairing text** and paste it at the terminal's `Pairing text:` prompt.
   The text is valid for about 60 seconds.
5. **`vault-dev` asks for approval** in the terminal. It prints the app, origin and
   relay it was asked about, a 6-digit code, and a prompt number such as `[#1]`. Compare
   the 6-digit code with the one in the browser. If they are the same, answer
   `y <prompt number>` (for `[#1]`: `y 1`) and press Enter. If they differ, answer `n`:
   someone may be in the middle.
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

## Test with your phone (LAN)

```sh
scripts/showcase.sh --lan
```

serves the page at `https://<LAN IP>:8443` and the relay at `wss://<LAN IP>:8787` so
that the Android vault app ([`docs/android.md`](android.md)) on a phone in the same home
network can pair with the page. It has to be a **debug build** of the app, which can
import the local CA. `--lan --dev` below pairs the page with a demo vault on the
computer instead, over the same TLS setup.

- **The LAN IP** is your computer's primary IPv4 address (Linux: the source address of
  `ip route get 1.1.1.1`; macOS: `ipconfig getifaddr` of the default route's
  interface). If that route goes through a VPN or tunnel (`tun*`, `wg*`, `utun*`,
  `tailscale*`, ...), the home-network address of another interface is used instead.
  Only home-network addresses are accepted (10/8, 172.16/12, 192.168/16); give any
  other one, such as a 100.64/10 address, explicitly with `--lan-ip 192.168.1.20`.
- **Give the computer a fixed address.** Set up a DHCP reservation for it in your
  router. The CA and the vault binding are both tied to the IP, so a new IP means a
  new CA to install on the phone and a new vault binding.
- **A local CA** is created once in `.showcase/ca/` (mode 0700; keys 0600, never
  printed). It is an ECDSA P-256 CA valid for 10 years with `pathlen:0`, and its name
  constraints let it vouch only for your IP (`<ip>/32`) and `localhost`: even if its
  key leaked, it could not be used to impersonate any other host on your network to
  the devices that trust it. The server certificate (825 days, for your IP and
  `localhost`) is issued from it. When your IP changes, the script makes a **new CA**
  and says so loudly: install the new one on your phone and browsers and remove the
  old one. If your `openssl` cannot add name constraints the script says so (with
  openssl's error) and makes the CA without them. A key file or CA
  directory that other users can read is refused. `--reset` keeps the CA; delete
  `.showcase/ca/` yourself to make a new one.
- **The page** is the release build: it only shows the QR code, which the vault app on
  your phone scans. `--lan --dev` keeps the **Copy pairing text** button and pairs with
  a demo vault on the computer instead (vault-dev, which trusts the CA with
  `--ca-file`).
- **Both ports listen on the LAN IP only** (in localhost mode on 127.0.0.1 only), not
  on a VPN or any other interface. If a host firewall blocks them, open them for your home network
  only (for example `sudo ufw allow from 192.168.1.0/24 to any port 8443,8787 proto tcp`),
  never forward them from your router, and stop the script with Ctrl-C when you are done.

### Trust the CA

The script prints the path of `ca.pem` and its **SHA-256 fingerprint**. Every device
that opens the page or runs the vault must trust the CA once. Compare the fingerprint
your device shows with the printed one before you trust it, and remove the CA again
when you no longer need it.

- **Android phone, the vault app:** the app's TLS (Rust) does not use Android's
  certificate store. In a **debug build**, copy `ca.pem` to the phone and import it in
  the app (vault list → **Debug build: extra CA for the LAN showcase** → **Import a CA
  certificate (PEM)**), then restart the app. A release build has no such setting and
  does not connect to the LAN relay. See [`docs/android.md`](android.md), "On your LAN".
- **Android phone, its browser** (only if you open the page on the phone): copy
  `ca.pem` to the phone, then **Settings → Security → Encryption & credentials →
  Install a certificate → CA certificate** (the path varies a little by vendor; search
  Settings for "CA certificate"). Android shows a warning; the installed CA is listed
  under **Trusted credentials → User**, where its details show the fingerprint.
- **Linux browser:** Chrome/Chromium: **Settings → Privacy and security → Security →
  Manage certificates** and import `ca.pem` as a trusted authority (the menu differs
  between Chrome versions), or `certutil -d sql:$HOME/.pki/nssdb -A -t "C,," -n "vault
  showcase CA" -i .showcase/ca/ca.pem`). Firefox: **Settings → Privacy & Security →
  Certificates → View Certificates → Authorities → Import**, tick "identify websites".
- **macOS browser:** Safari and Chrome use the keychain:
  `security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db .showcase/ca/ca.pem`
  (or open `ca.pem` in Keychain Access and set it to "Always Trust"). Firefox: as on
  Linux.
- **Windows browser** (if you open the page from another computer): Edge and Chrome
  use the Windows store: `certutil -user -addstore Root ca.pem`, or `certmgr.msc` →
  Trusted Root Certification Authorities → Import. Firefox: as on Linux.

### Pair the phone

1. On the phone, create (or reset) the vault for this page in the vault app: it must be
   bound to the
   app `vault-todo` and the origin the script prints, the same as
   `vaultctl init --app vault-todo --origin https://<LAN IP>:8443`.
2. Open `https://<LAN IP>:8443` in a browser that trusts the CA (on the computer or
   another device in the network), click **Connect to vault**, and scan the QR code
   with the vault app.
3. Compare the 6-digit codes on both screens, approve in the app, then click **Codes
   match** in the page.

The vault is bound to the exact origin, including the IP: if your IP changes, the page
moves to the new address, the phone needs the new CA, and the phone's vault has to be
created for the new origin. A DHCP reservation avoids all of that.

## Deploy it on a public host

To pair a phone from anywhere, run the page and its relay as the `vault-todo-demo`
image behind a TLS reverse proxy (Traefik or Caddy, with a Let's Encrypt certificate):
see [`docs/deploy-todo-demo.md`](deploy-todo-demo.md). It ends with the same pairing
steps: `vaultctl init --app vault-todo --origin https://todo.example`, open the page,
scan the QR.

## Where the data lives

Everything is under `.showcase/` in the repository (git-ignored), or `--dir`:

| Path | What |
|---|---|
| `.showcase-dir` | marker: the script only resets or reuses a directory that has it |
| `vault/` | the encrypted vault container (segments and a sealed manifest) |
| `passphrase` | random passphrase, mode 0600, never printed or passed on a command line |
| `origin` | the origin the vault is bound to |
| `web/` | the built page |
| `ca/` | with `--lan`: the local CA (`ca.pem`, `ca.key`) and the certificate for its IP (`leaf-<ip>.pem`, `.key`); kept by `--reset` |
| `logs/relay.log`, `logs/web.log` | what the background processes wrote |

The items you add are encrypted inside `vault/`. `vaultctl init` creates a recovery key
and normally prints it once; the script discards that output, so the demo vault's
recovery key is created and sealed in `vault/.vaultctl` but never shown. Treat the demo
vault as throwaway: it cannot be restored from a replica. Do not put real data in it.

## Reset

```sh
scripts/showcase.sh --reset
```

asks `y/N`, then deletes `vault/`, `passphrase`, `origin`, `web/` and `logs/` in the
demo directory (and nothing else), and starts over with a fresh vault and passphrase.
The script only touches a directory it created itself (one holding `.showcase-dir`),
and refuses `/`, your home directory, the repository, symlinks and any other existing
directory, so `--dir` cannot point it at your own files.
Use it after changing `--web-port`: a vault is bound to one origin.

## Troubleshooting

- **A port is busy** (the script says "already in use" before it builds anything): pick other ports with
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
- **`--lan`: "is not a home-network address" / "cannot detect":** the computer is not
  on a home network, or the detected interface is a VPN; pass the right address with
  `--lan-ip`.
- **`--lan`: the browser or the phone says the connection is not private:** the CA is
  not trusted on that device yet (see [Trust the CA](#trust-the-ca)), or the page is
  opened under another address than the one the script printed.
- **`--lan`: "refusing to reuse it" for a key file:** its permissions were widened.
  `chmod 600` it if nobody else could have copied it; otherwise delete `.showcase/ca/`,
  and install the new CA in place of the old one.

The page is a `--dev` build, which has the **Copy pairing text** button. The pairing
text is a secret while the QR is shown, so use this build only on localhost (or your
own home network with `--lan --dev`), for demos.

## Next steps

- **Install signed tooling** instead of building it: [`docs/install.md`](install.md).
- **Use the vault in your own app:** [`docs/integration.md`](integration.md) (what the
  UI must do, the pairing flow, running a relay), with the to-do example in
  [`examples/todo`](../examples/todo/README.md) as a working reference.
- **Replicate a vault to WebDAV or S3** with `vaultctl`. Do not use the demo vault for
  this: its recovery key was never shown, so a replica could not be restored. Create
  your own vault with `vaultctl init --app <id> --origin <url>` (it shows the recovery
  key once: write it down), then add a replica and replicate:

  ```sh
  vaultctl --dir ~/my-vault replica add webdav --id nextcloud \
    --url https://cloud.example.com/remote.php/dav/files/alice/vault --username alice
  vaultctl --dir ~/my-vault replicate
  ```

  S3, restore and divergence are covered in [`docs/vault/README.md`](vault/README.md).
