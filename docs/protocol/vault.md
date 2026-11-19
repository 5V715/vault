# Vault protocol v1

Wire protocol between an app's browser client and a vault (the Android app
later, a dev vault now), carried through a relay mailbox. The browser keeps no
user data; it reads and writes all state in the vault's encrypted container
through request/response messages. Reference implementation of the message
types, QR offer and chunking: the `vault-proto` crate. Vault-side session
server (mailbox dial, responder, approval, request serving): the `vault-server`
crate. Design context: `docs/superpowers/specs/2026-11-18-vault-design.md` ("Pairing and sessions").

## Roles and transport

- Browser: Noise initiator, fresh static key pair per session, discarded at
  session end.
- Vault: Noise responder, long-term static key.
- Relay: `GET /mailbox/<32 hex chars>` upgrades to WebSocket (binary frames
  only) and pairs the first two connections on that mailbox id (128-bit random,
  chosen by the browser). A mailbox expires 60 s after creation if unpaired.
  Other limits and timeouts as for `/tunnel`. The relay forwards bytes and
  does not parse them.
- One Noise transport message per WebSocket binary frame.

## QR pair offer

```
signal-web-pair:v1:<base64url-no-pad( postcard{ relay_url, mailbox_id[16], browser_eph_pub[32] } )>
```

- `relay_url`: string, at most 512 bytes. `https://` or `wss://`; `http://` or
  `ws://` only when the host is loopback (`localhost`, `127.0.0.1`, `[::1]`).
  No userinfo; an explicit port must be numeric, 1-65535; no backslash, control
  or non-ASCII bytes. Validated on encode and on decode.
- `browser_eph_pub`: the browser's ephemeral X25519 public key for this session
  (its Noise static `s` in XX).
- Any other prefix (including a different version) is rejected.

## Handshake

Pattern `Noise_XX_25519_ChaChaPoly_SHA256`.
Prologue (both sides, byte concatenation):

```
"signal-web/vault/v1" || mailbox_id (16 bytes) || browser_eph_pub (32 bytes)
```

The vault builds the prologue from the scanned QR; the browser from its own
values. If the QR was altered in transit (different mailbox or key), the
prologues differ and the handshake fails.

**The vault MUST check that the Noise remote static key received in the
handshake equals `browser_eph_pub` from the scanned QR, and abort otherwise.**
The prologue only hashes `browser_eph_pub`; it does not bind the key the
initiator actually authenticates with, so without this check a party that
copies the QR values could handshake with a different static key.

### Short authentication string (SAS)

After the handshake both sides compute:

```
okm = HKDF-SHA256(ikm = handshake_hash, salt = none, info = "signal-web/vault/v1/sas", L = 4)
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
| Handshake | Noise XX messages 1-3 as raw WS frames | success (-> AwaitApproval); any failure (-> Closed) |
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

Implemented by the browser-side client of the embedding app (a write-back
`RemoteBackend` over the vault's key-value interface, plus the pairing lifecycle);
the browser connects to `{relay origin}/mailbox/<hex>` with
its own WebSocket.

- Before `Approved` and the user's local confirmation (the app's
  `confirm_sas`) the browser sends only `OpenSession`, `Ping` and
  `Close`, and links/connects nothing; `cancel_pairing` ends the session with reason `denied`. The state is
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
- QR tampering (swapped mailbox or `browser_eph_pub`) breaks the prologue and
  fails the handshake; an attacker who instead substitutes the whole offer with
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
- The SAS is 6 digits (about 20 bits): it protects against an online
  man-in-the-middle in a single pairing attempt, not against offline guessing,
  because each attempt needs a fresh session and a user confirmation.
- The browser static key is per session and forgotten at session end; the
  vault's long-term static key identifies it across sessions.
- A compromised active browser sees what it reads while the session lasts and
  loses access on revocation or session end.

## Labels

The QR prefix (`signal-web-pair:v1:`), the HKDF/AAD/Noise-prologue/SAS labels
(`signal-web/vault/v1/...`) and the recovery prefix (`signal-web-recovery:v1:`) still carry the
name of the project this code was extracted from. They are kept byte-identical for now so
existing containers and pairings stay valid, and are scheduled to be renamed to neutral
`vault-...` labels (with explicit version bytes) in a later change.
