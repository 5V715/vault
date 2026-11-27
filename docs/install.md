# Installing the prebuilt tools

Each release of `5V715/vault` ships `vaultctl`, `vault-dev` and `vault-relay` prebuilt
for five targets (and the Android vault app, `vault-android-<version>.apk`: see
[`docs/android.md`](android.md)):

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

The repository is public: the scripts and releases download over plain HTTPS from
github.com, with no GitHub account and no GitHub CLI. There is still no `curl | sh` path:
download the script, read it, then run it.

## Install (users)

You need `ssh-keygen` from OpenSSH 8.2 or later. Linux and macOS ship it. On Windows
10/11 it comes with the OpenSSH client (Settings > Optional features) or with Git for
Windows. On Linux and macOS you also need `curl` (or else `wget`).

### Linux and macOS

```sh
curl --proto '=https' --tlsv1.2 -fsSLO https://raw.githubusercontent.com/5V715/vault/main/scripts/install.sh
less install.sh            # read it: it is short, and it is what decides what you trust
bash install.sh            # latest stable release, into ~/.local/bin
bash install.sh --version v1.2.3 --dir /opt/vault/bin
```

Before the first install, compare the release key the script trusts with the
fingerprint the owner publishes outside this repository (see
[Verify the release key](#verify-the-release-key)).

The script also needs `tar` and `sha256sum` (Linux) or `shasum` (macOS). It picks the
target from `uname -s` and `uname -m`. Under Rosetta it installs whatever the shell
reports, and it does not guess.

### Windows (PowerShell 5.1 or 7)

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 'Tls12'   # Windows PowerShell 5.1 only
Invoke-WebRequest -UseBasicParsing -OutFile install.ps1 https://raw.githubusercontent.com/5V715/vault/main/scripts/install.ps1
notepad .\install.ps1       # read it
powershell -ExecutionPolicy Bypass -File .\install.ps1          # latest stable, into %LOCALAPPDATA%\vault\bin
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
2. The tools they need are there (`ssh-keygen`, and `curl` or `wget` for `install.sh`).
3. The version is either `--version vX.Y.Z` / `-Version vX.Y.Z` (checked against
   `^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$`) or the latest stable release. The
   latest stable release is the published `vX.Y.Z` release that GitHub marks latest, or
   else the newest one. Finding it takes one or two unauthenticated requests to the GitHub
   API (`api.github.com`, 60 requests per hour per address); if that limit is used up the
   scripts say so, and `--version` / `-Version` needs no API request at all. Drafts are
   never installable. Pre-releases (tags with a suffix, such as `v1.3.0-rc.1`) are only
   installed when named with `--version` / `-Version`.
4. They download `SHA256SUMS` and `SHA256SUMS.sig` from
   `https://github.com/5V715/vault/releases/download/<tag>/` into a fresh temporary
   directory. Every download is TLS only (TLS 1.2 or later) and fails on any HTTP error;
   redirects are followed by hand, at most three, and only to `github.com` and GitHub's
   release-asset hosts (`release-assets.githubusercontent.com`,
   `objects.githubusercontent.com`). A redirect anywhere else, or to plain http, stops
   the install. Then they run
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
base=https://github.com/5V715/vault/releases/download/v1.2.3
for f in SHA256SUMS SHA256SUMS.sig vault-1.2.3-x86_64-unknown-linux-musl.tar.gz; do
  curl --proto '=https' --tlsv1.2 -fsSLO "$base/$f"
done
ssh-keygen -Y verify -f keys/allowed_signers -I release@vault -n vault-release -s SHA256SUMS.sig < SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS                                    # Linux
grep ' vault-1.2.3-aarch64-apple-darwin.tar.gz$' SHA256SUMS | shasum -a 256 --check  # macOS
```

Here `keys/allowed_signers` is the file from a checkout. Its key must match the
published fingerprint (next section).

## Verify the release key

The install scripts trust the key embedded in them, and you fetch them from the same
repository as the releases. Someone who took over the owner's GitHub account could swap
both the script and the releases. The only check against that is to compare the key's
fingerprint with the one the owner publishes **outside this repository** (on their
website or profile, for example), before the first install and whenever you fetch the
script again.

Print the fingerprint the downloaded script trusts:

```sh
# install.sh
grep "^embedded_signers='" install.sh | cut -d' ' -f3,4 | tr -d "'" > embedded.pub
ssh-keygen -lf embedded.pub
# a checkout
ssh-keygen -lf keys/release.pub
```

```powershell
# install.ps1
$line = (Select-String -LiteralPath .\install.ps1 -Pattern '^\$EmbeddedSigners = ''(.*)''$').Matches[0].Groups[1].Value
(($line -split ' ')[2, 3] -join ' ') | Set-Content -Encoding ascii embedded.pub
ssh-keygen -lf embedded.pub
```

Each prints a line like `256 SHA256:... (ED25519)`. The `SHA256:...` value must equal
the published fingerprint. If it does not, do not run the script.

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

Until this is done, the release workflow refuses to build, and the install scripts
refuse to install. Its first job runs `scripts/check-key.sh` and
`scripts/test/trust_root_test.sh`, so a tag whose scripts still embed the placeholder
or another key builds nothing.

Publish the fingerprint (`ssh-keygen -lf keys/release.pub`, the `SHA256:...` value)
somewhere **outside this repository** that you control separately, such as your website
or a profile. Users compare it with the script before their first install (see
[Verify the release key](#verify-the-release-key)).

Rotating the key means repeating these steps. Install scripts fetched before the
rotation embed the old key and refuse releases signed with the new one, so users need to
fetch the script again. Publish the new fingerprint too.

## Release procedure (owner)

1. Tag and push: `git tag v1.2.3 && git push origin v1.2.3`. The `release` workflow
   builds the five targets and the unsigned Android APK, and creates a **draft** release
   holding the five archives, `vault-android-1.2.3-unsigned.apk` and `SHA256SUMS`. It never signs and never publishes. A tag with a suffix
   (`v1.3.0-rc.1`) becomes a pre-release: the install scripts install it only when it is
   named with `--version`.
2. Check the workflow run (the five builds, the APK, and the `SHA256SUMS` it printed).
3. Sign the Android APK offline first:
   `scripts/sign-android-apk.sh v1.2.3 /media/offline/vault-android.p12`. It uploads
   `vault-android-1.2.3.apk` and adds its line to `SHA256SUMS`, so the signature of the
   next step covers the APK that is published. See [`docs/android.md`](android.md),
   "Releases".
4. On the signing machine, with the key available:

   ```sh
   scripts/sign-release.sh v1.2.3 /media/offline/vault-release
   ```

   The script refuses unless the trust root is a real key, `gh` is logged in, the
   release is a draft and has no `SHA256SUMS.sig` yet, and the key file is outside the
   repository (and not a symlink). It downloads `SHA256SUMS` and checks that it lists
   exactly the five archives and the two APKs of that version, and that the signed APK
   on the draft matches its line. It prints the file and asks `y/N`, signs
   with `ssh-keygen -Y sign -n vault-release` (which asks for the passphrase), and
   verifies the signature against `keys/allowed_signers`. Then it uploads
   `SHA256SUMS.sig`, checks that the draft's `SHA256SUMS` is still the file it signed,
   and publishes the release. The private key is only ever passed to `ssh-keygen` by
   path.
5. Optionally run `install.sh --version v1.2.3 --dir "$(mktemp -d)"` to see the published
   release verify end to end.

If anything fails before the last step, the release stays a draft and nobody can install
it.

## What the signature protects, and what it does not

The offline signature protects the **release assets and the download path**. A
changed archive fails the hash check. A replaced `SHA256SUMS` fails the signature check,
whether the change was made in release storage, in transit, or by someone who can edit
releases but holds no key.

It protects against a **takeover of the GitHub account** (or of push access) only if you
checked the key out of band. The documented install fetches the script, and with it the
trust root, from the same repository. An attacker with the account could commit a
script that embeds their own key and publish releases signed with it. A copy of the
script obtained before the takeover, or one whose embedded key you compared with the
fingerprint published outside this repository (see
[Verify the release key](#verify-the-release-key)), still refuses those releases.

The signature does not stop a **rollback** either. Anyone who can edit releases can
mark an older, validly signed release as latest, or delete newer ones, and the default
install then picks the old version. Pin the version you want with
`--version vX.Y.Z` / `-Version vX.Y.Z`. That is also how you go back to an earlier
version on purpose: `bash install.sh --version v1.2.2` reinstalls that release over the
current one, verified the same way.

It does **not** protect against a compromised CI producing bad binaries that the owner
then signs. The owner signs `SHA256SUMS` as CI produced it. Reproducible builds would
close that gap; they are out of scope for now. The archive packaging is deterministic,
but the compiled binaries are not claimed to be reproducible.

## Testing the scripts

The tests use a local fake release. Throwaway keys and archives are generated in a
temporary directory. The install tests serve them over HTTPS from a local stand-in for
github.com and its API (`scripts/test/fake-github/server.py`, with a throwaway certificate;
it also serves redirects off GitHub, plain-http redirects and an exhausted API rate limit,
which the scripts must refuse or report). The signing tests use a fake `gh`
(`scripts/test/fake-gh`). Nothing touches the network or a real key. The install tests
need `python3` and `openssl`.

```sh
scripts/test/install_test.sh         # install.sh (Linux, macOS)
scripts/test/sign_release_test.sh    # sign-release.sh
scripts/test/sign_android_apk_test.sh  # sign-android-apk.sh (fake apksigner; CI also runs the real one)
scripts/test/trust_root_test.sh      # the four trust-root copies agree
```

`scripts/test/install.Tests.ps1` (Pester 5) covers `install.ps1` and runs in CI on
Windows under both PowerShell 5.1 and 7. The test-only trust-root override
`VAULT_INSTALL_TEST_ALLOWED_SIGNERS` (and, for `install.ps1`, the PATH registry key
`VAULT_INSTALL_TEST_PATH_KEY`) is honoured only when `VAULT_INSTALL_TEST=1`. A test
checks that it is ignored otherwise.
