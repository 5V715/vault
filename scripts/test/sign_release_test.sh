#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Tests for scripts/sign-release.sh against the fake gh (scripts/test/fake-gh) and a
# copy of the scripts with a throwaway trust root. No network, no real release key.
# Usage: scripts/test/sign_release_test.sh [test-name...]
set -uo pipefail
scripts="$(cd "$(dirname "$0")/.." && pwd)"
shim="$scripts/test/fake-gh"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-sign-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
fails=0
version=1.3.0
tag="v$version"
archives="vault-$version-aarch64-apple-darwin.tar.gz vault-$version-aarch64-unknown-linux-musl.tar.gz
vault-$version-x86_64-apple-darwin.tar.gz vault-$version-x86_64-pc-windows-msvc.zip vault-$version-x86_64-unknown-linux-musl.tar.gz"
# As scripts/sign-android-apk.sh leaves the draft (its own test runs it for real).
apks="vault-android-$version-unsigned.apk vault-android-$version.apk"

sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# Throwaway keys outside the repository copies (deleted with $tmp).
mkdir -p "$tmp/offline"
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/offline/release"
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/offline/other"
line="release@vault namespaces=\"vault-release\" $(awk '{print $1 " " $2}' "$tmp/offline/release.pub")"

# repo <name> [signers-line pub-line]: a copy of the scripts with the given trust root.
repo() {
  local r="$tmp/$1/repo"
  mkdir -p "$r/scripts" "$r/keys"
  cp "$scripts/sign-release.sh" "$scripts/check-key.sh" "$r/scripts/"
  printf '%s\n' "${2:-$line}" > "$r/keys/allowed_signers"
  printf '%s\n' "${3:-$(cat "$tmp/offline/release.pub")}" > "$r/keys/release.pub"
  echo "$r"
}

# fake <name>: a fake GitHub holding the draft release $tag, as the workflow and
# sign-android-apk.sh leave it.
fake() {
  local f="$tmp/$1/gh" d a
  d="$f/releases/$tag"
  mkdir -p "$d"
  for a in $archives $apks; do echo "asset $a" > "$d/$a"; done
  # shellcheck disable=SC2086 # word lists
  (cd "$d" && for a in $(printf '%s\n' $archives $apks | LC_ALL=C sort); do printf '%s  %s\n' "$(sha256_file "$a")" "$a"; done) > "$d/SHA256SUMS"
  touch "$d/.draft"
  echo "$f"
}

# run <name> <repo> <fake> <answer> [args...]: sets $out, $rc, $log.
run() {
  local name="$1" r="$2" f="$3" answer="$4"
  shift 4
  log="$tmp/$name/gh.log"
  : > "$log"
  rc=0
  out="$(printf '%s\n' "$answer" | env PATH="$shim:$PATH" VAULT_FAKE_GH_DIR="$f" VAULT_FAKE_GH_LOG="$log" \
    ${auth_fail:+VAULT_FAKE_GH_AUTH_FAIL=1} "$BASH" "$r/scripts/sign-release.sh" "$@" 2>&1)" || rc=$?
}

check() {
  local what="$1"
  shift
  if "$@"; then
    echo "  ok   $what"
  else
    echo "  FAIL $what"
    printf '%s\n' "$out" | while IFS= read -r l; do printf '       | %s\n' "$l"; done
    fails=$((fails + 1))
  fi
}
failed() { [ "$rc" != 0 ]; }
succeeded() { [ "$rc" = 0 ]; }
says() { case "$out" in *"$1"*) return 0 ;; *) return 1 ;; esac; }
draft() { [ -e "$1/releases/$tag/.draft" ]; }
published() { ! draft "$1"; }
unsigned() { [ ! -e "$1/releases/$tag/SHA256SUMS.sig" ]; }
no_upload() { ! grep -q $'^release\tupload' "$log" && ! grep -q $'^release\tedit' "$log"; }
valid_sig() {
  ssh-keygen -Y verify -f "$2/keys/allowed_signers" -I release@vault -n vault-release \
    -s "$1/releases/$tag/SHA256SUMS.sig" < "$1/releases/$tag/SHA256SUMS" > /dev/null 2>&1
}
no_key_material() {
  local l
  says "PRIVATE KEY" && return 1
  while IFS= read -r l; do
    case "$l" in -----*) continue ;; esac
    [ -n "$l" ] && says "$l" && return 1
  done < "$tmp/offline/release"
  return 0
}

sign_happy_path() {
  local r f
  r="$(repo happy)"; f="$(fake happy)"
  run happy "$r" "$f" y "$tag" "$tmp/offline/release"
  check "exits 0" succeeded
  check "shows SHA256SUMS before asking" says "vault-$version-x86_64-pc-windows-msvc.zip"
  check "shows the signed APK's line" says "  vault-android-$version.apk"
  check "uploads a signature valid under keys/allowed_signers" valid_sig "$f" "$r"
  check "publishes the release" published "$f"
  check "never prints the private key" no_key_material
  check "uploads before publishing" test "$(grep -E $'^release\t(upload|edit)' "$log" | cut -f2 | tr '\n' ' ')" = "upload edit "
}

sign_aborts_without_confirmation() {
  local r f a
  r="$(repo abort)"; f="$(fake abort)"
  for a in n N no "" yes-please; do
    run abort "$r" "$f" "$a" "$tag" "$tmp/offline/release"
    check "answer '$a': fails" failed
    check "answer '$a': nothing uploaded, still a draft" bash -c "[ -e '$f/releases/$tag/.draft' ] && [ ! -e '$f/releases/$tag/SHA256SUMS.sig' ]"
  done
}

sign_refuses_non_draft() {
  local r f
  r="$(repo published)"; f="$(fake published)"
  rm "$f/releases/$tag/.draft"
  run published "$r" "$f" y "$tag" "$tmp/offline/release"
  check "fails" failed
  check "says the release is not a draft" says "not a draft"
  check "uploads nothing" no_upload
}

sign_refuses_already_signed() {
  local r f
  r="$(repo signed)"; f="$(fake signed)"
  echo "old signature" > "$f/releases/$tag/SHA256SUMS.sig"
  run signed "$r" "$f" y "$tag" "$tmp/offline/release"
  check "fails" failed
  check "says the release is already signed" says "already signed"
  check "uploads nothing" no_upload
  check "still a draft" draft "$f"
}

sign_refuses_key_inside_repo() {
  local r f
  r="$(repo inrepo)"; f="$(fake inrepo)"
  cp "$tmp/offline/release" "$r/keys/release"
  run inrepo "$r" "$f" y "$tag" "$r/keys/release"
  check "fails" failed
  check "says the key must not be inside the repository" says "inside the repository"
  check "uploads nothing" no_upload
  ln -s "$r/keys/release" "$tmp/inrepo/link"
  run inrepo "$r" "$f" y "$tag" "$tmp/inrepo/link"
  check "symlink into the repository: refused" failed
  check "symlink into the repository: uploads nothing" no_upload
  run inrepo "$r" "$f" y "$tag" "$tmp/offline/missing"
  check "missing key file: refused" failed
}

sign_refuses_wrong_key() {
  local r f
  r="$(repo wrong)"; f="$(fake wrong)"
  run wrong "$r" "$f" y "$tag" "$tmp/offline/other"
  check "fails" failed
  check "says the signature does not verify against keys/allowed_signers" says "keys/allowed_signers"
  check "uploads nothing" no_upload
  check "still a draft, unsigned" bash -c "[ -e '$f/releases/$tag/.draft' ] && [ ! -e '$f/releases/$tag/SHA256SUMS.sig' ]"
}

sign_refuses_placeholder_trust_root() {
  local r f p='PLACEHOLDER — owner must replace'
  r="$(repo placeholder "$p" "$p")"; f="$(fake placeholder)"
  run placeholder "$r" "$f" y "$tag" "$tmp/offline/release"
  check "fails" failed
  check "says the release key is not set up" says "release key not set up"
  check "talks to nothing" test ! -s "$log"
}

sign_refuses_unexpected_sums() {
  local r f d
  r="$(repo extra)"; f="$(fake extra)"
  d="$f/releases/$tag"
  printf '%s  %s\n' "$(sha256_file "$d/SHA256SUMS")" "evil.sh" >> "$d/SHA256SUMS"
  run extra "$r" "$f" y "$tag" "$tmp/offline/release"
  check "extra entry: fails" failed
  check "extra entry: uploads nothing" no_upload
  r="$(repo missing)"; f="$(fake missing)"
  d="$f/releases/$tag"
  grep -v windows "$d/SHA256SUMS" > "$d/s"; mv "$d/s" "$d/SHA256SUMS"
  run missing "$r" "$f" y "$tag" "$tmp/offline/release"
  check "missing entry: fails" failed
  check "missing entry: uploads nothing" no_upload
}

sign_requires_signed_apk() {
  local r f d
  r="$(repo noapk)"; f="$(fake noapk)"
  d="$f/releases/$tag"
  rm "$d/vault-android-$version.apk"
  grep -v " vault-android-$version.apk\$" "$d/SHA256SUMS" > "$d/s"; mv "$d/s" "$d/SHA256SUMS"
  run noapk "$r" "$f" y "$tag" "$tmp/offline/release"
  check "unsigned APK only: fails" failed
  check "unsigned APK only: says to run sign-android-apk.sh" says "sign-android-apk.sh"
  check "unsigned APK only: uploads nothing" no_upload
  r="$(repo swapped)"; f="$(fake swapped)"
  echo "another apk" > "$f/releases/$tag/vault-android-$version.apk"
  run swapped "$r" "$f" y "$tag" "$tmp/offline/release"
  check "signed APK not the listed one: fails" failed
  check "signed APK not the listed one: says so" says "does not match SHA256SUMS"
  check "signed APK not the listed one: uploads nothing" no_upload
}

sign_requires_gh_login() {
  local r f
  r="$(repo login)"; f="$(fake login)"
  auth_fail=1 run login "$r" "$f" y "$tag" "$tmp/offline/release"
  check "fails" failed
  check "tells the owner to run gh auth login" says "gh auth login"
  check "uploads nothing" no_upload
}

sign_validates_arguments() {
  local r f
  r="$(repo args)"; f="$(fake args)"
  run args "$r" "$f" y "1.3.0" "$tmp/offline/release"
  check "tag without v: refused" says "invalid tag"
  run args "$r" "$f" y "$tag"
  check "no key: usage" failed
  check "no key: talks to nothing" test ! -s "$log"
}

all="sign_happy_path sign_aborts_without_confirmation sign_refuses_non_draft sign_refuses_already_signed
sign_refuses_key_inside_repo sign_refuses_wrong_key sign_refuses_placeholder_trust_root
sign_refuses_unexpected_sums sign_requires_signed_apk sign_requires_gh_login sign_validates_arguments"
# shellcheck disable=SC2086 # word list
[ $# -gt 0 ] || set -- $all
for t in "$@"; do
  echo "$t"
  "$t"
done
if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; exit 1; fi
