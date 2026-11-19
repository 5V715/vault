# Tooling releases Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Signed multi-platform releases of `vaultctl`, `vault-dev` and `vault-relay`, plus verifying install scripts for Linux, macOS and Windows.

**Architecture:** A tag-triggered GitHub workflow builds the five targets and creates a draft release with the archives and `SHA256SUMS`. The owner signs `SHA256SUMS` offline with an OpenSSH key and publishes. The install scripts download with `gh` and verify the signature, then the hash, before installing.

**Tech Stack:** GitHub Actions, cargo-zigbuild, `ssh-keygen -Y`, bash, PowerShell 5.1+/7.

**Spec:** `docs/superpowers/specs/2026-11-19-tooling-releases-design.md`

## Global Constraints

- Targets exactly: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`.
- Archive name `vault-<version>-<target>.tar.gz` (Windows `.zip`), where `<version>` is the tag without the leading `v`. The archive contains `vaultctl`, `vault-dev`, `vault-relay` (with `.exe` on Windows), `LICENSE` and `README.md` at its top level. `SHA256SUMS` uses `sha256sum` format (`<hex>  <filename>`).
- Signature: `SHA256SUMS.sig` = `ssh-keygen -Y sign -n vault-release`. Principal `release@vault`. Trust root `keys/release.pub` plus `keys/allowed_signers`; the identical allowed-signers line is embedded in both install scripts. A CI test asserts that all three agree.
- The release private key never enters the repo, CI, secrets or logs. The workflow only creates **drafts**.
- Install scripts: verify the signature first, then the hash; install nothing on any failure; temporary files are always removed; no `curl | sh` path while the repo is private (gh only).
- Never force-push, never amend pushed commits. Commit trailers:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019sALvTRSTdS4PZJbSXGRqj`.

## Review Focus

- **Tampered archive with a valid signature on the real `SHA256SUMS`.** The install must fail on the hash. Test `install_rejects_tampered_archive`.
- **`SHA256SUMS` replaced together with a signature from a different key.** The install must fail on the signature. Test `install_rejects_foreign_signature`.
- **Missing `SHA256SUMS.sig`** (draft-like or half-published release). The install must fail with a clear message. Test `install_rejects_unsigned_release`.
- **Placeholder trust root** (owner key not yet created). The scripts and workflow must refuse. Tests `install_refuses_placeholder_key` and `release_workflow_requires_key`.
- **Windows path handling** (spaces in the profile path, existing PATH entry). The install must be idempotent. Test `install_ps1_idempotent_path` on a Windows runner.

---

### Task 1: Release workflow and Windows build

**Files:**
- Create: `.github/workflows/release.yml`, `keys/release.pub` (placeholder line `PLACEHOLDER — owner must replace`), `keys/allowed_signers` (placeholder), `scripts/package.sh`, `scripts/package.ps1` (or one cross-platform packaging step)
- Modify: `vault-server/src/bin/vault-dev.rs`, `vault-container/src/secrets.rs` (gate `--passphrase-fd` with `cfg(unix)`, including the help text), `.github/workflows/ci.yml` (add a Windows and a macOS `cargo build` + `cargo test` job for the three binaries' crates)

- [ ] **Step 1:** Make the Windows build compile: gate the passphrase-fd code and flag. Test `passphrase_fd_flag_absent_on_windows` (`#[cfg(windows)]`, asserts `--help` lacks it), run in the new Windows CI job.
- [ ] **Step 2:** `release.yml` on `push: tags: ['v*']`:
  - A `check-key` job fails if `keys/release.pub` is the placeholder (`release_workflow_requires_key`).
  - A build matrix over the 5 targets (`--locked --release`, features `cli`/`dev`/`bin`); Linux via cargo-zigbuild on ubuntu, macOS natively on `macos-14` (arm64) and `macos-13` (x86_64), Windows natively.
  - Package per the Global Constraints, then a `publish-draft` job computes `SHA256SUMS` and creates the draft release with `gh release create --draft`.
  - Permissions `contents: write` only in `publish-draft`.
- [ ] **Step 3:** Run `cargo build --locked` for the host and `actionlint` (install it) on the workflows. Expected: clean.
- [ ] **Step 4:** Commit `release: multi-platform draft releases; vault-dev/vaultctl build on Windows`.

### Task 2: Signing and install scripts

**Files:**
- Create: `scripts/sign-release.sh`, `scripts/install.sh`, `scripts/install.ps1`, `scripts/test/install.bats` or `scripts/test/install_test.sh` (plain bash test runner), `scripts/test/install.Tests.ps1` (Pester), `docs/install.md`
- Modify: `.github/workflows/ci.yml` (shellcheck; bash install tests on ubuntu and macOS; Pester tests on Windows; the trust-root agreement test), `README.md`, `docs/vault/README.md`

- [ ] **Step 1:** Write the tests first. They run against a **local fake release**: a test-only throwaway OpenSSH key, archives and sums generated in the test, and a `gh` shim on PATH that serves files from a directory and fails `gh auth status` on request. The scripts take a test-only override for the embedded allowed-signers (`VAULT_INSTALL_TEST_ALLOWED_SIGNERS`), honoured only when `VAULT_INSTALL_TEST=1`; a test asserts that the override is ignored otherwise.
  Tests: `install_happy_path`, `install_rejects_tampered_archive`, `install_rejects_foreign_signature`, `install_rejects_unsigned_release`, `install_refuses_placeholder_key`, `install_requires_gh_login`, `install_pins_version`, `install_cleans_up_on_failure`, `test_override_ignored_without_test_mode`, `install_ps1_idempotent_path`, `trust_root_files_agree`. Run: they fail.
- [ ] **Step 2:** Implement `install.sh`, POSIX-ish bash. It checks for `gh`, `ssh-keygen`, `tar` and `sha256sum`/`shasum`, detects the target from `uname -s/-m`, then downloads, verifies, extracts and installs with mode 0755.
- [ ] **Step 3:** Implement `install.ps1` (PowerShell 5.1 compatible). `Get-FileHash` for the hash, `ssh-keygen.exe -Y verify` for the signature, `Expand-Archive`, and a user-PATH update without duplicates.
- [ ] **Step 4:** `sign-release.sh <tag> <private-key-path>`:
  1. `gh release download <tag> -p SHA256SUMS`.
  2. Print the file and ask for confirmation.
  3. `ssh-keygen -Y sign -f <key> -n vault-release SHA256SUMS`.
  4. Verify the result against `keys/allowed_signers`.
  5. `gh release upload <tag> SHA256SUMS.sig`, then `gh release edit <tag> --draft=false`.
  It refuses if the release is not a draft or is already signed.
- [ ] **Step 5:** `docs/install.md`:
  - Owner one-time key setup: `ssh-keygen -t ed25519 -C "release@vault" -f vault-release`, with a passphrase, stored offline. Commit `keys/release.pub` and `keys/allowed_signers` (`release@vault namespaces="vault-release" <pubkey>`) and update the embedded line in both scripts; the agreement test enforces it.
  - The release steps.
  - The user install commands: `gh api repos/5V715/vault/contents/scripts/install.sh --jq .content | base64 -d > install.sh`, inspect, then run. Same for PowerShell.
  - The honest limit from the spec.
- [ ] **Step 6:** Run all tests locally (bash; Pester if pwsh is available, else CI). Run shellcheck. Commit `release: offline-signed SHA256SUMS; verifying install scripts`.
