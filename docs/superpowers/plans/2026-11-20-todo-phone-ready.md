# To-do sample: phone-ready (LAN + deployable image) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** The to-do example can pair with a vault on a phone: (1) locally on the home network with a generated CA, (2) as a deployable OCI image behind a TLS reverse proxy.

**Spec (owner-approved 2026-11-20, in chat):**
- LAN mode: `showcase.sh --lan` uses the laptop's LAN IP and creates a local CA once in `.showcase/ca/` (0700). It issues a certificate for that IP, serves the page at `https://<ip>:8443`, and has vault-relay terminate `wss://` itself (new optional `--tls-cert/--tls-key`). The script prints the trust steps and CA fingerprint. The page is the QR-only release build, except that `--lan --dev` keeps the copy button. `vault-dev` gains `--ca-file`.
- Image `vault-todo-demo`:
  - multi-arch (amd64, arm64, arm/v7), containing the release page and vault-relay;
  - env `PUBLIC_ORIGIN`, plus `RELAY_ORIGIN` defaulting to the same host, relay under `/mailbox`;
  - TLS by reverse proxy; Traefik (labels + Let's Encrypt) and Caddy examples;
  - CI smoke: CSP header, `/healthz`, foreign Origin refused.
- Tests: e2e in LAN mode with the generated CA, and against the image, with vault-dev as the phone stand-in.

## Global Constraints

- Private keys (CA, leaf) are created with umask 077, never logged or committed, and `.showcase/` stays gitignored. The CA has `pathlen:0` and name constraints for the LAN IP and `localhost` where `openssl` supports them, with a validity ≤ 825 days for the leaf and ≤ 3650 days for the CA. `--lan` refuses to reuse a CA whose key file permissions are wider than 0600.
- vault-relay TLS: rustls (ring) only, TLS 1.2+ (prefer 1.3), no client auth. `--tls-cert` and `--tls-key` must be given together. The key file must not be world-readable (warn on Unix if it is). Without them, behaviour is unchanged.
- The page CSP stays exact as in the to-do plan, with `connect-src` set to the relay's `wss:` origin in LAN and image modes.
- Image: non-root user, read-only root filesystem compatible, no shell needed at runtime, the same SHA-pinned build conventions as `Dockerfile.relay`, hashed pip installs.
- SHA-pinned actions, `persist-credentials: false`. Commit trailers: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019sALvTRSTdS4PZJbSXGRqj`. Never amend; the controller pushes.

## Review Focus

- **CA key leakage.** Key files are 0600, inside 0700 directories, and never printed. Test `lan_ca_key_private`.
- **Wrong certificate or hostname.** A leaf issued for another IP must fail the TLS handshake for vault-dev and Node. Test `lan_wrong_ip_cert_refused`.
- **Image reached through the wrong host.** An Origin that isn't `PUBLIC_ORIGIN` must get 403 on `/mailbox`. Test: the `image_smoke_foreign_origin` CI step.
- **Mixed content.** An https page with a `ws:` relay must refuse to build. Test `build_refuses_ws_relay_for_https_page`.

---

### Task 1: vault-relay TLS and vault-dev `--ca-file`
- [ ] Tests first: `relay_tls_serves_wss` (rustls client with a test CA connects to `/mailbox` and `/healthz` over TLS), `relay_tls_flags_must_pair`, `relay_tls_bad_key_refused`, `vault_dev_ca_file_trusts_local_ca`, `vault_dev_without_ca_file_rejects_local_ca`. Test certs are generated in-test with `rcgen` (dev-dependency, pinned), never checked in.
- [ ] Implement: vault-mailbox `serve` accepts an optional `rustls::ServerConfig`. Flags `--tls-cert/--tls-key` (PEM; chain allowed). vault-dev `--ca-file <pem>` goes to `tls_config(extra_ca_pem)`. Docs in `docs/vault/README.md` and `docs/integration.md` ("Running the relay").

### Task 2: `showcase.sh --lan`
- [ ] Tests first (stub-based, extending `scripts/test/showcase_test.sh`): `lan_ca_created_once_and_reused`, `lan_ca_key_private`, `lan_ip_detection_and_override` (`--lan-ip`), `lan_refuses_wide_key_perms`, `lan_prints_fingerprint_and_trust_steps`, `build_refuses_ws_relay_for_https_page` (in `examples/todo` `build.mjs`).
- [ ] Implement: CA and leaf via the `openssl` CLI (preflight checks for it, LibreSSL-compatible flags); `serve.mjs` gains HTTPS (`--tls-cert/--tls-key`); `build.mjs` accepts `wss://`, and refuses `ws:` with `https` and `wss:` with `http` (the localhost dev path is unchanged); the vault init origin is `https://<ip>:8443`; vault-dev runs with `--ca-file`.
- [ ] e2e: `lan_mode_e2e` in `examples/todo/test` runs the real relay with TLS, the HTTPS page server and vault-dev with `--ca-file` (Node trusts the CA via `NODE_EXTRA_CA_CERTS`): pair, add a to-do, re-pair, persisted. Also `lan_wrong_ip_cert_refused`. CI runs it in the `todo-example` job.
- [ ] Docs: a getting-started "Test with your phone (LAN)" section covering the Android CA install (Settings → Security → Encryption & credentials → Install a certificate → CA certificate), the laptop browser trust per OS, the fingerprint check, and the fact that the Android vault app trusts user CAs only in debug builds.

### Task 3: `vault-todo-demo` image
- [ ] `Dockerfile.todo-demo`: build stage for vault-relay (zigbuild, like `Dockerfile.relay`) and the page (Node, pinned, `npm ci`). The runtime is a scratch or distroless static image with vault-relay plus the page files, and vault-relay serves the static files too (new optional `--static-dir`, read-only, no directory listing, exact CSP header computed from `PUBLIC_ORIGIN`/`RELAY_ORIGIN`). The page's `connect-src` is set at container start: the page build reads the relay origin from a `config.json` served by vault-relay, which the CSP allows via `connect-src 'self'`. Document the CSP change in the plan's spirit, and keep it otherwise exact.
- [ ] Tests: `static_dir_refuses_traversal`, `static_dir_serves_csp`, and `/mailbox` on the same port. CI job `todo-demo-image`: multi-arch build, plus a smoke run with `PUBLIC_ORIGIN=https://todo.example` (CSP header present, `/healthz` 200, `/mailbox` upgrade with Origin `https://evil.example` → 403, with `https://todo.example` → 101).
- [ ] Docs: `docs/deploy-todo-demo.md` with a Traefik `docker compose` example (labels, Let's Encrypt resolver, websocket passthrough) and a Caddy example, plus the `vaultctl init --app vault-todo --origin https://todo.example` step for the phone vault.
