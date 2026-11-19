# Installing the prebuilt tools

Each release of `5V715/vault` ships `vaultctl`, `vault-dev` and `vault-relay` prebuilt
for five targets:

| Platform | Target | Archive |
|---|---|---|
| Linux x86_64 (static) | `x86_64-unknown-linux-musl` | `vault-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64 (static) | `aarch64-unknown-linux-musl` | `vault-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `vault-<version>-x86_64-apple-darwin.tar.gz` |
| macOS Apple silicon | `aarch64-apple-darwin` | `vault-<version>-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `vault-<version>-x86_64-pc-windows-msvc.zip` |

`<version>` is the tag without the leading `v`. Next to the archives, `SHA256SUMS` lists
the hash of every archive, and `SHA256SUMS.sig` is the owner's offline OpenSSH signature
over `SHA256SUMS`.

While the repository is private, everything goes through the GitHub CLI
([`gh`](https://cli.github.com/)) with your own login. There is no `curl | sh` path.

## Install (users)

You need `gh`, logged in (`gh auth login`) with read access to `5V715/vault`, and
`ssh-keygen` from OpenSSH 8.2 or later. Linux and macOS ship it. On Windows 10/11 it
comes with the OpenSSH client (Settings > Optional features) or with Git for Windows.

### Linux and macOS

```sh
gh api repos/5V715/vault/contents/scripts/install.sh --jq .content | base64 -d > install.sh
less install.sh            # read it: it is short, and it is what decides what you trust
bash install.sh            # latest release, into ~/.local/bin
bash install.sh --version v1.2.3 --dir /opt/vault/bin
```

The script also needs `tar` and `sha256sum` (Linux) or `shasum` (macOS). It picks the
target from `uname -s` and `uname -m`. Under Rosetta it installs whatever the shell
reports, and it does not guess.

### Windows (PowerShell 5.1 or 7)

```powershell
$b64 = (gh api repos/5V715/vault/contents/scripts/install.ps1 --jq .content) -join ''
[IO.File]::WriteAllBytes("$PWD\install.ps1", [Convert]::FromBase64String($b64))
notepad .\install.ps1       # read it
powershell -ExecutionPolicy Bypass -File .\install.ps1          # latest, into %LOCALAPPDATA%\vault\bin
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Version v1.2.3 -Dir C:\tools\vault
```

`-ExecutionPolicy Bypass` applies to this one run only, because Windows refuses to run
scripts by default. The install directory is added to your user `PATH` once. Running the
script again does not add a second entry, and your other `PATH` entries stay exactly as
they were. Open a new terminal to pick up the change.

### What the scripts check

Both scripts do the same steps in the same order, and any failure stops the run with
nothing installed:

1. The trust root embedded in the script is a real key. Until the owner has created the
   release key, the scripts stop with `release key not configured`.
2. `gh` is installed and logged in.
3. The version is either `--version vX.Y.Z` / `-Version vX.Y.Z` (checked against
   `^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$`) or the latest published release.
   Drafts are never installable.
4. They download `SHA256SUMS` and `SHA256SUMS.sig` into a fresh temporary directory, then run
   `ssh-keygen -Y verify -f <allowed_signers> -I release@vault -n vault-release -s SHA256SUMS.sig < SHA256SUMS`
   against the allowed-signers line embedded in the script. A release without
   `SHA256SUMS.sig` is "not signed" and is refused.
5. Only then do they download the archive for this platform. `SHA256SUMS` must list its
   exact file name exactly once (a missing or duplicate entry is refused), and its
   SHA-256 must match.
6. Only then do they extract it, still in the temporary directory, and check that the
   three binaries are there.
7. They install all three binaries or none. Each new binary is first copied next to its
   destination under a temporary name. Then every previous version is moved aside, and
   only then is every new one renamed into place. If any step fails, the binaries that
   were placed are removed and the previous versions are moved back, so a failed run
   leaves an earlier install as it was. A destination that exists but is not a regular
   file is refused. On Windows, a running `.exe` can usually be moved aside. If it
   cannot, the install stops before replacing anything and asks you to stop the tool
   and retry. If an old copy is still running after a successful install, it is left
   next to the new one under a `.vault-old` name for you to delete.

The temporary directory is removed on every exit path.

You can check a release by hand the same way:

```sh
gh release download v1.2.3 --repo 5V715/vault -p SHA256SUMS -p SHA256SUMS.sig -p 'vault-1.2.3-x86_64-unknown-linux-musl.tar.gz'
ssh-keygen -Y verify -f keys/allowed_signers -I release@vault -n vault-release -s SHA256SUMS.sig < SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

## Release key (owner, once)

The release key is an OpenSSH Ed25519 key that never touches GitHub: not the repository,
not CI, not secrets, not logs. Create it on the machine that will do the signing, with a
passphrase, and keep it offline (for example on an encrypted USB stick with a backup copy
stored somewhere else):

```sh
ssh-keygen -t ed25519 -C "release@vault" -f vault-release     # asks for a passphrase
```

Then commit only the public half and its derived allowed-signers line, run from the
repository root:

```sh
cp /path/to/vault-release.pub keys/release.pub
printf 'release@vault namespaces="vault-release" %s\n' "$(cut -d' ' -f1,2 keys/release.pub)" > keys/allowed_signers
```

Replace the embedded line in both install scripts with exactly the content of
`keys/allowed_signers`. It sits between `# BEGIN VAULT RELEASE ALLOWED SIGNERS` and
`# END VAULT RELEASE ALLOWED SIGNERS`, as `embedded_signers='...'` in
`scripts/install.sh` and `$EmbeddedSigners = '...'` in `scripts/install.ps1`. Keep
`install.ps1` saved as UTF-8 with BOM. Then check:

```sh
scripts/check-key.sh                 # release.pub and allowed_signers agree
scripts/test/trust_root_test.sh      # all four copies agree (CI runs this too)
```

Until this is done, the release workflow refuses to build (`scripts/check-key.sh` is its
first job) and the install scripts refuse to install.

Rotating the key means repeating these steps. Install scripts fetched before the
rotation embed the old key and refuse releases signed with the new one, so users need to
fetch the script again.

## Release procedure (owner)

1. Tag and push: `git tag v1.2.3 && git push origin v1.2.3`. The `release` workflow
   builds the five targets and creates a **draft** release holding the five archives and
   `SHA256SUMS`. It never signs and never publishes.
2. Check the workflow run (the five builds, and the `SHA256SUMS` it printed).
3. On the signing machine, with the key available:

   ```sh
   scripts/sign-release.sh v1.2.3 /media/offline/vault-release
   ```

   The script refuses unless the trust root is a real key, `gh` is logged in, the
   release is a draft and has no `SHA256SUMS.sig` yet, and the key file is outside the
   repository (and not a symlink). It downloads `SHA256SUMS` and checks that it lists
   exactly the five archives of that version. It prints the file and asks `y/N`, signs
   with `ssh-keygen -Y sign -n vault-release` (which asks for the passphrase), and
   verifies the signature against `keys/allowed_signers`. Then it uploads
   `SHA256SUMS.sig`, checks that the draft's `SHA256SUMS` is still the file it signed,
   and publishes the release. The private key is only ever passed to `ssh-keygen` by
   path.
4. Optionally run `install.sh --version v1.2.3 --dir "$(mktemp -d)"` to see the published
   release verify end to end.

If anything fails before the last step, the release stays a draft and nobody can install
it.

## What the signature protects, and what it does not

The offline signature protects against tampering with a release **after it is built**:
changes in release storage, a takeover of the GitHub account after signing, or anything
on the download path. A changed archive fails the hash check, and a replaced
`SHA256SUMS` fails the signature check.

It does **not** protect against a compromised CI producing bad binaries that the owner
then signs. The owner signs `SHA256SUMS` as CI produced it. Reproducible builds would
close that gap; they are out of scope for now. The archive packaging is deterministic,
but the compiled binaries are not claimed to be reproducible.

## Testing the scripts

The tests use a local fake release. Throwaway keys and archives are generated in a
temporary directory, and a fake `gh` (`scripts/test/fake-gh`) serves them. Nothing
touches the network or a real key.

```sh
scripts/test/install_test.sh         # install.sh (Linux, macOS)
scripts/test/sign_release_test.sh    # sign-release.sh
scripts/test/trust_root_test.sh      # the four trust-root copies agree
```

`scripts/test/install.Tests.ps1` (Pester 5) covers `install.ps1` and runs in CI on
Windows under both PowerShell 5.1 and 7. The test-only trust-root override
`VAULT_INSTALL_TEST_ALLOWED_SIGNERS` (and, for `install.ps1`, the PATH registry key
`VAULT_INSTALL_TEST_PATH_KEY`) is honoured only when `VAULT_INSTALL_TEST=1`. A test
checks that it is ignored otherwise.
