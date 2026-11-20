# Android vault app Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A sideloaded Android 11+ app that holds per-app vault containers, approves browser sessions with a code comparison plus biometric/device credential, serves them over the relay, and replicates to S3/WebDAV in the background.

**Architecture:** A new Rust crate `vault-ffi` wraps vault-container and vault-server through UniFFI. The Android project in `android/` has three modules: `:core` (pure Kotlin/JVM logic), `:ffi` (generated bindings plus .so loading) and `:app` (Compose UI, Keystore, foreground service, CameraX+ZXing, WorkManager).

**Tech Stack:** Rust (cargo-ndk, UniFFI), Kotlin, Jetpack Compose, AndroidX Biometric, CameraX, ZXing core, WorkManager, Gradle with dependency verification.

**Spec:** `docs/superpowers/specs/2026-11-20-android-vault-design.md`, including "Settled details" and both owner-decision sections. Background replication (option B) applies.

## Global Constraints

- minSdk 30; targetSdk and compileSdk at the current stable level. No Google Play Services or Firebase dependency anywhere; check with `./gradlew :app:dependencies`.
- Pins:
  - AGP, Kotlin, Compose BOM, AndroidX libraries, CameraX, ZXing core, the UniFFI crate and bindgen, cargo-ndk and the NDK are pinned exactly.
  - Gradle dependency verification is on (`gradle/verification-metadata.xml` with sha256).
  - The Gradle wrapper has `distributionSha256Sum`.
  - GitHub actions are SHA-pinned with `persist-credentials: false`.
- Secrets:
  - The plaintext CRK, recovery key and replica credentials are never logged, never in Parcels, Intents, SavedState or SharedPreferences.
  - `FLAG_SECURE` is set on the create, recovery-key, approval and restore screens.
  - `android:allowBackup="false"`, data extraction rules exclude everything, and all vault files live in `noBackupFilesDir`.
- Keystore, per vault:
  - **K_auth** (AES-256-GCM): `setUserAuthenticationParameters(0, BIOMETRIC_STRONG | DEVICE_CREDENTIAL)`, `setUnlockedDeviceRequired(true)`, `setInvalidatedByBiometricEnrollment(true)`, StrongBox if `FEATURE_STRONGBOX_KEYSTORE` is available. It wraps the CRK.
  - **K_bg** (AES-256-GCM): `setUnlockedDeviceRequired(true)`, no user auth, StrongBox if available. It seals the high-water file, the manifest key and the replica credentials.
- Vault identity: the exact normalized `(app_id, origin)` via `vault_proto::binding`.
- Network: Rust uses bundled webpki roots. `extra_ca_pem` is settable only in debug builds.
- Commits use `-c user.name="Claude" -c user.email="noreply@anthropic.com"` and the trailers `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019sALvTRSTdS4PZJbSXGRqj`. Never amend; the controller pushes.

## Review Focus

- **Approval without user presence.** No code path sends `Approved` without a successful BiometricPrompt/CryptoObject unwrap. Test: `approve_requires_crk_unwrap` (FFI plus app level).
- **Mailbox-taken warning.** It must block approval. Test: `mailbox_taken_blocks_approval`.
- **Keystore key invalidated** (new biometric enrolled). Opening fails cleanly and the re-wrap-with-recovery-key flow works. Test: `rewrap_after_key_invalidation` (instrumented, simulated by deleting the key).
- **Process death mid-session or mid-approval.** The vault ends locked, no CRK stays in memory after restart, and the browser sees a close. Test: `process_death_locks`.
- **Background replication with a rolled-back replica.** The high-water floor refuses it, and the job reports an error instead of overwriting. Test: `bg_replication_refuses_rollback`.

---

### Task 1: vault-server pairs while locked; unlock at approval
- [ ] Tests first:
  - `serve_offer_while_locked_shows_code`;
  - `approval_unlocks_then_sends_approved`;
  - `approval_unlock_failure_denies`;
  - `wrong_app_checked_from_header_while_locked`;
  - `per_session_static_key_is_fresh`;
  - `mailbox_taken_reported_to_approver` (a new field on `ApprovalRequest`).
- [ ] Implement: `VaultServer` gets a locked-pairing mode, where `serve_offer` uses a fresh random Noise static key per offer. `Approver` returns `Approve(UnlockMaterial)` or `Deny`; the server opens the container with that material (`open_with_crk` plus the high-water) before sending `Approved`. vault-dev is adapted (its identity file is removed; document the migration: none needed).

### Task 2: `vault-ffi` crate
- [ ] Tests first (host):
  - `create_open_roundtrip`;
  - `restore_from_local_replica`;
  - `rewrap_with_recovery_key`;
  - `serve_and_approve_via_callback` (in-process relay plus vault-client native browser);
  - `errors_map_to_ffi_enum`;
  - `extra_ca_only_when_set`;
  - `manifest_key_export_for_background`;
  - `replicate_with_manifest_key_only`.
- [ ] Implement a UniFFI (proc-macro) facade:
  - `VaultFactory` (create, restore, open, rewrap) and `VaultHandle` (stats, compact, lock, serve_offer, replicate) with async methods;
  - an `Approver` callback interface, plus `ReplicaTarget` records (S3/WebDAV/local);
  - the container's CRK exported only as bytes for immediate Keystore wrapping, and the manifest key exported for K_bg sealing;
  - an owned multi-thread tokio runtime;
  - `Zeroizing` buffers everywhere.
  Add a pinned `uniffi-bindgen` binary target, a CI host build and tests, and an `aarch64-linux-android` cross build check with `cargo-ndk` (pinned NDK).

### Task 3: Android project scaffold and CI
- [ ] `android/` with Gradle KTS and a version catalog.
  - Modules: `:core` (Kotlin/JVM), `:ffi` (Android library: loads the .so files and holds the generated bindings, generated at build time by a Gradle task running cargo-ndk plus uniffi-bindgen), `:app` (Compose, empty Vaults list).
  - Dependency verification metadata, the wrapper checksum and the manifest hardening (backup off, data extraction rules, network security config: system CAs, no cleartext, user CAs only in debug).
- [ ] CI job `android`: JDK pinned, Android SDK and NDK via pinned versions, Rust targets, JVM tests, lint, `assembleDebug` and `assembleRelease` (unsigned). Instrumented tests run on the API 30 and latest x86_64 emulators (reactivecircus/android-emulator-runner, SHA-pinned): an FFI smoke test (`ffi_loads_and_creates_vault`). Upload the APKs as artifacts.

### Task 4: `:core` logic
- [ ] Tests first (JVM):
  - `qr_payload_validation`;
  - `pairing_state_machine_happy_path`;
  - `pairing_denied_on_background`;
  - `mailbox_taken_blocks_approval`;
  - `idle_timeout_locks`;
  - `screen_off_lock_rule`;
  - `recovery_key_confirmation_4_of_16`;
  - `vault_identity_normalized`;
  - `replication_schedule_rules` (Wi-Fi only, backoff, rollback error surfaced).
- [ ] Implement pure Kotlin: state machines, the repository and `KeyStore` interfaces (for fakes), and view-model logic classes with no Android imports.

### Task 5: Keystore, storage, create/restore/re-wrap flows
- [ ] Instrumented tests first:
  - `k_auth_params_correct` (KeyInfo check);
  - `k_bg_params_correct`;
  - `wrap_unwrap_crk_with_device_credential_test_key` (the test variant uses an auth-free key — a documented gap);
  - `rewrap_after_key_invalidation`;
  - `files_in_no_backup_dir`;
  - `flag_secure_on_secret_screens`.
- [ ] Implement:
  - the K_auth and K_bg managers; sealed files for the wrapped CRK, high-water, manifest key and replica credentials;
  - the Create screens (app id and origin, recovery key display, re-enter 4 groups);
  - Restore (target, credentials, recovery key, progress, cancel cleanup);
  - the Re-wrap flow;
  - the Vault detail screen with stats.

### Task 6: Pairing — scanner, approval, foreground service
- [ ] Tests first:
  - `scanner_accepts_only_vault_pair` (CameraX + ZXing decode on a fixture image);
  - `approve_requires_crk_unwrap`;
  - `fgs_starts_at_scan_and_stops_on_end`;
  - `stop_action_locks`;
  - `process_death_locks`;
  - `e2e_pairing_with_browser_stand_in` (emulator: in-process relay or a host relay through `10.0.2.2`, vault-client native as the browser, test Approver override only in androidTest).
- [ ] Implement:
  - the Scan screen (camera permission flow) and the Approval screen (app, origin, relay host, device, code, mailbox-taken banner blocking approval, BiometricPrompt with a CryptoObject on K_auth);
  - a `specialUse` foreground service holding the sessions, with a notification showing app and origin plus a Stop action;
  - idle and screen-off locking;
  - WrongApp → "No vault for this app — create one?";
  - the debug-only "import CA" setting.

### Task 7: Background replication and vault settings
- [ ] Tests first:
  - `bg_replication_uses_manifest_key_only`;
  - `bg_replication_refuses_rollback`;
  - `wifi_only_respected`;
  - `replicate_now`;
  - `replication_after_session_end`;
  - `delete_vault_requires_auth_and_typed_app_id`.
- [ ] Implement:
  - a periodic WorkManager job (6 h default, constraints: network, Wi-Fi when set) plus a one-shot run after session end and "replicate now";
  - the replica target editor (S3: endpoint, region, bucket, prefix, key id, secret; WebDAV: URL, user, password), sealed under K_bg;
  - status and last-run UI;
  - settings (auto-lock on screen off, idle timeout, Wi-Fi only, compact now, delete vault).

### Task 8: Release integration and docs
- [ ] `release.yml`: build `vault-android-<version>-unsigned.apk` (all 3 ABIs) and add it to the draft release and to `SHA256SUMS` before signing. Document offline APK signing (`apksigner sign --ks <offline> --v2-signing-enabled true --v3-signing-enabled true`, verify, upload, then `sign-release.sh`).
- [ ] Docs:
  - `docs/android.md` covering install (sideload, signing-cert fingerprint check with `apksigner verify --print-certs`), create, pair with the to-do demo (LAN with the imported CA in a debug build; deployed image with a release build), replication, recovery, re-wrap, what's lost on uninstall, and OEM battery settings;
  - the owner's manual test checklist (real biometric, StrongBox, Doze 30 min, camera from a laptop screen, LAN and deployed demo).
