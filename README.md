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

Docs: `docs/vault/README.md` (container and CLI), `docs/protocol/vault.md` (wire
protocol), `docs/design/vault.md` (design), `docs/MOVED_TESTS.md` (where each test came from).

```sh
cargo test --workspace
cargo build -p vault-container --release --bin vaultctl
```

## Status

The label strings, the QR prefix `signal-web-pair:v1:` and the recovery prefix
`signal-web-recovery:v1:` are still the ones inherited from the original code and are
scheduled for rename.

## Origin

Copied from signal-web commit `4324d53` (branch `vault-design`) without behaviour changes,
then relicensed from AGPL-3.0-only to MIT by the copyright holder.

## License

MIT, see `LICENSE`.
