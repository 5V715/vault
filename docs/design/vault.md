# Vault design

Extracted from the design notes written in the signal-web repository (2026-11-18),
keeping the parts that concern the vault, its container, relay mailbox transport
and pairing; app-specific sections are omitted or reworded to "the app". The wire
details are in `docs/protocol/vault.md`, the container and CLI in `docs/vault/README.md`.
Label strings are the neutral `vault/v1/...` ones; the full list is in
`docs/protocol/vault.md` ("Versions and labels").

## Intent

Separate a web app from everything user-specific. The browser becomes a
stateless client: it holds app state only in memory while a session is
open. All persistent state lives in an encrypted **container** owned by a
**Vault** app on the owner's Android phone. The vault replicates the
encrypted container to S3 or a NAS. Losing or sharing a laptop exposes
nothing; sessions are approved and revoked on the phone.

## Decisions taken with the owner

- Maximum security: **if the vault is unreachable, the app does not
  work.** No browser fallback to replicas.
- The browser never talks to replicas. S3/NAS are the vault's backups.
- **Restore key (option A):** creating a container shows a recovery key once;
  with it and a replica, a new phone restores the vault. Without it, a lost
  phone means the data is gone.
- Android first; iOS later (background networking limits).
- **All persistent user data lives only in the encrypted container on the
  phone.** The browser keeps no user data and no browser identity key:
  **every session is paired fresh** (QR scan + biometric on the phone).
  There is no passphrase or IndexedDB mode in the browser.
  Only app-integrity state stays in the browser: the app's signed bundle and its pinned release version, which are
  the embedding app's concern and hold no user data.

## Non-goals

- Browser reading replicas directly; multiple concurrent writers; sharing a
  container between people; SMB.

## Anti-goals

- The vault, relay or a replica provider learning plaintext or long-term keys
  they don't need.
- A browser session surviving revocation or holding the container's
  long-term key.
- Silent rollback by a replica provider going unnoticed.

## Architecture

```
 browser (stateless)                      Android Vault                    replicas
 ┌──────────────────────┐   Noise XX over  ┌───────────────────────┐  sealed  ┌────────┐
 │ core Worker (RAM only)│◄─relay mailbox─►│ Rust core (UniFFI)     │ segments │ S3     │
 │  KvBackend = Remote   │   (later WebRTC)│  Container (V1 format) │────────►│ WebDAV │
 └──────────────────────┘                  │  Keystore + biometrics │         └────────┘
                                           └───────────────────────┘
```

### Key hierarchy

- **CRK** (container root key, 32 random bytes) — generated with the container.
- CRK is stored only **wrapped**: (1) by an Android Keystore AES-GCM key,
  hardware-backed (StrongBox when available), `setUserAuthenticationRequired`
  per use, biometric or device credential; (2) by the **recovery key**.
- **Recovery key**: 32 random bytes, shown once as 64 base32 characters in
  groups of 4 plus a QR code; wrap key = HKDF-SHA256(recovery key,
  "vault/v1/recovery"). Shown again only after biometric
  confirmation. Rotation (re-wrapping the CRK under a new recovery key) is
  **not implemented yet** (future work; replicas' headers would then need an
  explicit update).
- From CRK via HKDF: `enc` (segment AEAD), `mac` (record-key HMAC),
  `manifest` (manifest sealing), `manifest-auth` (HMAC of the manifest's inner
  `auth_tag`), `replica-id` (opaque object names).

### Container format (V1)

- `header`: the fixed marker `"vault"`, the format version, the recovery-wrapped
  CRK and the app binding (`app_id`, `origin`), nothing else (the Keystore-wrapped
  CRK is device-local, kept outside the container and never replicated). The
  binding is the only plaintext identifier; every manifest carries the same
  wrapped CRK and binding under its `auth_tag`, so a changed header fails to open.
- `seg/<random 128-bit id>`: immutable, AES-256-GCM framed chunks
  (64 KiB) of record changes (put/delete with HMAC'd keys); padded to size buckets to blur record sizes.
- `manifest`: list of live segment ids (no duplicates) + monotonically
  increasing `seq` + previous manifest hash + recovery-wrapped CRK + app binding, plus
  `auth_tag` = HMAC-SHA256(`manifest-auth` key, canonical bytes of
  `{seq, prev, segments, recovery_wrapped_crk, binding}`), the whole AEAD-sealed with the
  `manifest` key. The `manifest-auth` key is derived from the CRK and is not
  kept with the `manifest` key: whoever holds only the `manifest` key (the
  vault's background replication) can read `seq`/segment ids and copy sealed
  manifests, but cannot forge one (e.g. reseal a subset of segments at a
  higher `seq`). Opening the container and restore verify both; replication
  copies sealed manifests unchanged and never needs `manifest-auth`. The vault
  keeps the highest `seq` it has written to each replica; a replica serving a
  lower one is reported as rollback and automatically repaired (the current
  manifest is written again); restore never picks a manifest below the
  caller's mark.
- Compaction rewrites live records into new segments and drops old ones.
- Writes: a commit writes the pending batch as new segment(s) and then a new
  manifest (compare-and-swap). The app's
  "persist before ack" maps to a vault commit acknowledgement.

### Backends

- Trait `ContainerBackend` (list/get/put_if_absent/put_if_match/delete;
  `get_range` is future work, not in V1) with: local directory (Android app
  storage), S3 (SigV4, conditional writes `If-None-Match`/`If-Match` for the
  manifest), WebDAV (`If-Match` ETags). Credentials stay in the vault (encrypted with a Keystore key).
- Replication: asynchronous, segment-first then manifest; status visible in
  the vault UI; failures retried with backoff (by the caller: V1's
  `replicate_once` is one idempotent pass and reports per-replica errors).

### Pairing and sessions (V2)

- The web app shows a QR: relay URL, mailbox id (random 128 bit), ephemeral
  X25519 public key, protocol version. Vault scans it.
- Noise XX handshake over the mailbox; both sides show a 6-digit
  short-authentication string; the user confirms on the phone **and in the
  browser** ("Codes match"; as built, V2a fix wave: the browser goes Active only
  after both, so a holder of the QR text posing as the vault cannot be
  continued by click-through; a vault that finds the mailbox taken (HTTP 409)
  warns that the code was already used). The vault then
  records the session (device name, time) in its session log. The browser
  generates a fresh key pair per session and forgets it at session end.
- Each session: browser asks to open → vault shows "Allow <device>?" →
  biometric → session key derived from the Noise channel; session expires on
  disconnect, lock, timeout (default 12 h) or revocation from the vault.
- Exactly one active session; a new one ends the old one (with a notice).
- Browser side: a `RemoteBackend` with a RAM write-back cache; writes are
  batched and committed before the app acknowledges anything derived from them;
  reads go to the vault. The browser persists nothing.
- Relay: new route `GET /mailbox/<id>` pairing exactly two WebSockets,
  binary only, same style of limits and timeouts as the app's other relay routes, mailbox expires after 60 s
  unpaired.

## Constraints

- Browser APIs: no mDNS or raw sockets, LAN HTTPS blocked by certificates and
  Local Network Access prompts, WebRTC only in window context (V3 moves
  transport to the shell and relaxes `webrtc 'block'` only there).
- Android: a foreground service keeps the vault reachable while a session is
  active; Doze can still delay it — show its state in the web app.
- Latency: every store operation crosses the network; batching is mandatory.
- MIT licensed; no Google Play Services dependency (FCM not required).

## Known limits to state to the owner

- Vault offline → web app unusable (by design).
- Recovery key lost and phone lost → data lost (by design).
- A compromised active browser session sees what it decrypts while the session
  lasts; it cannot keep access after revocation.
- The relay and replica providers see timing and sizes (padded), never
  content.
- Single writer: a second vault device on the same replicas is not supported
  (multiple writers are a non-goal). An old phone with replica credentials
  that still holds the vault could overwrite newer data on a replica that it
  last saw in sync with it.

## Calls made in the design review (controller's, owner may override)

- **Multiple replicas** may be configured (e.g. S3 + NAS); each tracks its own
  `seq`. Commits never wait for replication. Restore reads all reachable
  replicas and takes the highest valid manifest chain; a rollback on restore
  is only detectable when at least one replica is current — documented limit.
- **Acked-but-unreplicated state** lost with the phone is accepted: after
  restore, the app must be able to recover from state that is slightly older
  than what it last acknowledged. Documented limit.
- **CRK in RAM** only while a session is approved or the vault is unlocked in
  the foreground service; wiped on lock/session end. Uploading sealed segments
  and manifests needs no data key: replication uses only the manifest subkey
  (lets a holder read segment ids and seq and copy sealed manifests, never
  data, and not forge a manifest: that needs the CRK-derived `manifest-auth`
  key, which is not kept with it), which the vault keeps under the no-auth
  Keystore key so replication runs while locked; commits and compaction run
  only while unlocked.
- **Local container** lives in app-private storage (`noBackupFilesDir`, no
  Android auto-backup). Replica credentials are wrapped by a separate Keystore
  key without per-use auth (requires an unlocked device) so retries can run in
  the background.
- **Conditional writes** are required from replica providers (S3 `If-Match`/
  `If-None-Match`, WebDAV ETags); unsupported providers are refused at setup.
- The **RemoteBackend wire protocol and Noise transport framing** are owned
  by the V2 work and written as a protocol doc in `docs/protocol/vault.md`.
- Manifest length is padded to buckets like segments.

## V2 load notes

V1 is correct under any call pattern but not cheap under every one. The V2
vault and `RemoteBackend` must respect:

- **Commits must be batched.** Every commit writes at least one new segment
  (smallest bucket 64 KiB, however small the change) and a new sealed
  manifest (at least 4 KiB), and lengthens the manifest by one 16-byte id.
  Committing per incoming message or per key would multiply storage, upload
  and listing cost; batch writes and commit once per acknowledgement window.
- **Replica cost is at least 64 KiB per commit** (one minimum-size segment,
  plus the re-uploaded manifest), on every replica, over mobile data too.
- **An automatic compaction policy is required.** Nothing compacts by itself
  in V1. Each commit adds a segment; replica listings refuse more than
  `MAX_LIST_KEYS` = 100 000 keys (a manifest holds at most 1 000 000 ids),
  after which replication to that replica fails. The vault must compact
  (while unlocked) at a segment-count threshold well below that (e.g. a few
  hundred to a few thousand live segments), and also when dead bytes dominate.
  *As built (V2a):* `VaultServer` checks `CompactionPolicy` after every
  session ends, under the container lock and only while no session is active
  and nothing is staged (never on a commit path): it runs `Container::compact`
  at 64 listed segments, or at a dead-record-bytes ratio of at least 50 % once
  8 or more segments exist (`Container::stats`). `vault-dev` also runs the
  check at clean exit before saving the high-water mark. Compaction publishes a
  later manifest seq, so the high-water mark only moves forward; replication
  deletes the superseded segments on replicas on its next pass.
