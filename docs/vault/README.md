# Vault V1: encrypted, replicated container

V1 is the storage layer of the vault: a single encrypted container that holds all
persistent user data, stored as opaque objects (`header`, `manifest`, `seg/<id>`) and
copied to one or more replicas (S3-compatible storage or WebDAV such as Nextcloud).
The code lives in the `vault-container` crate; `vaultctl` is a command-line tool that
exercises it before the Android vault exists. Design: `docs/design/vault.md`.

## Threat model (summary)

- A storage provider (or anyone with the bucket / WebDAV credentials) sees only
  ciphertext, object sizes, and timing. Keys, values and key names are encrypted or MACed.
- Tampering is detected (AES-GCM, authenticated manifest); a replica that serves an
  older manifest than the one last written there is reported as a **rollback** and
  automatically repaired (the current manifest is written to it again); replicas
  holding different valid manifests are reported as **diverged** and are never
  overwritten without explicit confirmation.
- The manifest carries a second authenticator keyed from the root key, so the manifest
  subkey alone (which replication uses) lets a holder read sequence numbers and segment
  ids and copy manifests, but not forge one.
- All keys derive from one 32-byte container root key (CRK). The **recovery key**
  (64 base32 characters) unwraps it; with the recovery key alone, the whole container
  can be restored from any replica.
- Not covered: a compromised device while the vault is unlocked, traffic analysis,
  providers that ignore conditional writes (these are rejected by a probe when adding a replica).

## Build

```sh
cargo build -p vault-container --release --bin vaultctl
# binary: target/release/vaultctl
```

## Quick start

Every command takes `--dir D`, the local container directory (must be empty or new for
`create`). The passphrase protecting the CLI's secrets file is prompted for
(recommended; an empty passphrase is refused). For scripts, read it without echo and
export it for the session:

```sh
read -rs VAULTCTL_PASSPHRASE; export VAULTCTL_PASSPHRASE
```

Never type the secret into a command line or `export VAR='...'`: it would land in your
shell history, and exported variables are visible to every child process you start (and
to same-user processes via `/proc`). The same applies to `VAULTCTL_S3_SECRET_KEY`,
`VAULTCTL_WEBDAV_PASSWORD` and `VAULTCTL_RECOVERY_KEY`; each is also prompted for when unset.

```sh
vaultctl --dir ~/vault create          # prints the recovery key + QR code once
vaultctl --dir ~/vault put greeting hello
vaultctl --dir ~/vault get greeting
vaultctl --dir ~/vault scan gr          # key<TAB>value; add --base64 for binary values
```

`put` commits immediately (each invocation is its own process); `commit` prints the
current sequence number.

### Replica: S3 with MinIO

```sh
docker run -d --name minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin123 \
  -v minio-data:/data minio/minio server /data
# create a bucket "vault" (e.g. with `mc mb`), then:
# the S3 secret key (minioadmin123 here) is prompted for
vaultctl --dir ~/vault replica add s3 --id minio \
  --endpoint http://127.0.0.1:9000 --region us-east-1 --bucket vault \
  --prefix my-vault --access-key minioadmin --path-style
vaultctl --dir ~/vault replicate
```

`http://` is accepted for loopback hosts only; for other hosts use `https://`, or
`--allow-insecure-http` if you accept unencrypted transport (requests are signed, the
ciphertext is not hidden from the network). A private CA can be trusted with `--ca-pem FILE`.

### Replica: Nextcloud (WebDAV)

Create an app password in Nextcloud (Settings, Security, "Create new app password")
and an empty folder, e.g. `vault`. The URL is the folder's WebDAV URL:

```sh
# the app password (xxxxx-xxxxx-...) is prompted for
vaultctl --dir ~/vault replica add webdav --id nextcloud \
  --url https://cloud.example.com/remote.php/dav/files/alice/vault --username alice
vaultctl --dir ~/vault replicate
```

Adding a replica first runs a conditional-write probe; if the server ignores
`If-Match` / `If-None-Match`, the replica is refused and nothing is saved.
The URL must not contain `user:password@`; it is rejected.
`vaultctl replica list` shows replicas and their last sync (never secrets).

### Restore with the recovery key

Into a new or empty directory, from one replica: the CLI reads exactly the replica
given on the command line (the library's `restore` can read several and picks the
highest valid manifest; the CLI does not expose that yet). The source is probed for
conditional writes first, like `replica add`. Nothing is created in the target
unless the restore verifies; on failure it is left untouched.

```sh
# the recovery key is prompted for
vaultctl --dir ~/vault-restored restore s3 --id minio \
  --endpoint http://127.0.0.1:9000 --region us-east-1 --bucket vault \
  --prefix my-vault --access-key minioadmin --path-style
```

The report names the replica restored from and any stale, diverged, unreachable or
broken replicas. The restored directory gets a new secrets file (protected by the
passphrase you provide) and keeps the source as a replica.

### Divergence

If `replicate` reports `DIVERGED`, the replica holds a manifest this container does not
descend from (e.g. another device wrote to it). Nothing is overwritten. To keep this
container and overwrite the replica, run `vaultctl --dir D replica adopt <id>`. It shows
what the replica held when `replicate` last reported it (manifest seq and a short hash,
or "foreign container header hash") and asks you to type the replica id again. The
confirmation applies only to exactly that state: if the replica changed since, the next
`replicate` reports it diverged again and nothing is overwritten. Then run `replicate`;
it prints `ADOPTED`, and exits 2 if the replaced manifest was older than the last one
written there (a rollback). To keep the replica's content instead,
restore from it into a new directory.

## Test vault on your laptop (vault-dev)

`vault-dev` is a development stand-in for the Android vault: it serves your container
to the web app over a relay mailbox, so you can pair, approve, open a session and read and write
state end to end before the Android app exists. It is part of the `vault-server` crate.

```sh
cargo build -p vault-server --release --bin vault-dev
# binary: target/release/vault-dev

vaultctl --dir ~/vault create                 # once; remember the passphrase
vault-dev serve --dir ~/vault --pair "signal-web-pair:v1:..."
```

1. Open the web app. The "Connect to your vault" screen shows a QR code.
2. Get the pairing text (`signal-web-pair:v1:...`) from it. **Gap:** the web app has no
   "copy pairing text" button yet, only the QR. Read the QR with your phone's built-in
   camera (or a scanner you already trust), copy the text and paste it between the
   quotes. **Treat the text as a short-lived secret:** whoever has it within its 60 s
   life can connect to the mailbox before your vault and pose as it toward the browser.
   Do not use third-party QR apps or clipboard sync you do not trust, do not share
   it, do not show the QR on a shared screen.
3. Run the command within about 60 seconds of the QR appearing: the relay drops an
   unpaired mailbox after 60 s. Enter the passphrase when prompted (no echo).
4. The terminal prints the device name, the relay host from the pairing text, a 6-digit
   code and a prompt number, for example `[#1] Code: 123456`. **Approve only if the code
   matches the one the browser shows and the relay is the address of the page you
   opened**: type `y 1` (the number is part of the answer) to allow, `n` to deny. End of
   input denies. A mismatch means someone else may be in the middle: type `n`.
5. In the browser, press **Codes match** only if the terminal shows the same code
   (either before or after approving there); otherwise press **Cancel**. The browser
   opens the vault only after both confirmations. If `vault-dev` instead prints
   `WARNING ... already used by another device` (the relay answered HTTP 409: the
   mailbox is taken), someone else got the pairing text first: do **not** press Codes
   match; press Cancel, close the tab and start over.
6. The process exits (code 0) when the session ends (browser disconnect, denial,
   replaced, timeout; it prints `session ended: <reason>`). Run it again with the new
   QR text for the next session. Ctrl-C locks and exits (the high-water mark of the
   last commit is saved; staged, uncommitted writes are dropped).

Options and behaviour:

- `--passphrase-file PATH` or `--passphrase-fd N` read the passphrase for scripts and
  tests (first line, no echo needed). It is never accepted on the command line or in an
  environment variable. Without either, it is prompted for.
- Errors (wrong passphrase, malformed pairing text, unreachable relay, not a
  `vaultctl` directory) print one `vault-dev: ...` line and exit 1.
- The relay URL comes from the pairing text. `https://`/`wss://` anywhere;
  `http://`/`ws://` only for loopback hosts (`localhost`, `127.0.0.1`, `[::1]`), which
  the web app's own offer encoding also enforces. There is therefore no
  `--insecure-dev-relay` flag: a plain-`ws` offer for a non-loopback host cannot be
  produced or parsed. To test against a relay with a private CA, that CA support exists in
  `ServerConfig::extra_ca_pem` but is not yet exposed on this command line.
- The vault's long-term Noise key is created on first use in `D/.vault-dev-identity`,
  sealed under the same passphrase (Argon2id, AES-256-GCM, mode 0600). The
  high-water mark is saved back into `D/.vaultctl` when a session ends.
- Growth and compaction: every commit adds at least one segment file of 64 KiB or
  more under `D/seg/` (the browser commits once per received batch and up to three
  times per sent message), and every session start decrypts all of them. The vault
  therefore compacts automatically after a session ends, and `vault-dev` again at
  clean exit, when either threshold is crossed: **64 segments**, or **at least 50 %
  dead record bytes** (overwritten or deleted) once there are at least 8 segments
  (`vault_server::CompactionPolicy`, constants `COMPACT_*`). Compaction rewrites the
  live state into fresh segments, publishes a new, later manifest (the high-water
  mark moves forward, so anti-rollback is unaffected) and deletes the old segment
  files; replicas drop them on the next `replicate`. It never runs while a session
  is active. So the directory stays within roughly 64 segments plus the live data.

Security caveats (development only):

- The container directory lives on your laptop's disk, protected by the passphrase and
  by the operating system alone; there is no Keystore, no biometric prompt, and an
  unlocked process's memory is readable by anything running as you.
- The approval is a typed answer; the code comparison is the only protection against a
  man in the middle at pairing time, so never approve without matching the code, and
  never press Codes match in the browser unless this terminal showed that code.
- One session is served per run, and only while the process is running; stdin must be
  a terminal you control (anything that can write to it can approve).
- The printed prompt shows the browser-supplied device name escaped; treat it as untrusted.

## Recovery key handling

- It is shown once at `create`, as 16 groups of 4 characters plus a QR code. It cannot be
  displayed again (the CLI keeps a copy only inside its passphrase-encrypted secrets file).
  The recovery key alone restores everything, so keep it offline and private.
- Write it on paper (or use the QR) and keep it offline, apart from the passphrase.
  Anyone with it can read the data; without it, a lost device means lost data.
- Typing is forgiving: case, spaces and dashes are ignored, and a checksum catches typos.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | error (bad arguments, wrong passphrase, I/O or provider failure, non-empty target directory, a replica failed) |
| 2 | a replica diverged, or a rollback was detected |

## Limits

- Single writer: one process at a time commits to a container; concurrent writers
  conflict, they do not merge. The whole index is held in memory.
- Multiple writing devices are a non-goal: with two devices (e.g. an old phone or a
  second copy of the directory) using the same replicas, the older one could overwrite
  newer data on a replica it last saw in sync with itself.
- Recovery key rotation is not implemented yet.
- `.vaultctl` (Argon2id with 64 MiB, t=3, p=1, then AES-256-GCM) holds the recovery key,
  replication state and replica credentials. It is a testing aid for the CLI; the
  Android app keeps its keys in the Keystore instead. Do not use the CLI passphrase file
  as your only protection for real data on a shared machine.
- Secrets passed as flags are visible to other local users; use the environment
  variables or prompts. Environment variables: `VAULTCTL_PASSPHRASE`,
  `VAULTCTL_S3_SECRET_KEY`, `VAULTCTL_WEBDAV_PASSWORD`, `VAULTCTL_RECOVERY_KEY`.
