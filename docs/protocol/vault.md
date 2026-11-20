# Vault protocol v1

Wire protocol between an app's browser client and a vault (the Android app
later, a dev vault now), carried through a relay mailbox. The browser keeps no
user data; it reads and writes all state in the vault's encrypted container
through request/response messages. Reference implementation of the message
types, QR offer and chunking: the `vault-proto` crate. Vault-side session
server (mailbox dial, responder, approval, request serving): the `vault-server`
crate. Browser side (pairing lifecycle, write-back store): the `vault-client`
crate. Design context: `docs/design/vault.md` ("Pairing and sessions (V2)") and
`docs/superpowers/specs/2026-11-19-vault-split-design.md` (app binding, versions).

## Roles and transport

- Browser: Noise initiator, fresh static key pair per session, discarded at
  session end.
- Vault: Noise responder, fresh random static key per pairing, never stored.
  Nobody pins it: the SAS authenticates every session. The vault can pair while
  its container is locked; it unlocks only when the user approves.
- Relay: `GET /mailbox/<32 hex chars>` upgrades to WebSocket (binary frames
  only) and pairs the first two connections on that mailbox id (128-bit random,
  chosen by the browser). A mailbox expires 60 s after creation if unpaired.
  A browser's `Origin` must be on the relay's allowlist (HTTP 403 otherwise;
  an empty allowlist admits no browser); requests without `Origin` (native
  vaults) are admitted. Limits and timeouts are the relay's configuration
  (`vault_mailbox::MailboxConfig`; `vault-relay` flags; deployment notes in
  `docs/integration.md`, "Running the relay"). The relay forwards bytes and
  does not parse them.
- One Noise transport message per WebSocket binary frame.

## QR pair offer

```
vault-pair:v1:<base64url-no-pad( postcard{ version: u8, relay_url, mailbox_id[16], browser_eph_pub[32], app_id, origin } )>
```

- `version`: the protocol version, `1` (`vault_proto::PROTOCOL_VERSION`). It is
  the first wire field (postcard writes a `u8` as one raw byte), so a decoder
  reads it before anything else and refuses any other value as
  `UnsupportedVersion(v)`, distinct from a malformed payload. Encoding an offer
  with another version is refused too.
- `relay_url`: string, at most 512 bytes. `https://` or `wss://`; `http://` or
  `ws://` only when the host is loopback (`localhost`, `127.0.0.1`, `[::1]`).
  No userinfo; an explicit port must be numeric, 1-65535; no backslash, control
  or non-ASCII bytes. Validated on encode and on decode.
- `browser_eph_pub`: the browser's ephemeral X25519 public key for this session
  (its Noise static `s` in XX).
- `app_id`: string, the app the browser runs (see "App binding").
- `origin`: string, the page's web origin in canonical form (see "App binding").
  Both are validated on encode and on decode (`BadAppId`, `BadOrigin`); a
  non-canonical origin is refused, not normalized, by the decoder.
- Any other prefix (including a different version) is rejected.

## App binding

A vault is bound to one app and one web origin. The pair is set when the
container is created (`vaultctl init --app <id> --origin <url>`), stored in the
container header and in every manifest (covered by the manifest `auth_tag`, see
`docs/vault/README.md`), and never changes; replication and restore copy it with
the header.

- `app_id` matches `^[a-z0-9][a-z0-9.-]{0,63}$` (1 to 64 bytes).
- `origin` is `scheme "://" host [":" port]` with nothing after the authority:
  no path (not even `/`), no query, no fragment, no userinfo. The scheme is
  `https`, or `http` only for the loopback hosts `localhost`, `127.0.0.1` and
  `[::1]`. The host is ASCII: a DNS name of letters, digits, `-` and `.` (an
  internationalized name must be written in punycode, `xn--...`), an IPv4
  address, or a bracketed IPv6 address. At most 300 bytes.
- Canonical form, the only form offers, prologues and containers carry (they are
  compared byte for byte): scheme and host in lower case, the port without
  leading zeros, and the scheme's default port (`:443` for https, `:80` for
  http) left out. `https://App.Example:443` becomes `https://app.example`.
  Clients normalize what the page gives them (`location.origin` already is in
  this form) before building the offer; `vaultctl init` stores the normalized
  form. Reference: `vault_proto::binding::{validate_app_id, normalize_origin,
  validate_origin}`.
- After the handshake, before reading any request, the vault compares the
  offer's `app_id` and `origin` with its binding. On a mismatch it sends
  `Notice::WrongApp` and closes the session: no approval prompt is shown, no
  `OpenSession` is read and no data is touched. The browser reports the session
  as `closed(wrong app)` (`CloseReason::WrongApp`).
- The binding compared is that of the container the vault holds at the moment
  of the check, never a value taken before the handshake. A vault that can swap
  containers (lock, then unlock another one) repeats the comparison immediately
  before it shows the approval prompt and again, atomically with becoming the
  active session, before it answers `Approved`. A mismatch at either point is
  handled the same way: `Notice::WrongApp`, the session ends, the prompt is not
  shown (or its answer is ignored) and no data is touched.
- The approval prompt shows the app id, the origin and the relay host along with
  the device name and the SAS.

## Handshake

Pattern `Noise_XX_25519_ChaChaPoly_SHA256`.
Prologue (both sides, byte concatenation):

```
"vault/v1" || version (1 byte) || u16_be(len(app_id)) || app_id
           || u16_be(len(origin)) || origin || mailbox_id (16 bytes) || browser_eph_pub (32 bytes)
```

`app_id` and `origin` are the UTF-8 (ASCII) bytes of the offer's fields, each
prefixed by its length as a big-endian `u16`. The vault builds the prologue from
the scanned QR; the browser from its own values. If the QR was altered in transit
(different version, app, origin, mailbox or key), the prologues differ and the
handshake fails.

**The vault MUST check that the Noise remote static key received in the
handshake equals `browser_eph_pub` from the scanned QR, and abort otherwise.**
The prologue only hashes `browser_eph_pub`; it does not bind the key the
initiator actually authenticates with, so without this check a party that
copies the QR values could handshake with a different static key.

### Short authentication string (SAS)

After the handshake both sides compute:

```
okm = HKDF-SHA256(ikm = handshake_hash, salt = none, info = "vault/v1/sas", L = 4)
sas = (u32_be(okm) mod 1_000_000), zero-padded to 6 decimal digits
```

Both show it, and the user confirms the match on **both** sides: on the phone
(the vault's approval) and in the browser ("Codes match"). A mismatch, or a
browser code with no code on the phone, means the user aborts ("Cancel" in the
browser, deny on the phone) and nothing is stored.

## Session states

```
Offer -> Handshake -> AwaitApproval -> Active -> Closed
```

| State | Meaning | Leaves on |
|---|---|---|
| Offer | Browser shows the QR and waits on its mailbox | vault connects (-> Handshake); 60 s expiry (-> Closed) |
| Handshake | Noise XX messages 1-3 as raw WS frames | success (-> AwaitApproval); any failure (-> Closed); offer not for this vault's app and origin: `Notice::WrongApp` (-> Closed, reason `wrong app`) before any prompt |
| AwaitApproval | SAS shown on both sides; the browser sent `OpenSession`; the vault shows "Allow <device>?" and asks for biometrics; the browser asks "Codes match?" | vault side: `Approved` (-> Active). Browser side: `Approved` **and** the local "Codes match", in either order (-> Active); `Denied`, timeout or the local "Cancel" (-> Closed, reason `denied`) |
| Active | Store requests are served | `Close`, disconnect, lock, timeout (default 12 h), revocation, replacement by a newer session (-> Closed) |
| Closed | Keys discarded | terminal |

Requests other than `OpenSession` outside Active are answered with
`Error{NotApproved}`. Exactly one session is active per vault; a new session
ends the old one, which receives `Notice::SessionTerminated`.

## Framing and chunking

- Each Noise transport message carries one frame; frame plaintext is at most
  60 KiB (61440 bytes). The 16-byte ChaChaPoly AEAD tag is added on top, so the
  WebSocket frame is at most 61456 bytes; the relay's frame limit must allow
  this.
- Frame plaintext = `postcard{ msg_id: u64, index: u32, last: bool }` (at most
  16 bytes) followed by payload; payload at most 61440 - 16 = 61424 bytes.
- An application message (postcard-encoded `Request`, `VaultMessage`) is split
  into chunks with a per-direction strictly increasing `msg_id` (starting at 0), `index` 0, 1, 2, ...,
  and `last = true` on the final chunk. An empty message is one empty last
  chunk.
- Receiver: one message in flight per direction, and a new message must have a
  `msg_id` greater than that of every earlier message in the same direction
  (replayed or reordered messages are rejected). A chunk with another `msg_id`
  before `last`, an `index` that is not the next expected, a first chunk with
  `index != 0`, more than 600 chunks, or more than 32 MiB reassembled is a
  protocol error: close the session.

## Browser client behaviour

Implemented by the `vault-client` crate, which the embedding app wraps (a write-back
`RemoteBackend` over the vault's key-value interface, plus the pairing lifecycle
`VaultSession`);
the browser connects to `{relay origin}/mailbox/<hex>` with
its own WebSocket.

- The app passes its `app_id` and the page origin (`location.origin`) when it
  starts a pairing (`VaultSession::pair`); the client validates and normalizes
  them and puts them in the offer. `Notice::WrongApp` ends the session with
  reason `wrong app`.
- Before `Approved` and the user's local confirmation (the app's
  `confirm_sas`) the browser sends only `OpenSession`, `Ping` and
  `Close`, and links/connects nothing; `cancel_pairing` ends the session with reason `denied` and wins at any point
  before `active`: also after "Codes match" and after the vault's `Approved`
  (`active` is never reported; no data request is sent before both approvals,
  but the app's activation step runs with a live backend, so its own requests
  may reach the vault before the cancel, which apps should keep read-only). After `active` it is
  refused; `lock` ends an active session. The state is
  reported to the UI as `pairing`, `awaitingApproval` (until both
  confirmations), `active`, then `closed(reason)`.
- It sends `Ping` every 30 s from the end of the handshake on.
- Store requests go through a RAM write-back cache: reads hit the cache, then
  the vault (`Get`, `ScanPrefix`; scans merge the unsent writes);
  `put_if_absent` is answered locally when the cache knows the key, otherwise
  by the vault's `PutIfAbsent`. Writes are buffered and sent at `flush`: first
  `Clear` (if any), then each key's latest `Put`/`Delete`, and only when all were
  answered `Ok`, `Commit`; the flush resolves on `Committed`.
- Incoming app messages (for example queued server events) are handled in batches (the already queued ones,
  up to 64 within 200 ms), then one flush, then they are acked and their effects
  shown. If the commit fails nothing is acked (the server redelivers). Any other
  state change (linking, uploads, sends) is committed before they are reported as done.
- On disconnect, `SessionTerminated`, `Locked`, a failed commit or a user lock,
  the cache and all unsent writes are discarded, the app's own server connection is
  closed and `closed(reason)` is reported. Nothing is written to browser
  storage at any point.
- Keys are stored as `namespace ‖ 0x00 ‖ key` with postcard values (the
  vault encrypts at rest); the browser holds no storage key and has no
  passphrase.

## Messages

Browser to vault, after reassembly: `Request{ id: u64, body }`. `id` is chosen
by the browser and echoed in the response; requests are answered in order.

| RequestBody | Fields | Success response | Notes |
|---|---|---|---|
| OpenSession | device_name | Approved / Denied | first message; requires user approval on the vault |
| Get | key | Value(Option<bytes>) | |
| Put | key, value | Ok | staged until Commit |
| PutIfAbsent | key, value | Bool | true if written |
| Delete | key | Ok | staged until Commit |
| ScanPrefix | prefix | Entries(Vec<(key, value)>) | sorted by key |
| Commit | - | Committed{ seq } | returns after `Container::commit` finished |
| Clear | - | Ok | removes all entries, staged until Commit |
| Close | - | Ok | vault then ends the session |
| Ping | - | Ok | keepalive; allowed in every state, also before approval; touches no data |

**Keepalive.** While Active, the browser MUST send `Ping` at least every 30 s
(the relay closes mailboxes idle for longer than its idle timeout). `Ping` is
also allowed in AwaitApproval (answered `Ok`; the session stays in
AwaitApproval). It does not count as a data request: it does not reset a vault
idle timeout.

**Approval.** The vault shows one approval prompt at a time; further sessions
wait for it (their approval timeout starts once their prompt is shown). At most
2 sessions may wait for approval at once; an `OpenSession` beyond that is
answered `Denied` and the session is closed.

**Undecodable requests.** A request that fails to decode is answered
`Error{TooLarge}` (size cap) or `Error{BadRequest}` with `id = 0`, then the
session is closed. Frames that fail Noise decryption close the session without
an answer.

These mirror `KvBackend`. Reads see the session's staged writes. `Clear` has no
dedicated storage primitive: the vault implements it as a scan of all keys
followed by deletes.

**Scope of `Clear`.** A vault is bound to one app and origin (see "App
binding"), so its whole key space is that app's data: `Clear` erases the bound
app's data, nothing else, and takes effect at `Commit` like any write. An app
uses it to implement "unlink" (erase this app's vault). Replicas are not
touched by the erase itself: they receive the new, empty state at the next
replication, but older replica snapshots keep the old data until the replica's
own retention expires them (the vault does not control that retention).

Decoding is strict: after the postcard value there must be no trailing bytes
(also for the QR payload), and the field caps below are enforced on decode.
Size violations are reported as `TooLarge`, bad field content (control
characters in `device_name`) as `BadRequest`.

Vault to browser: `VaultMessage::Response(Response{ id, body })` or
`VaultMessage::Notice(..)`.

| ResponseBody | Meaning |
|---|---|
| Ok | done, no value |
| Value(Option<bytes>) | result of Get |
| Bool(bool) | result of PutIfAbsent |
| Entries(Vec<(bytes, bytes)>) | result of ScanPrefix |
| Committed{ seq } | state durable in the container at sequence `seq` |
| Approved / Denied | answer to OpenSession |
| Error{ code, msg } | request failed; `msg` is for logs only |

| Notice | Meaning |
|---|---|
| SessionTerminated{ reason } | vault ended the session (replaced, revoked, timeout) |
| Locked | vault locked; session is over |
| WrongApp | the offer's `app_id` or `origin` is not this vault's binding; sent right after the handshake, before any approval prompt (or, if the vault's container changed meanwhile, in place of the prompt or of `Approved`); session is over |

### Error codes

| Code | Use |
|---|---|
| NotApproved | request outside an approved session |
| Conflict | write conflict or precondition failure |
| Storage | container or replica I/O failure |
| TooLarge | key/value/response beyond limits |
| BadRequest | malformed or unexpected message |
| Locked | vault is locked |
| Internal | anything else |

## Limits

| Limit | Value |
|---|---|
| Frame plaintext | 60 KiB |
| Chunk payload | 61424 bytes |
| Message | 32 MiB, 600 chunks |
| relay_url | 512 bytes (no `\\`, controls or non-ASCII; port 1-65535) |
| device_name | 64 bytes UTF-8, no control characters |
| app_id | 64 bytes, `^[a-z0-9][a-z0-9.-]{0,63}$` |
| origin | 300 bytes, canonical form (see "App binding") |
| key / prefix | 1 KiB |
| Single value | 16 MiB |
| Entries response | 32 MiB total of keys + values |
| Mailbox unpaired lifetime | 60 s |
| Session timeout (default) | 12 h |
| Active sessions per vault | 1 |

## Threat notes

- The relay sees connection timing, frame sizes and counts only; all content is
  inside Noise. It can drop or delay traffic (denial of service) but not read or
  alter it undetected.
- QR tampering (swapped mailbox, `browser_eph_pub`, `app_id` or `origin`)
  breaks the prologue and fails the handshake; an attacker who instead substitutes the whole offer with
  their own is caught by the SAS comparison, which the user must perform.
- The pairing text is a short-lived bearer secret: anyone who reads it within
  its 60 s life (screen share, camera, an untrusted QR app or clipboard sync) can
  take the second place in the mailbox before the user's vault and complete the
  handshake as "the vault" (prologue and remote-static checks pass for them).
  Their vault can answer `Approved` itself; the user's real vault gets HTTP 409
  from the relay. Defence: the confirmation is two-sided. The browser goes
  Active only after `Approved` *and* the user's "Codes match" in the browser,
  which the user gives only when their own phone shows the same code; an
  honest vault that got 409 shows no code and warns that the pairing code was
  already used. Read the QR only with the phone's own camera or a trusted
  scanner and never share the text.
- **The origin is a declared, pinned label, not an authenticated claim.** The
  page supplies it (`location.origin` in the browser; native and test clients
  pass any origin they like) and the vault cannot verify it: a malicious page
  can declare another site's origin. Pinning it at container creation and
  refusing mismatches (`WrongApp`) keeps apps apart and catches mistakes (the
  wrong app or a stale deployment pairing with the vault). Protection against
  a malicious page still rests on the user checking the origin, the relay host
  and the SAS on the phone before approving. The same holds for `app_id`.
- The app binding is plaintext in the container header (an observer of a
  replica learns which app and origin the vault serves); the authenticated
  manifest carries the same values, so a changed header is refused on open.
- The SAS is 6 digits (about 20 bits): it protects against an online
  man-in-the-middle in a single pairing attempt, not against offline guessing,
  because each attempt needs a fresh session and a user confirmation.
- Both static keys are per session and forgotten at session end; neither
  identifies a party across sessions (the SAS comparison does that job).
- A compromised active browser sees what it reads while the session lasts and
  loses access on revocation or session end.

## Versions and labels

- Protocol version `1`: the `version` field of the offer, and the `v1` in the QR
  prefix and in every label below. A new protocol version changes all three.
- Container format version `1`: stored in the container `header` (see
  `docs/vault/README.md`, "Format v1"); an unknown version is refused.
- Labels (ASCII, no terminator):

  | Use | Label |
  |---|---|
  | QR prefix of the pair offer | `vault-pair:v1:` |
  | QR prefix of the recovery key | `vault-recovery:v1:` |
  | Noise prologue label | `vault/v1` |
  | SAS HKDF info | `vault/v1/sas` |
  | Container subkey HKDF infos | `vault/v1/enc`, `vault/v1/mac`, `vault/v1/manifest`, `vault/v1/manifest-auth`, `vault/v1/replica-id` |
  | Recovery wrap key HKDF info | `vault/v1/recovery` |
  | Recovery checksum domain (SHA-256 prefix) | `vault/v1/recovery-check` |
  | Wrapped-CRK AAD (followed by the wrap version byte) | `vault/v1/crk-wrap` |
  | Manifest `auth_tag` domain (HMAC input prefix) | `vault/v1/manifest-auth-tag` |

  Containers and pairings made with the labels used before the split do not
  work with these; there is no compatibility path. Opening a container created
  before the split fails with `unsupported container format (created before
  vault v1)` (its header has no `vault` marker), not with a decryption error.

## Known answers

Computed independently of the Rust code by `tools/kat/kat.py` (Python,
`cryptography` package) from the definitions above and in
`docs/vault/README.md`, and pinned in the Rust tests (`sas_kat_v1` in
`vault-proto`, `subkey_kats_v1`, `recovery_checksum_kat_v1` and `known_answers`
in `vault-container`). Recompute with:

```sh
python3 -I tools/kat/kat.py
```

| Input | Value |
|---|---|
| SAS, `handshake_hash = [0; 32]` | okm `83f0d811`, SAS `599249` |
| `enc`, CRK `[0x11; 32]` | `fadf21e22aeeb4991efd0af527ddfdfe15a9d07c797d49b5f93e239f98a085d5` |
| `mac`, CRK `[0x11; 32]` | `95386a492d73b845448b54e6f46de466c1a76a74d3b04103d7e9401a95f59fd4` |
| `manifest`, CRK `[0x11; 32]` | `8348e40177efe9bf9096df8d9f037acbe54dca8d279b5f068ac45ad14e71b16a` |
| `manifest_auth`, CRK `[0x11; 32]` | `f39f7075471082f238686355f684641795ed0e576e04a86e777fe5843c9d36e4` |
| `replica_id`, CRK `[0x11; 32]` | `88c1e18db40d7125c8cb58e8f44d8ff7a79989b9d5f577518054dd82379317c7` |
| recovery wrap key, recovery key `[0x11; 32]` | `89b2113a8f028550ac5c7f8156bd26d761bb0b75af35083109ca3d303c87be37` |
| recovery checksum, recovery key `[0x11; 32]` | `52c5e2d43fcec705` |
| recovery key text, recovery key `[0x11; 32]` | `CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIV-FRPC-2Q74-5RYF` |

The script also prints the values for the older test inputs (CRK `1..=32`,
recovery key `0xa0..=0xbf`) used by `known_answers`.
