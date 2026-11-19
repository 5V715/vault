# vault

An encrypted, replicated container for an app's persistent user data, plus the
protocol and server that let a stateless browser app use it through a relay mailbox.

Crates:

- `vault-container`: the encrypted container (segments, sealed manifest, recovery key),
  local/S3/WebDAV backends, replication, and the `vaultctl` CLI.
- `vault-proto`: wire messages, QR pair offer, chunking and the Noise handshake helpers
  (compiles for `wasm32-unknown-unknown`).
- `vault-server`: the vault side of the protocol (mailbox dial, Noise responder,
  approval, request serving) and the `vault-dev` laptop test vault.
- `vault-mailbox`: the relay mailbox (`Mailboxes::handle` pairs two WebSocket clients
  and pipes Noise frames) and the `vault-relay` binary; `Dockerfile.relay` builds a
  scratch image of it.
- `vault-client`: the app/browser side (pairing session with the two-sided code check,
  Noise request channel, RAM write-back `RemoteBackend`); feature `wasm` for the browser
  WebSocket, `native` for a tokio-tungstenite client. Compiles for `wasm32-unknown-unknown`.

Docs: `docs/integration.md` (how an app pairs with a vault and what its UI must do),
`docs/protocol/vault.md` (wire protocol), `docs/vault/README.md` (container and CLI), `docs/design/vault.md` (design), `docs/MOVED_TESTS.md` (where each test came from).

```sh
cargo test --workspace
cargo build -p vault-container --release --bin vaultctl
# a container is bound to one app and web origin when it is created:
vaultctl --dir ~/vault init --app signal-web --origin https://app.example
vaultctl --dir ~/vault info
```

A vault serves one app at one origin: the browser declares both in the pair offer, the
Noise prologue binds them, and the vault refuses any other with `WrongApp` before it asks
the user. The origin is a declared, pinned label, not an authenticated claim
(`docs/protocol/vault.md`, "App binding" and "Threat notes").

## Versions

Protocol version 1 and container format version 1, with neutral labels: QR prefix
`vault-pair:v1:`, recovery prefix `vault-recovery:v1:`, `vault/v1/...` for every key
derivation and the Noise prologue (`docs/protocol/vault.md`, "Versions and labels").
Known-answer values are recomputed independently by `tools/kat/kat.py`.

## Origin

Copied from signal-web commit `4324d53` (branch `vault-design`) without behaviour changes,
then relicensed from AGPL-3.0-only to MIT by the copyright holder.

## License

MIT, see `LICENSE`.
