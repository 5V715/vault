# Android vault app (V2b) — design

Status: draft for owner approval · 2026-11-20 · repo `5V715/vault`, directory `android/`

## Intent

The phone becomes the vault: it holds each app's encrypted container, approves
browser sessions after the user compares a code, serves the session over the
relay, and replicates to S3 or WebDAV. This replaces `vault-dev` for daily use.
The owner demands maximum security; if the phone (vault) is unreachable, the web
app doesn't work.

## Owner decisions

- Android only for now: Kotlin + Jetpack Compose. The app logic lives in a plain
  Kotlin/JVM module with no Android dependencies, so KMP stays a refactor later.
- Sideload only: CI builds the APK and the owner signs it offline. Store releases
  come later.
- Minimum Android 11 (API 30); target the current API level.
- Rust core via UniFFI. Reuse `vault-container` and `vault-server` as they are.

## Goals

1. Create a vault for an app (app id + origin). Show the recovery key exactly
   once, with a confirmation that it was written down: the user re-enters 4 of its
   16 groups.
2. Restore a vault from a replica (S3/WebDAV) plus the recovery key.
3. Pair: scan the QR → show the app id, origin, relay host, device name and the
   6-digit code → the user approves with a strong biometric or the device
   credential → the session runs.
4. Keep the session alive while the browser is in use (foreground service with a
   notification and a "Stop" action). Lock on stop, on screen lock (configurable),
   and after an idle timeout.
5. Replicate each vault to one or more targets. Credentials are stored inside that
   vault's own container. There is a "Wi-Fi only" switch and a manual "replicate
   now".
6. Several vaults side by side, one per app + origin (the vault model is "one per
   app for now").

## Non-goals

Play Store distribution, iOS/KMP, multi-app vaults, receiving Signal history or
media (`signal-web` milestones), push notifications to wake the phone (the browser
pairs while the app is open), WebRTC/LAN direct transport (V3), Wear OS, tablets
beyond what Compose gives for free.

## Anti-goals

- Plaintext vault data, CRKs, recovery keys or replica credentials outside
  memory, the Keystore, or the encrypted container: no logs, no shared prefs in
  the clear, no screenshots of secret screens (`FLAG_SECURE`), no backups
  (`allowBackup=false`, data extraction rules exclude everything).
- Approving a session without user presence (biometric or device credential) and
  an explicit code comparison.
- Any Google Play Services dependency (ML Kit, Play Integrity, Firebase). The app
  must build and run on a de-Googled phone.

## Architecture

```
┌─────────── Android app (android/) ────────────────────────────┐
│ :app        Compose UI, Activities, FGS, Keystore, CameraX     │
│ :core       pure Kotlin/JVM: state machines, view models'      │
│             logic, repository interfaces (testable on JVM)     │
│ :ffi        generated UniFFI Kotlin bindings + .so loading     │
└───────────────┬────────────────────────────────────────────────┘
                │ UniFFI (async → suspend)
┌───────────────▼─── vault-ffi (new Rust crate) ─────────────────┐
│ thin facade over vault-container (create/open/restore, stats,  │
│ replicate) and vault-server (VaultServer, Approver callback,   │
│ serve_offer, lock/unlock, compaction); tokio runtime owned here│
└────────────────────────────────────────────────────────────────┘
```

- **vault-ffi** is the only new Rust crate. It exposes coarse, async,
  UniFFI-friendly calls (`VaultHandle.create/open/restore/lock/serve_offer/...`,
  `Approver` as a callback interface returning approve/deny), bytes as
  `Vec<u8>`/`ByteArray`, and errors as a small enum. Secrets returned across the
  boundary are limited to the recovery key once at creation and the CRK only as
  ciphertext for wrapping. *My call:* Kotlin wraps and unwraps the CRK with a
  Keystore AES-GCM key, so the plaintext CRK crosses FFI exactly twice per
  unlock (Kotlin→Rust) and is zeroed on both sides afterwards (best-effort on the
  JVM: `ByteArray.fill(0)`).
- **Storage:** each vault's container uses the local file backend in the app's
  no-backup directory (`noBackupFilesDir/vaults/<id>/`). App-private, excluded from
  backup, encrypted by the container itself.
- **Keystore:** per vault, an AES-256-GCM key
  (`setUserAuthenticationParameters(0, BIOMETRIC_STRONG | DEVICE_CREDENTIAL)`,
  `setUnlockedDeviceRequired(true)`,
  StrongBox when `FEATURE_STRONGBOX_KEYSTORE` is available, else TEE). It wraps
  the CRK; the wrapped CRK lives next to the container. Auth timeout 0 means
  every unlock needs a BiometricPrompt with a CryptoObject. The approval prompt
  *is* that unlock, so approving a session and unlocking the vault are one user
  action.
- **Noise static key:** derived inside vault-server from the CRK, as `vault-dev`
  does today. No separate Android storage.
- **Session lifecycle:** a foreground service (type `specialUse`, allowed for
  sideloaded apps; declared with a property describing "encrypted storage session
  for a paired browser") holds the `VaultServer` while a session is active.
  - The notification shows the app id, origin and a Stop action.
  - Stop locks: it drops the CRK and the server.
  - The service stops itself when the session ends, and also after a configurable
    idle period with no browser activity (default 15 min).
- **QR scanning:** CameraX + ZXing core (Apache-2.0, no Play Services). Only
  `vault-pair:v1:` payloads are accepted. Anything else shows "not a vault pairing
  code".
- **Approval screen:**
  - Shows the app id, origin (prominent), relay host, browser device name, the
    6-digit code and a warning to approve only if the browser shows the same code.
  - The buttons are "Codes match — approve" (opens the BiometricPrompt) and "Deny".
  - `FLAG_SECURE` is set on this screen.
  - A WrongApp result (QR for a different app/origin than any vault on the phone)
    shows "No vault for this app" and offers to create one. Pairing never
    silently creates a vault.
- **Replication:** see the owner decision below. Background replication runs
  through Android's JobScheduler (no WorkManager, no app-side job database) with
  the manifest key only, plus right after each session. "Wi-Fi only" is the
  job's unmetered-network constraint and a capability check before starting.
- **Network trust:** release builds trust only system CAs. Debug builds also trust
  user CAs, for the LAN showcase. `network_security_config` sets
  `cleartextTrafficPermitted=false` everywhere except loopback in debug.
- **Settings:** app-wide auto-lock on screen off (default on) and idle timeout
  (owner decision 2026-11-26: it applies to every session started after a
  change, a running one keeps its own; no restart). Per vault: Wi-Fi-only
  replication, replicate now, show stats (size, segments, last replication,
  seq), compact now, delete vault (requires auth + typed app id).
- **Compact now** (owner decision 2026-11-26) runs at once: the fingerprint/PIN
  prompt unwraps the CRK (the approval's unlock path, re-wrap check included),
  the vault is unlocked only for the compaction (`VaultHandle.compact_with_crk`),
  then locked and the CRK wiped. Refused while a session or replication holds
  the vault.

## UI flows (screens)

1. **Vaults list** (empty state: "Create a vault" / "Restore a vault")
2. **Create:** app id + origin (pre-filled when coming from a WrongApp scan) →
   recovery key display (`FLAG_SECURE`, monospace groups) → re-enter 4 random
   groups → done.
3. **Restore:** pick target type → credentials → recovery key → restores (progress) →
   done.
4. **Scan** (camera) → **Approve** (above) → **Session running** (notification +
   in-app status: browser device, since, Stop).
5. **Vault detail:** replication targets, settings, stats.

## Build, signing and CI

- Gradle (Kotlin DSL, version catalog). AGP, Kotlin, Compose BOM, CameraX, ZXing,
  and the UniFFI bindgen are pinned exactly; Gradle dependency verification
  (`verification-metadata.xml`) is enabled. The Gradle wrapper is checksum-pinned.
- Rust for Android via `cargo-ndk` (pinned) for `arm64-v8a`, `armeabi-v7a` and
  `x86_64`. The NDK version is pinned. The UniFFI bindings are generated in CI
  from the built library, never committed.
- CI job `android`:
  - Rust FFI tests (host), JVM unit tests for `:core`, lint;
  - an assembled unsigned release APK plus a debug APK;
  - instrumented tests on an x86_64 emulator (API 30 and the latest): FFI smoke
    (create, write, reopen with the wrapped CRK), pairing against an in-process
    relay and a browser stand-in (vault-client native) using a test-only
    `Approver` override available only in the androidTest build variant;
  - SHA-pinned actions, `persist-credentials: false`.
- Release: the tag workflow attaches `vault-android-<version>-unsigned.apk` and its
  SHA-256 to the draft release. The owner signs offline (`apksigner sign --ks
  <offline keystore>` with the v2+v3 schemes), verifies, uploads the signed APK,
  then runs the existing `sign-release.sh` (SHA256SUMS now includes the APK). The
  docs give the signing-certificate fingerprint check for users (`apksigner verify
  --print-certs`). *(Task 8: `scripts/sign-android-apk.sh` does the signing,
  verification against the pinned certificate `keys/android-cert.sha256`, the upload
  and the SHA256SUMS line before `sign-release.sh`; with minSdk 30, apksigner writes
  only the v3 signature even with v2 enabled, so v3 is what is required.)*

## Testing strategy

- Rust: vault-ffi unit tests (lifecycle, error mapping, approver callback,
  cancellation) on the host.
- JVM (`:core`): state machines for pairing (scan → approve → running → ended),
  lock rules, idle timeout, QR payload validation, the recovery-key confirmation.
- Instrumented (emulator): Keystore wrap/unwrap (no biometric on the emulator: the
  test variant uses a device-credential-less key with an `androidTest`-only flag —
  documented as a test gap), FGS start/stop, end-to-end pairing with the browser
  stand-in, `FLAG_SECURE` set on secret screens.
- Manual checklist for the owner's phone: real biometric prompt, StrongBox
  presence, camera scan from a laptop screen, Doze (screen off 30 min with an
  active session), LAN showcase with the user CA, deployed demo image with Let's
  Encrypt.

## Risks

- **UniFFI async on Android:** coroutine bridging and cancellation. Mitigation:
  coarse APIs and an owned runtime in vault-ffi.
- **Doze and OEM battery killers** can cut the session. The FGS helps. The docs
  name settings for Samsung, Xiaomi and others, and the browser side already
  handles disconnects safely.
- **Emulator tests can't exercise real biometrics or StrongBox.** This is covered
  by the manual checklist.
- **APK size** (three ABIs × the Rust core) is likely 15–30 MB. Acceptable for
  sideloading; split APKs later if needed.

## Settled details (builder check)

- **Noise static key:** a fresh random key per pairing. The protocol never pins
  the vault's static key, because the 6-digit code authenticates every session.
  Nothing is stored. *(Corrects the draft, which said "derived from the CRK";
  vault-dev's persisted identity file is unnecessary.)*
- **Order of events:** scan → vault-server starts the handshake with the
  per-session static key while the container is still locked → the code is
  shown → "Codes match — approve" → BiometricPrompt unwraps the CRK → the
  container is unlocked → `Approved` is sent. This needs a small vault-server
  change: `serve_offer` works while locked, and the approval path performs the
  unlock (today `serve_offer` returns `Locked`). The binding check (WrongApp)
  uses the container header, which is readable without the CRK, and still
  happens before the prompt.
- **High-water (anti-rollback):** it is stored outside the container in
  `noBackupFilesDir`, sealed with a second per-vault Keystore AES-GCM key that
  needs no user authentication (`setUnlockedDeviceRequired(true)`), because it
  is updated on every commit during a session. A restore starts with the
  replica's sequence as its floor (trust on first restore, as `vaultctl restore`
  does today).
- **Unlock path:** only `open_with_crk` with the Keystore-wrapped CRK. Removing
  the screen lock invalidates the key. *(Owner decision 2026-11-21: a new
  biometric does not. Android only invalidates biometric-only keys on
  enrollment; this key also accepts the PIN, which is required to enroll a
  biometric and already unlocks the key, so invalidation would add nothing.)*
  The app then offers **"Re-wrap with recovery key"**:
  `open_with_recovery` on the local container, then a new Keystore key. This flow
  is added to the goals and screens.
- **Approver timing:** the approval wait equals the offer lifetime (60 s). If the
  app goes to the background, or the activity dies while the prompt is pending,
  the answer is deny. The foreground service starts at scan time, while the app
  is in the foreground (Android 11+ background-start rules).
- **Concurrency:**
  - One active session per vault.
  - Several vaults can each have a session, all held by one foreground service.
  - Container operations are serialized behind the vault-server lock.
  - Compaction and replication run only when the vault has no active session
    (existing policy).
- **Vault identity:** the exact normalized `(app_id, origin)` pair. The LAN
  showcase vault (`https://192.168.x.y:8443`) and a deployed one
  (`https://todo.example`) are separate vaults.
- **Restore:**
  - The user types the replica credentials for the first fetch. They are then
    sealed under the background key.
  - The container is written completely before the Keystore key is created.
  - Cancelling deletes the partial directory.
- **Rust TLS:** Android's network security config doesn't apply to Rust
  networking, so Rust uses bundled webpki roots (Mozilla) everywhere. Debug builds
  only can import one extra CA (PEM) in settings for the LAN showcase. It is
  passed as `extra_ca_pem`, like vault-dev's `--ca-file`.
- **Pairing errors:**
  - An expired offer, an unreachable relay or a malformed QR each shows a clear
    message.
  - The mailbox-taken warning is shown prominently and **blocks approval**.
- **Not replicated:** the wrapped CRK and the high-water file live outside the
  container directory and never go to replicas. Uninstalling the app or moving
  to a new phone loses the local vault unless it is restored from a replica with
  the recovery key. The docs say this.

## Owner decisions (2026-11-20, continued)

- **Background replication (option B):**
  - Replication also runs while the vault is locked, via JobScheduler (owner
    decision 2026-11-26, replacing WorkManager: no unsealed app-side job
    database), optionally on Wi-Fi only, whenever the device is unlocked or the
    job runs. Jobs: one persisted periodic job per vault (6 h; any network or
    unmetered), a one-shot after each session end, and retries; "replicate now"
    runs in the app. A job's id and extras carry only the random vault id (and
    its kind). Scheduling sits behind a small interface (`ReplicationJobs`) whose
    decisions come from `:core`'s `ReplicationPolicy`, so iOS can later put
    BGTaskScheduler behind the same interface.
  - It uses only the manifest key, which cannot read vault data, plus the replica
    credentials. Both are sealed with a per-vault background Keystore key
    (AES-GCM, `setUnlockedDeviceRequired(true)`, no user authentication), the same
    key class as the high-water file.
  - Replica credentials therefore live **outside** the container, sealed under
    this key: the earlier "inside the container" idea is replaced.
  - Someone who can misuse that key could copy ciphertext and read the replica
    credentials, but never vault contents.

## What the builder decides

Module internals, the exact UniFFI API shape within the constraints above,
Compose structure, test fakes, and the idle-detection mechanism (relay pings vs.
data activity).
