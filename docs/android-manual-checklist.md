# Android vault app: manual test checklist (owner)

CI runs the app's instrumented tests on x86_64 emulators (API 30 and 36) against a
host-side relay and browser stand-in. Emulators have no fingerprint sensor, no StrongBox,
no camera worth the name, no vendor battery killer, and the tests keep the screen on and
replace the BiometricPrompt with a stand-in. Everything below is therefore **not tested
in CI** and has to be checked by hand on real phones before a release: at least one
phone on **Android 11 (API 30)** and one on a **recent version (API 36 or later)**.

Use test vaults only (data you can lose). Tick a box only after seeing the expected
result. `adb` commands assume USB debugging; `<pkg>` is `dev.vault.android` (placeholder,
see item 0).

## 0. Before the first release

- [ ] **Application id decided.** `dev.vault.android` is a placeholder
      (`android/app/build.gradle.kts`). It cannot change after users install the app
      without them restoring every vault from a replica. Owner decision.
- [ ] **Signing keystore** created offline and backed up; `keys/android-cert.sha256`
      committed; the fingerprint published outside the repository
      ([`docs/android.md`](android.md), "Releases").

## 1. Install and update

- [ ] **Release build** from a published release: `SHA256SUMS.sig` verifies, the APK's
      hash matches, `apksigner verify --verbose --print-certs` shows v2 and v3, one
      signer, and the certificate SHA-256 equals `keys/android-cert.sha256` and the
      published value. Sideload it (Files app and `adb install`).
- [ ] **Update over the previous release** (`adb install -r` or the Files app): vaults,
      replicas and settings are all still there; a vault pairs and its data is intact.
- [ ] An APK signed with another key (for example a debug build) is **refused** as an
      update.
- [ ] Android shows the permissions of [`docs/android.md`](android.md) and no others
      (Settings → Apps → Vault → Permissions; `adb shell dumpsys package <pkg> | grep permission`).

## 2. Unlock prompt (real BiometricPrompt)

- [ ] **Fingerprint**: create a vault, pair, approve with a fingerprint: the session runs.
- [ ] **PIN instead**: at the prompt choose "Use PIN" and enter the PIN: the session runs.
- [ ] **Cancel** the prompt (Back or Cancel): the browser is denied; nothing is unlocked.
- [ ] **Switching apps during PIN entry**: start approving, switch to the PIN screen,
      then press Home (or switch to another app) before finishing. Expected: the
      approval is **denied** (leaving the app denies), and the browser says so. Then
      approve normally and check that entering the PIN *without* leaving does **not**
      deny (the PIN screen only pauses the app). Note the phone model if the PIN screen
      itself denies: that phone stops the activity during the prompt.
- [ ] **Too many wrong fingerprints**: the app says to unlock the phone with its PIN,
      then try again; after that it works.
- [ ] **New fingerprint enrolled** (Settings → Security → add a fingerprint): every vault
      still unlocks with fingerprint and PIN (by design the key is not invalidated).
- [ ] **Screen lock removed** (set to None/Swipe, then a new PIN): each vault shows that
      its unlock key was invalidated and offers **Re-wrap with recovery key**; re-wrap
      with the recovery key, then pair and approve: works. Cancelling the re-wrap prompt
      keeps the re-wrap option.
- [ ] **Compact now** asks for fingerprint/PIN, compacts, and leaves the vault locked;
      it is refused while a session runs.
- [ ] **Delete vault** needs fingerprint/PIN and the typed app id; replicas are untouched.

## 3. Keystore hardware

- [ ] **StrongBox**: `adb shell pm list features | grep -i strongbox` on each phone; note
      which have `android.hardware.strongbox_keystore`. The app uses StrongBox when the
      feature exists and falls back to the TEE without telling you if StrongBox rejects
      the key parameters; the app does not show which one a key uses. On a StrongBox
      phone: create a vault and check the unlock prompt and pairing still work (StrongBox
      is slower; approval must stay well inside the 60 s offer lifetime).
- [ ] **Keys need an unlocked phone** (`setUnlockedDeviceRequired`, only asserted in CI
      on API 37+), on the **API 30** phone and the **API 36** phone: with a replica set
      up, lock the phone and force the replication job
      (`adb shell cmd jobscheduler run -f <pkg> <job id>`, ids from
      `adb shell dumpsys jobscheduler | grep -A3 <pkg>`). Expected: the vault's screen
      later shows it waited for an unlocked phone; after unlocking, the next run
      succeeds. No replication succeeds while the phone stays locked.

## 4. Camera

- [ ] Scan a pairing QR from a **laptop screen** (normal brightness, 20–50 cm, slight
      angle): it is read within a few seconds.
- [ ] A **non-vault QR** (a URL) shows "not a vault pairing code" and nothing else
      happens.
- [ ] **Camera permission denied** (and "don't ask again"): the scanner offers the app's
      settings; after allowing it there, scanning works.
- [ ] An **expired** code (wait > 60 s after the page shows it) and an **unreachable relay**
      (phone offline) each give a clear message.

## 5. Sessions and locking

- [ ] **Notification**: while a session runs, "Vault sessions" shows the app and origin
      with **Stop**; on the lock screen it shows neither. **Stop** in the notification
      locks the vault at once and the browser loses the session.
- [ ] **Screen off locks** (setting on, the default): with a session running, press
      power. Expected: the session ends, the vault is locked; the browser must pair
      again.
- [ ] **Screen off with the setting off**: the session survives screen off and on.
- [ ] **Idle timeout**: set it to 1 min; start a session and leave the browser idle for
      over a minute: the session ends and the vault locks. Change it during a running
      session: that session keeps its old value; the next one uses the new value.
- [ ] **Process death locks everything**: with a session running, kill the app's
      process (`adb shell am kill <pkg>` does nothing for a foreground app; use
      `adb shell run-as <pkg> kill -9 <pid>` on a debug build, or Settings → Apps →
      Vault → Force stop on a release build). Expected: the browser loses the session,
      the notification disappears, the service is not restarted; opening the app shows
      every vault locked; pairing again needs a new approval.
- [ ] **Mailbox taken**: if possible (two scanners on one code), the warning is shown and
      approval is blocked.
- [ ] No screenshot or screen recording shows app content; the recent-apps thumbnail is
      blank.

## 6. Background, Doze, battery

- [ ] **Foreground service with the screen off** (screen-off lock **off**): start a
      session, turn the screen off, wait 5 min, use the web app from the laptop: it
      still works.
- [ ] **Doze, about 30 min**: the same with the phone lying still, unplugged, screen off,
      for 30 minutes (or force it: `adb shell dumpsys battery unplug; adb shell dumpsys
      deviceidle force-idle`, then `adb shell dumpsys deviceidle unforce; adb shell
      dumpsys battery reset`). Expected: the session survives, or ends cleanly (the
      browser asks to pair again, nothing is unlocked afterwards). Note which.
- [ ] **OEM battery settings**: on each vendor phone you have, repeat the 30 min test
      with the default battery settings and again with the settings in
      [`docs/android.md`](android.md) ("Battery settings"). Correct the docs if a menu
      name differs.
- [ ] **Periodic job really fires** (CI's tests bypass the real scheduler): add a
      replica, leave the app closed; `adb shell dumpsys jobscheduler | grep -A5 <pkg>`
      lists one persisted periodic job per vault (and no others when idle). Either wait
      past 6 h or force it (`cmd jobscheduler run -f <pkg> <job id>`): "Last run" and "Last
      success" in the vault's screen move; the replica holds the new data.
- [ ] **After a session**: a one-shot replication runs within a few minutes after the
      session ends.
- [ ] **Boot persistence**: reboot the phone and unlock it; `dumpsys jobscheduler` still
      lists the periodic job, and it runs (force it) without opening the app first.
- [ ] **Wi-Fi only**: on mobile data only, the job waits ("Waiting for Wi-Fi."); on Wi-Fi
      it runs. **Replicate now** runs on mobile data anyway.
- [ ] **Stale warning**: with an unreachable replica (wrong host), after 24 h without
      success the vault shows the warning.

## 7. Replication and recovery with real servers

- [ ] **S3** (a real bucket, and MinIO with path-style URLs) and **WebDAV** (Nextcloud
      with an app password): add, replicate now, check the objects exist and contain no
      plaintext (only container files).
- [ ] **Restore on a second phone (or after uninstall)** with the recovery key and the
      replica credentials: the vault's data is there; it pairs with the same web app
      and origin.
- [ ] **Uninstall loses the vault**: uninstall, reinstall: no vaults; restore works.
      A wrong recovery key is refused with a clear message.
- [ ] **Diverged replicas** (restore the same vault on two phones, write on both, then
      replicate both): the second shows "Needs your attention" and background
      replication stops for it.

## 8. To-do demo, end to end

- [ ] **LAN, debug build**: `scripts/showcase.sh --lan` on a laptop; debug APK from a CI
      run; import `ca.pem` in the app (vault list → Debug build: extra CA → Import),
      restart the app; create `vault-todo` at the printed `https://<LAN IP>:8443`; open
      the page in a browser that trusts the CA; scan, compare, approve, **Codes match**;
      add, reload and edit to-dos; Stop and pair again: the to-dos are still there.
- [ ] **LAN with a release build** fails with a certificate error (no user CAs in
      release), as intended.
- [ ] **Deployed demo, release build**: the `ghcr.io/5v715/vault-todo-demo` image behind
      Traefik or Caddy with a Let's Encrypt certificate
      ([`docs/deploy-todo-demo.md`](deploy-todo-demo.md)); create `vault-todo` at its
      origin; pair from a laptop on another network (phone on mobile data); use the app;
      background replication runs after the session.

Record for each run: phone model, Android version, security patch level, app version,
and anything that did not match the expected result.
