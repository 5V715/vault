#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Installs vaultctl, vault-dev and vault-relay from a signed GitHub release of 5V715/vault
# (Linux and macOS). See docs/install.md.
#
#   install.sh [--version vX.Y.Z] [--dir DIR]
#
# Downloads with the GitHub CLI (`gh`, logged in), then verifies before installing:
#   1. SHA256SUMS.sig is a valid `ssh-keygen -Y` signature over SHA256SUMS by the release
#      key embedded below (principal release@vault, namespace vault-release);
#   2. the archive's SHA-256 matches its single entry in SHA256SUMS.
# Only then is the archive extracted (in a temporary directory) and the three binaries
# installed to DIR (default ~/.local/bin) with mode 0755. Any failure installs nothing;
# the temporary directory is always removed. Needs gh, ssh-keygen (OpenSSH 8.2+), tar and
# sha256sum or shasum.
set -euo pipefail

REPO=5V715/vault
BINARIES="vaultctl vault-dev vault-relay"
VERSION_RE='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

# The release trust root: must equal keys/allowed_signers (scripts/test/trust_root_test.sh
# enforces it). Replace it only together with keys/release.pub and keys/allowed_signers.
# BEGIN VAULT RELEASE ALLOWED SIGNERS
embedded_signers='PLACEHOLDER — owner must replace'
# END VAULT RELEASE ALLOWED SIGNERS

die() {
  echo "install.sh: error: $*" >&2
  exit 1
}

usage() {
  sed -n '6p' "$0" | sed 's/^# *//'
  exit "${1:-0}"
}

version=""
dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version="$2"; shift 2 || true; [ -n "$version" ] || die "invalid version '' (expected vX.Y.Z)" ;;
    --version=*) version="${1#--version=}"; shift; [ -n "$version" ] || die "invalid version '' (expected vX.Y.Z)" ;;
    --dir) [ $# -ge 2 ] || die "--dir needs a value"; dir="$2"; shift 2 ;;
    --dir=*) dir="${1#--dir=}"; shift ;;
    -h | --help) usage 0 ;;
    *) echo "install.sh: unknown argument: $1" >&2; usage 1 ;;
  esac
done
if [ -n "$version" ] && ! [[ "$version" =~ $VERSION_RE ]]; then
  die "invalid version '$version' (expected vX.Y.Z)"
fi
[ -n "$dir" ] || dir="${HOME:?HOME is not set}/.local/bin"

# Trust root. The test override is honoured only in test mode (VAULT_INSTALL_TEST=1).
signers="$embedded_signers"
if [ "${VAULT_INSTALL_TEST:-}" = 1 ] && [ -n "${VAULT_INSTALL_TEST_ALLOWED_SIGNERS:-}" ]; then
  signers="$VAULT_INSTALL_TEST_ALLOWED_SIGNERS"
fi
if ! [[ "$signers" =~ ^release@vault\ namespaces=\"vault-release\"\ ssh-ed25519\ AAAAC3NzaC1lZDI1NTE5[A-Za-z0-9+/]+=*$ ]]; then
  die "release key not configured: this copy of install.sh has no release key (the owner has not created one yet, see docs/install.md). Nothing was installed."
fi

for tool in gh ssh-keygen tar; do
  command -v "$tool" > /dev/null 2>&1 || die "$tool is required but not installed"
done
if command -v sha256sum > /dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum > /dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "sha256sum or shasum is required but not installed"
fi

# Platform. macOS reports the hardware it runs on; no guessing under Rosetta.
os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) die "unsupported platform $os/$arch (prebuilt: Linux x86_64/aarch64, macOS arm64/x86_64; build from source instead)" ;;
esac

gh auth status > /dev/null 2>&1 || die "gh is not logged in to GitHub; run: gh auth login"

if [ -z "$version" ]; then
  # The latest published release (drafts and prereleases are never "latest").
  version="$(gh release view --repo "$REPO" --json tagName --jq .tagName)" || die "could not find the latest release of $REPO"
  [[ "$version" =~ $VERSION_RE ]] || die "invalid version '$version' reported as the latest release"
fi
archive="vault-${version#v}-$target.tar.gz"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-install.XXXXXX")" || die "could not create a temporary directory"
# Install state, for cleanup and rollback. Only the fixed binary names are ever split on
# whitespace; every path is built from "$install_dir" and quoted.
install_dir=""   # set once files are staged in the destination
suffix=".vault-install.$$"
backed=""        # binaries whose previous version was moved aside to .<b>.vault-old<suffix>
placed=""        # binaries whose new version was renamed into place
installed=0
cleanup() {
  local b
  if [ -n "$install_dir" ]; then
    if [ "$installed" != 1 ]; then
      # Roll back: remove what this run placed, then put the previous versions back.
      for b in $placed; do rm -f "$install_dir/$b"; done
      for b in $backed; do
        mv -f "$install_dir/.$b.vault-old$suffix" "$install_dir/$b" ||
          echo "install.sh: error: could not restore $install_dir/$b from $install_dir/.$b.vault-old$suffix" >&2
      done
    fi
    for b in $BINARIES; do rm -f "$install_dir/.$b$suffix"; done
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$tmp/dl" "$tmp/x"

echo "vault $version for $target"

# 1. The signature over SHA256SUMS, before anything else is downloaded.
gh release download "$version" --repo "$REPO" -p SHA256SUMS -D "$tmp/dl" > /dev/null ||
  die "could not download SHA256SUMS of release $version"
gh release download "$version" --repo "$REPO" -p SHA256SUMS.sig -D "$tmp/dl" > /dev/null 2>&1 || true
[ -f "$tmp/dl/SHA256SUMS" ] || die "release $version has no SHA256SUMS"
[ -f "$tmp/dl/SHA256SUMS.sig" ] || die "release $version is not signed (no SHA256SUMS.sig); nothing was installed"
printf '%s\n' "$signers" > "$tmp/allowed_signers"
if ! ssh-keygen -Y verify -f "$tmp/allowed_signers" -I release@vault -n vault-release \
  -s "$tmp/dl/SHA256SUMS.sig" < "$tmp/dl/SHA256SUMS" > "$tmp/verify.log" 2>&1; then
  cat "$tmp/verify.log" >&2
  die "SHA256SUMS.sig signature check failed: release $version was not signed by the vault release key; nothing was installed"
fi
echo "signature: ok (release@vault)"

# 2. The archive's hash: exactly one SHA256SUMS line must name it.
expected=""
count=0
while IFS= read -r line || [ -n "$line" ]; do
  line="${line%$'\r'}"
  [ -n "$line" ] || continue
  [[ "$line" =~ ^([0-9a-fA-F]{64})\ [\ *](.+)$ ]] || die "SHA256SUMS has a malformed line: $line"
  if [ "${BASH_REMATCH[2]}" = "$archive" ]; then
    expected="${BASH_REMATCH[1]}"
    count=$((count + 1))
  fi
done < "$tmp/dl/SHA256SUMS"
[ "$count" -gt 0 ] || die "SHA256SUMS does not list $archive (no build for $target in $version?)"
[ "$count" = 1 ] || die "SHA256SUMS lists $archive more than once; refusing an ambiguous release"
gh release download "$version" --repo "$REPO" -p "$archive" -D "$tmp/dl" > /dev/null ||
  die "could not download $archive"
[ -f "$tmp/dl/$archive" ] || die "could not download $archive"
actual="$(sha256 "$tmp/dl/$archive")"
expected="$(printf '%s' "$expected" | tr 'A-F' 'a-f')"
[ "$actual" = "$expected" ] || die "$archive does not match SHA256SUMS (got $actual, signed $expected); nothing was installed"
echo "sha256: ok ($archive)"

# 3. Extract (verified content only) and check the three binaries are there.
tar -xzf "$tmp/dl/$archive" -C "$tmp/x" || die "could not extract $archive"
for b in $BINARIES; do
  [ -f "$tmp/x/$b" ] && [ ! -L "$tmp/x/$b" ] || die "$archive is missing $b; nothing was installed"
done

# 4. Install, all or nothing: stage every binary next to its destination, move every
#    previous version aside, rename every new one into place; on any failure the cleanup
#    trap removes what was placed and restores the previous versions.
mkdir -p "$dir" || die "could not create $dir"
dir="$(cd "$dir" && pwd)"
for b in $BINARIES; do
  if [ -e "$dir/$b" ] && [ ! -f "$dir/$b" ]; then
    die "$dir/$b exists and is not a regular file; nothing was installed"
  fi
done
install_dir="$dir"
for b in $BINARIES; do
  if ! cp "$tmp/x/$b" "$dir/.$b$suffix" || ! chmod 0755 "$dir/.$b$suffix"; then
    die "could not write to $dir; nothing was installed"
  fi
done
for b in $BINARIES; do
  if [ -e "$dir/$b" ] || [ -L "$dir/$b" ]; then
    mv -f "$dir/$b" "$dir/.$b.vault-old$suffix" || die "could not move $dir/$b aside; nothing was installed"
    backed="$backed $b"
  fi
done
for b in $BINARIES; do
  if [ "${VAULT_INSTALL_TEST:-}" = 1 ] && [ "${VAULT_INSTALL_TEST_FAIL_RENAME:-}" = "$b" ]; then
    die "could not install $dir/$b (test hook); the previous version was restored"
  fi
  mv -f "$dir/.$b$suffix" "$dir/$b" || die "could not install $dir/$b; the previous version was restored"
  placed="$placed $b"
done
installed=1
for b in $backed; do rm -f "$dir/.$b.vault-old$suffix"; done
echo "installed $BINARIES ($version) to $dir"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "note: $dir is not on your PATH; add it, e.g. export PATH=\"$dir:\$PATH\"" ;;
esac
