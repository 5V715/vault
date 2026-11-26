# The Android vault app

The phone is the vault. The app keeps one encrypted container per web app and origin,
approves a browser after you compare a 6-digit code, serves the browser over the relay
while you use the web app, and replicates each vault to S3 or WebDAV. If the phone is not
reachable, the web app does not work: there is no copy of your data anywhere else that
the browser could use.

It is sideloaded only (no Play Store) and needs no Google Play Services. Design:
[`docs/superpowers/specs/2026-11-20-android-vault-design.md`](superpowers/specs/2026-11-20-android-vault-design.md).
What a person has to check on a real phone, because the emulators in CI cannot:
[`docs/android-manual-checklist.md`](android-manual-checklist.md).

> **Placeholder application id.** The app is built as `dev.vault.android`. This id is
> permanent once an APK is installed: Android updates an app only from an APK with the
> same id **and** the same signing certificate. The id is an open owner decision and
> must be settled before the first release. If it changes later, the new app is a
> different app: it cannot read the old one's vaults, which then have to be restored
> from a replica with the recovery key.

## Requirements

- Android 11 (API 30) or later, phone. Built for `arm64-v8a`, `armeabi-v7a` and
  `x86_64` in one APK.
- A **screen lock** (PIN, pattern or password). Every vault's unlock key is a Keystore
  key that needs it. A fingerprint is optional: every prompt accepts the fingerprint or
  the screen-lock PIN.
- Permissions, and nothing else (CI fails if the release APK asks for any other:
  `android/ci/apk-assertions.sh`):

  | Permission | Why |
  |---|---|
  | `INTERNET` | the relay (pairing, sessions) and the replicas, all TLS |
  | `ACCESS_NETWORK_STATE` | "Wi-Fi only" replication: is the network unmetered? |
  | `CAMERA` | scanning the pairing QR code (on the device, no Play Services) |
  | `USE_BIOMETRIC` | the fingerprint/PIN prompt that unwraps a vault's key |
  | `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_SPECIAL_USE` | the session service that keeps a paired browser served |
  | `POST_NOTIFICATIONS` | the session notification with its Stop action (Android 13+ asks) |
  | `RECEIVE_BOOT_COMPLETED` | background replication jobs survive a reboot (no boot receiver of the app's own) |
  | `dev.vault.android.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION` | declared and used by AndroidX Core itself (signature level), not a user permission |

## Install

Releases of `5V715/vault` carry `vault-android-<version>.apk`, signed offline by the
owner. `vault-android-<version>-unsigned.apk` next to it is what CI built; it cannot be
installed and is there so the signed APK can be compared with it.

1. **Download and check the release**, as for the other tools
   ([`docs/install.md`](install.md), "You can check a release by hand"): the signature on
   `SHA256SUMS`, then the APK's hash.

   ```sh
   gh release download v1.2.3 --repo 5V715/vault -p SHA256SUMS -p SHA256SUMS.sig -p vault-android-1.2.3.apk
   ssh-keygen -Y verify -f keys/allowed_signers -I release@vault -n vault-release -s SHA256SUMS.sig < SHA256SUMS
   sha256sum --check --ignore-missing SHA256SUMS
   ```

   The key in `keys/allowed_signers` must match the release-key fingerprint the owner
   publishes outside this repository ([`docs/install.md`](install.md), "Verify the
   release key").

2. **Check the signing certificate.** With `apksigner` from the Android SDK build tools:

   ```sh
   apksigner verify --verbose --print-certs vault-android-1.2.3.apk
   ```

   It must say `Verified using v2 scheme (APK Signature Scheme v2): true` and the same for
   v3, `Number of signers: 1`, and `Signer #1 certificate SHA-256 digest: <hex>`. That hex
   value must equal `keys/android-cert.sha256` in the repository **and** the
   fingerprint the owner publishes outside this repository. Android pins the certificate
   from the first install on: every update must carry the same one, so this check
   matters most for the first install.

3. **Sideload it.** Either `adb install vault-android-1.2.3.apk` from a computer (USB
   debugging on), or copy the APK to the phone and open it in the Files app, which asks
   you to allow "Install unknown apps" for that app. Revoke that permission again
   afterwards (Settings → Apps → Special app access → Install unknown apps).

**Updates** install over the old version (`adb install -r`, or open the new APK). The
vaults stay. An APK signed with another certificate is refused by Android; never
uninstall to get around that (see [What is lost on uninstall](#what-is-lost-on-uninstall)).

**Debug builds** (needed only for the LAN showcase below) come from CI: the `android-apks`
artifact of a `ci` run holds `app-debug.apk`. Each CI run signs it with a fresh debug key,
so a later debug APK cannot update an earlier one: Android insists on uninstalling first,
which deletes the vaults. Keep debug builds for test vaults. Debug and release builds have
the same application id, so only one of them can be installed at a time.

## Create a vault

A vault belongs to exactly one **app id and origin**, for example `vault-todo` at
`https://todo.example`. `https://192.168.1.20:8443` is a different origin and needs its
own vault.

1. **Create a vault**: type the app id and the origin. (After scanning a code for an app
   that has no vault, "No vault for this app" offers to create one, pre-filled. Pairing
   never creates a vault by itself.)
2. Confirm with your fingerprint or screen lock. The app creates the container, then the
   vault's Keystore key, which wraps the container's root key.
3. The **recovery key** is shown once: 16 groups. Write it down on paper. Screenshots are
   blocked in the whole app (`FLAG_SECURE`), and the recent-apps thumbnail is blank.
4. Re-enter the 4 groups the app asks for. Only then is the vault complete; a vault
   whose creation is cancelled or interrupted is removed the next time the app starts.

Then add a replica (below): without one, losing the phone loses the vault.

## Pair a browser

1. In the web app, click **Connect to vault**: it shows a QR code. In the vault app,
   **Scan a pairing code** and point the camera at it (a laptop screen works). Only
   `vault-pair:v1:` codes are accepted; anything else says it is not a vault pairing
   code.
2. The phone shows the app id, the **origin** (check it), the relay host, the browser's
   device name and a 6-digit code. Approve only if the browser shows the same code:
   **Codes match — approve**, then your fingerprint or PIN. That one prompt unlocks the
   vault. **Deny** (or doing nothing for 60 s) refuses.
3. If the app warns that the pairing mailbox was taken by someone else, approval is
   blocked: someone raced the browser to the relay. Scan a new code.
4. The session runs: a notification "Vault sessions" with **Stop** stays while a browser
   is paired. On the lock screen it shows neither the app nor the origin.

Leaving the app while the approval is pending (the Home button, another app) denies it.
The fingerprint/PIN prompt itself does not count as leaving.

### When a vault locks

A locked vault holds no key in memory; the browser must pair again (scan and approve).

- **Stop** in the notification or in the app locks the vault at once.
- **Screen off** locks every vault (Settings: "Lock every vault when the screen turns
  off", on by default).
- **Idle timeout** (Settings, default 15 min, 1 to 240 min): a
  session with no browser request for that long ends and locks. It is one setting for
  the whole app. A change applies to every session started afterwards and whenever no
  session runs; a session that is already running keeps the value it started with.
- **Any end of a session** locks its vault: the browser closed, the network dropped.
- **The app's process dies** (killed by the system, force-stopped, crashed): everything
  is locked. Nothing unlocked is ever written to disk, and the session service is not
  restarted by the system. Open the app and pair again.

## Pair with the to-do demo

The to-do example (`examples/todo`) is the reference web app; its app id is `vault-todo`.

### On your LAN (debug build)

For trying it at home without a public host: a debug build of the app, and the showcase
script on a computer in the same network ([`docs/getting-started.md`](getting-started.md),
"Test with your phone (LAN)").

1. `scripts/showcase.sh --lan` on the computer. It prints the page's origin
   (`https://<LAN IP>:8443`), the path of the local CA `ca.pem` and its SHA-256
   fingerprint.
2. Install a **debug** build of the app (above). The app's TLS (it is Rust code) trusts
   only its bundled public roots, never Android's certificate store, so installing the
   CA in Android's settings does not help the app. Instead: copy `ca.pem` to the phone,
   then at the bottom of the app's vault list (below **Settings**), under **Debug build:
   extra CA for the LAN showcase**, tap **Import a CA certificate (PEM)** and pick the
   file. It is sealed in the app's private storage and
   **applies from the next app start**: close the app (swipe it away in recent apps) and
   open it again. Remove it there when you are done. Release builds have no such
   setting and do not connect to the LAN relay (a certificate error).
3. Create a vault for `vault-todo` and exactly the origin the script printed.
4. Open the page in a browser that trusts the CA (the computer's, or the phone's own
   browser after installing `ca.pem` in Android's settings), **Connect to vault**, scan,
   compare, approve, then **Codes match** in the page.

The vault is bound to the IP: if the computer's address changes, so does the origin,
and the vault for the old one no longer pairs. Give the computer a DHCP reservation.

### Deployed (release build)

The `vault-todo-demo` image (`ghcr.io/5v715/vault-todo-demo`, published by `ci` from
`main`) serves the page and its relay behind a TLS reverse proxy with a Let's Encrypt
certificate: [`docs/deploy-todo-demo.md`](deploy-todo-demo.md). A release build trusts
that certificate as it is (public roots). Create a vault for `vault-todo` and the
deployed origin (for example `https://todo.example`), open the page anywhere, scan.

## Replication

Each vault replicates to one or more targets: an **S3-compatible bucket** (endpoint,
region, bucket, optional prefix, access key; path-style URLs for MinIO and most
self-hosted servers) or **WebDAV** (for example a Nextcloud folder: collection URL, user,
app password). Add them in the vault's detail screen.

- **What leaves the phone** is the container's ciphertext. Background replication uses
  only the manifest key, which cannot read vault data.
- **The replica credentials** are not in the container: they are sealed with the vault's
  *background* Keystore key, which needs no fingerprint but only works while the phone is
  unlocked (`setUnlockedDeviceRequired`). Whoever could misuse that key (with control of
  the unlocked phone) could read the replica credentials, but never vault contents.
- **When it runs** (Android's JobScheduler; the app keeps no job database of its own):
  - every 6 hours, a persisted job per vault that survives reboots (any network, or
    unmetered only with **Wi-Fi only**);
  - right after each session ends;
  - **Replicate now** in the vault's screen, at once (it ignores Wi-Fi only and clears a
    "needs your attention" stop);
  - after a failure: retries 30 s later, doubling up to 6 hours.
- **What delays it**: a running session (replication starts when it ends), a locked
  phone (jobs wait until you unlock it), no network or no Wi-Fi with Wi-Fi only.
  Android stops a job after about 10 minutes; a large first upload continues in the next
  run. Android also spaces out jobs of apps that are rarely opened (standby buckets) and
  some vendors stop them altogether (see [Battery settings](#battery-settings)). The
  vault's screen shows the last run and the last success, and warns when there was no
  successful replication for 24 hours.
- **Needs your attention**: the replicas hold different versions of the vault, or a
  replica is older than the phone's record (rollback), or the key does not fit. Background
  replication stops for that vault until you act (fix the replica, then Replicate now).

The vault's screen also shows its stats (size and segments while a session runs, last
replication, sequence number) and has **Compact now**: after your fingerprint or PIN the
vault is unlocked only to rewrite it without dead data, then locked again. It is refused
while a session or a replication uses the vault.

## Recovery

**Restore a vault** brings a vault back from a replica, on a new phone or after an
uninstall: pick S3 or WebDAV, type the replica's credentials, then the recovery key. The
whole vault is downloaded; cancelling keeps nothing. The vault's app id and origin come
from the replica. The replica's sequence number becomes the phone's anti-rollback floor
(trust on first restore). The credentials you typed are sealed as the vault's first
replica.

### Re-wrap with the recovery key

Removing the phone's screen lock (or switching to "None"/"Swipe") makes Android delete
every key that needs it: each vault's unlock key is gone, while the container is intact.
The vault then shows **Re-wrap with recovery key**: set a screen lock again, open the
vault with the recovery key, and the app protects it with a new key.

Adding or removing a **fingerprint** does not invalidate anything: the key also accepts
the PIN, which is needed to enroll a fingerprint anyway.

## What is lost on uninstall

Uninstalling the app, **Clear storage** in the app's settings, a factory reset, or moving
to a new phone deletes every local vault: the containers, their wrapped keys, the
anti-rollback records, the sealed replica credentials and the settings. None of it is in
any backup (`allowBackup=false`, data extraction rules exclude everything), and the
Keystore keys never leave the phone.

What you need to get a vault back: **the recovery key** (paper) and **the replica's
address and credentials** (they are only on the phone otherwise). Restore as above.

**Delete vault** (vault screen; fingerprint/PIN plus typing the app id) deletes one vault
from the phone. Replicas are not touched.

## Battery settings

A session runs in a foreground service with a notification, which Android keeps alive
with the screen off. Doze still restricts network for other apps, and many vendors kill
background apps regardless, which ends sessions and stops background replication. The
browser side handles a dropped session safely (it asks to pair again). If sessions or
replication stop on your phone, set the app to unrestricted battery use:

- **Android (Pixel, stock):** Settings → Apps → Vault → App battery usage → **Unrestricted**.
- **Samsung (One UI):** the same, plus Settings → Battery → Background usage limits →
  **Never sleeping apps** → add Vault; make sure it is not in "Sleeping apps" or "Deep
  sleeping apps".
- **Xiaomi / Redmi / POCO (MIUI, HyperOS):** app info → **Autostart** on; Battery saver →
  **No restrictions**; in recent apps, pull the app's card down to lock it.
- **Huawei / Honor:** Settings → Battery → App launch → Vault → **Manage manually** with
  auto-launch, secondary launch and run in background on.
- **OnePlus / OPPO / realme (ColorOS, OxygenOS):** app info → Battery → **Allow background
  activity** (and turn off "optimize"), lock the app in recent apps.

Menu names change between versions; <https://dontkillmyapp.com> keeps current
instructions per vendor. On Android 13 and later the app asks for the notification permission when you open the
scanner. Without it the session still runs, but its notification (and its Stop action)
is hidden: allow it.

## Security notes and limits

- Plaintext vault data, root keys, recovery keys and replica credentials exist only in
  memory, in the Keystore, or encrypted (container, sealed files). No logs, no backups,
  no screenshots.
- Each vault has two Keystore keys: the **unlock key** (AES-256-GCM, a fingerprint or
  PIN for every use, only while the phone is unlocked, StrongBox when the phone has it
  and accepts the parameters, else the TEE) wraps the container's root key; the
  **background key** (no prompt, only while the phone is unlocked) seals the
  anti-rollback record and the replica credentials. Which one the hardware backs is not
  shown in the app (manual checklist).
- The anti-rollback record is a sealed file that is not bound to a counter: someone who
  can write the app's private storage (root, a compromised OS) can put back an older
  record together with an older container. That attacker is outside what the app
  defends against.
- Debug builds trust user-installed CAs for Android's own networking and the one
  imported CA for the app's TLS; they also contain test hooks. Use them for test vaults
  only. CI checks that release APKs contain none of this, trust only system CAs, permit
  no cleartext and export only the launcher activity (`android/ci/check-apks.sh`).

## Releases (owner)

The `release` workflow builds the unsigned APK for a tag; the owner signs it offline, then
signs `SHA256SUMS`. The order matters: `SHA256SUMS` is signed **last**, so its signature
covers the signed APK that is published, not only CI's unsigned one.

### Before the first release (once)

1. Settle the application id (`android/app/build.gradle.kts`, `applicationId`, see the
   note at the top).
2. Create the signing keystore **offline**, on the signing machine, with a strong
   password, and back it up (losing it means no updates: users would have to uninstall,
   losing their vaults, and restore). For example:

   ```sh
   keytool -genkeypair -keystore /media/offline/vault-android.p12 -storetype PKCS12 \
     -alias vault-android -keyalg RSA -keysize 4096 -validity 36500 -dname "CN=vault android"
   ```

3. Commit the certificate's SHA-256 (the value `apksigner` reports), lower-case hex, one
   line:

   ```sh
   keytool -exportcert -keystore /media/offline/vault-android.p12 -alias vault-android | sha256sum | cut -d' ' -f1 > keys/android-cert.sha256
   ```

   Publish the same value outside this repository, next to the release-key fingerprint.
   Until the file holds a real value, `scripts/sign-android-apk.sh` refuses to sign.

### Every release

1. Raise `versionCode` and set `versionName` to the release version (the tag without
   `v`) in `android/app/build.gradle.kts`. The workflow refuses a tag whose version does
   not equal the APK's `versionName`.
2. Tag and push (`git tag v1.2.3 && git push origin v1.2.3`). The `release` workflow builds
   the tools and, through `.github/workflows/android-release-apk.yml`, the APK with all
   three ABIs from a clean state (no Gradle cache, `--no-build-cache`), runs the APK
   checks, and creates a **draft** holding the archives, `vault-android-1.2.3-unsigned.apk`
   and `SHA256SUMS` over all of them. (`ci` runs the same APK workflow on every push.)
3. On the signing machine (needs `gh` and `apksigner` from the SDK build tools; set
   `APKSIGNER=/path/to/build-tools/<version>/apksigner` if it is not on `PATH`):

   ```sh
   scripts/sign-android-apk.sh v1.2.3 /media/offline/vault-android.p12 [alias]
   ```

   It refuses unless the release is an unsigned draft without a signed APK and
   `keys/android-cert.sha256` is set. It downloads the unsigned APK and checks it against
   `SHA256SUMS`, signs it with
   `apksigner sign --ks <keystore> --v1-signing-enabled false --v2-signing-enabled true --v3-signing-enabled true --v4-signing-enabled false`
   (apksigner asks for the keystore password), then runs `apksigner verify --print-certs`
   and requires v2, v3, one signer and the pinned certificate. Only then does it upload
   `vault-android-1.2.3.apk`, replace `SHA256SUMS` with one that also lists it, and check
   the draft serves that file.
4. Then sign and publish as for every release ([`docs/install.md`](install.md),
   "Release procedure"):

   ```sh
   scripts/sign-release.sh v1.2.3 /media/offline/vault-release
   ```

   It now requires `SHA256SUMS` to list exactly the five archives and both APKs, and
   checks that the signed APK on the draft is the one listed, before it asks you to sign.
5. Install the published APK on a phone over the previous release (the update path) and
   go through the release part of the [manual checklist](android-manual-checklist.md).

If step 3 stops after uploading the APK but before `SHA256SUMS`, delete the uploaded APK
from the draft (`gh release delete-asset v1.2.3 vault-android-1.2.3.apk --repo 5V715/vault`)
and run it again. Both scripts are tested against a fake GitHub
(`scripts/test/sign_android_apk_test.sh`, `scripts/test/sign_release_test.sh`); CI also
runs the signing procedure with the real `apksigner` on each built APK, with a throwaway
keystore.

## Building

`android/` is a Gradle project (Kotlin DSL; pinned versions in
`android/gradle/libs.versions.toml`, dependency verification in
`android/gradle/verification-metadata.xml`). It needs the Android SDK platform and build
tools, the pinned NDK, Rust with the Android targets and the pinned `cargo-ndk`; the
`.github/actions/android-toolchain` action shows the exact setup. Run Gradle through
`android/ci/gradle.sh` (strict dependency verification):

```sh
android/ci/gradle.sh :core:test assembleDebug assembleRelease
```

`:core` is plain Kotlin/JVM (state machines, policies) and its tests run anywhere with a
JDK. The instrumented tests run on emulators in CI (`android-emulator` job, API 30 and
36) against a host-side relay and browser stand-in.
