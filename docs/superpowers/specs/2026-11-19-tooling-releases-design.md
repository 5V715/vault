# Tooling releases and install scripts — short design

Status: approved by owner 2026-11-19 (signing option 1).

## Intent

Prebuilt `vaultctl`, `vault-dev` and `vault-relay` for the major desktop and
server platforms, plus install scripts that verify a release before installing
it. While `5V715/vault` is private, downloads go through the GitHub CLI (`gh`)
with the user's login.

## Decisions

- **Targets:** `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`
  (static, cargo-zigbuild), `x86_64-apple-darwin`, `aarch64-apple-darwin`
  (native macOS runners), `x86_64-pc-windows-msvc`.
- **Archives:** one per target, `vault-<version>-<target>.tar.gz` (`.zip` on
  Windows), each holding all three binaries, `LICENSE` and `README.md`.
  `SHA256SUMS` lists every archive.
- **Trigger:** a tag `v*` makes the workflow build every target and create a
  **draft** GitHub release with the archives and `SHA256SUMS`. A draft is never
  installable.
- **Signing (owner option 1):** the owner signs `SHA256SUMS` offline with an
  OpenSSH Ed25519 key (`ssh-keygen -Y sign -n vault-release`); the key never
  touches GitHub. `scripts/sign-release.sh <tag>` downloads `SHA256SUMS` from
  the draft, shows it, signs it, uploads `SHA256SUMS.sig` and publishes the
  release. *My call:* OpenSSH signatures rather than minisign, because
  `ssh-keygen` is already on Linux, macOS and Windows 10+, so the install
  scripts need no extra tool.
- **Trust root:** `keys/release.pub` and the derived `keys/allowed_signers`
  (principal `release@vault`) are committed. The install scripts embed the
  same allowed-signers line, so a downloaded script can't be paired with a
  swapped key file. The owner creates the key; until then the release workflow
  refuses to run and the scripts refuse to install.
- **Install:** `scripts/install.sh` (Linux and macOS) and `scripts/install.ps1`
  (Windows):
  1. Check `gh` is installed and logged in.
  2. Resolve the version (`--version vX.Y.Z`, or the latest published release).
  3. Download the archive, `SHA256SUMS` and `SHA256SUMS.sig` into a temporary
     directory with `gh release download`.
  4. Verify the signature with `ssh-keygen -Y verify`, then the archive hash
     against `SHA256SUMS`.
  5. Only then extract and install: to `~/.local/bin` (or `--dir`) on Unix, or
     to `%LOCALAPPDATA%\vault\bin` on Windows, added to the user PATH.
  Any failure aborts with nothing installed.
  *(2026-11-27: the repository is public, so steps 1 and 3 are now plain HTTPS from
  github.com with curl/wget or System.Net.Http, no `gh` and no login; the latest
  release comes from the unauthenticated API. Trust model unchanged. See
  `docs/install.md`.)*
- **Windows:** `--passphrase-fd` stays Unix-only and is not shown in Windows
  `--help`; the interactive prompt and `--passphrase-file` work everywhere.
- **Honest limit:** the offline signature protects the release assets and the
  download path against tampering after the build. Against a GitHub account
  takeover it helps only users whose install script predates it, or who compared
  the embedded key with a fingerprint the owner publishes outside the repository
  (the script and its trust root come from the same repository). It does not stop
  a rollback to an older signed release; `--version` pins (amended after the final
  review). It does not protect against a compromised CI producing bad
  binaries that the owner then signs. Reproducible builds would close that;
  they are out of scope here.

## Approaches considered

1. Owner offline signing (picked).
2. GitHub artifact attestations: automatic, but private repos need GitHub
   Enterprise. Revisit once the repo is public (they can be added alongside).
3. Both: deferred until the repo is public.
