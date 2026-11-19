#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Tests for scripts/install.sh against a local fake release: a throwaway OpenSSH key,
# archives and SHA256SUMS generated here, and a fake `gh` (scripts/test/fake-gh) that
# serves them from a directory. No network, no real release key. Runs on Linux and macOS
# (bash 3.2 compatible). Usage: scripts/test/install_test.sh [test-name...]
set -uo pipefail
scripts="$(cd "$(dirname "$0")/.." && pwd)"
install="$scripts/install.sh"
shim="$scripts/test/fake-gh"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-install-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
fails=0
placeholder='PLACEHOLDER — owner must replace'
targets="x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin"
windows_target=x86_64-pc-windows-msvc

sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

host_target() {
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) echo x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) echo aarch64-unknown-linux-musl ;;
    Darwin/x86_64) echo x86_64-apple-darwin ;;
    Darwin/arm64) echo aarch64-apple-darwin ;;
    *) echo "unsupported test host $(uname -s)/$(uname -m)" >&2; exit 1 ;;
  esac
}
host="$(host_target)"

# Throwaway signing keys, generated here and deleted with $tmp.
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/key"
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/foreign"
test_signers="release@vault namespaces=\"vault-release\" $(awk '{print $1 " " $2}' "$tmp/key.pub")"

# make_archive <out.tar.gz> <version> <target> [binaries...]
make_archive() {
  local out="$1" version="$2" target="$3" stage b
  shift 3
  stage="$(mktemp -d "$tmp/stage.XXXXXX")"
  [ $# -gt 0 ] || set -- vaultctl vault-dev vault-relay
  for b in "$@"; do
    printf '#!/bin/sh\necho "%s %s %s"\n' "$b" "$version" "$target" > "$stage/$b"
    chmod 0755 "$stage/$b"
  done
  echo "MIT" > "$stage/LICENSE"
  echo "vault" > "$stage/README.md"
  (cd "$stage" && tar -czf "$out" "$@" LICENSE README.md)
  rm -rf "$stage"
}

# sums <release-dir>: writes SHA256SUMS over every archive in the directory.
sums() {
  local f
  (cd "$1" && for f in vault-*; do printf '%s  %s\n' "$(sha256_file "$f")" "$f"; done) > "$1/SHA256SUMS"
}

# sign <release-dir> [key]: writes SHA256SUMS.sig.
sign() {
  rm -f "$1/SHA256SUMS.sig"
  ssh-keygen -q -Y sign -f "${2:-$tmp/key}" -n vault-release "$1/SHA256SUMS" > /dev/null 2>&1
}

# release <fake-dir> <tag>: a complete, signed, published release.
release() {
  local d="$1/releases/$2" v="${2#v}" t
  mkdir -p "$d"
  for t in $targets; do make_archive "$d/vault-$v-$t.tar.gz" "$v" "$t"; done
  echo "not a real zip" > "$d/vault-$v-$windows_target.zip"
  sums "$d"
  sign "$d"
}

# fake <name>: a fresh fake GitHub with v1.2.3 and v1.3.0 (latest).
fake() {
  local f="$tmp/$1/gh"
  release "$f" v1.2.3
  release "$f" v1.3.0
  echo v1.3.0 > "$f/latest"
  echo "$f"
}

# run <name> <fake-dir> [args...]: runs install.sh in test mode with the test key.
# Sets $out, $rc, $log (fake gh invocations), $home and $scratch (TMPDIR, must stay empty).
# Extra environment comes from $extra (space-separated NAME=VALUE, no spaces in values); $use_test_mode=0 drops
# VAULT_INSTALL_TEST=1; $path_prefix is put in front of PATH.
# shellcheck disable=SC2086 # $extra is split into NAME=VALUE words on purpose
run() {
  local name="$1" f="$2" mode=() extra="${extra:-}"
  shift 2
  home="$tmp/$name/home"
  scratch="$tmp/$name/scratch"
  log="$tmp/$name/gh.log"
  mkdir -p "$home" "$scratch"
  : > "$log"
  if [ "${use_test_mode:-1}" = 1 ]; then mode=(VAULT_INSTALL_TEST=1); fi
  rc=0
  out="$(env -u VAULT_INSTALL_TEST -u VAULT_INSTALL_TEST_ALLOWED_SIGNERS \
    PATH="${path_prefix:+$path_prefix:}$shim:$PATH" HOME="$home" TMPDIR="$scratch" \
    VAULT_FAKE_GH_DIR="$f" VAULT_FAKE_GH_LOG="$log" \
    VAULT_INSTALL_TEST_ALLOWED_SIGNERS="${signers_override-$test_signers}" \
    ${mode[@]+"${mode[@]}"} $extra \
    bash "$install" "$@" 2>&1)" || rc=$?
}

check() { # check <description> <command...>
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
# shellcheck disable=SC2012 # test-generated names only
listing() { ls -A "$1" | LC_ALL=C sort | tr '\n' ' '; }
says_either() { says "$1" || says "$2"; }
empty_dir() { [ -d "$1" ] && [ -z "$(ls -A "$1")" ]; }
absent_or_empty() { [ ! -e "$1" ] || empty_dir "$1"; }
no_download() { ! grep -q $'^release\tdownload' "$log"; }
installed_version() { [ "$("$1/vaultctl")" = "vaultctl $2 ${3:-$host}" ]; }
logged() { grep -q "$1" "$log"; }

install_happy_path() {
  local f d
  f="$(fake happy)"
  d="$tmp/happy/dest dir"
  run happy "$f" --dir "$d"
  check "exits 0" succeeded
  check "installs the latest release" installed_version "$d" 1.3.0
  check "installs all three binaries with mode 0755" \
    test "$(find "$d" -type f -perm 755 \( -name vaultctl -o -name vault-dev -o -name vault-relay \) | wc -l | tr -d ' ')" = 3
  check "installs nothing else" test "$(listing "$d")" = "vault-dev vault-relay vaultctl "
  check "temporary files are removed" empty_dir "$scratch"
  check "asked gh for the latest published release" logged $'^release\tview\t--repo\t5V715/vault'
  run happy "$f"
  check "default directory is ~/.local/bin" installed_version "$home/.local/bin" 1.3.0
  run happy "$f" --dir "$d"
  check "reinstalling over an existing install works" succeeded
}

install_rejects_tampered_archive() {
  local f r a
  f="$(fake tampered)"
  r="$f/releases/v1.3.0"
  a="$r/vault-1.3.0-$host.tar.gz"
  # The signed SHA256SUMS stays; only the archive changes.
  make_archive "$a" 6.6.6 "$host"
  run tampered "$f" --dir "$tmp/tampered/dest"
  check "fails" failed
  check "says the archive does not match SHA256SUMS" says "does not match SHA256SUMS"
  check "installs nothing" absent_or_empty "$tmp/tampered/dest"

  # A validly signed SHA256SUMS with the archive listed twice is ambiguous.
  f="$(fake duplicate)"
  r="$f/releases/v1.3.0"
  grep "$host" "$r/SHA256SUMS" | sed 's/^[0-9a-f]*/0000000000000000000000000000000000000000000000000000000000000000/' > "$r/extra"
  cat "$r/extra" >> "$r/SHA256SUMS" && rm "$r/extra"
  sign "$r"
  run duplicate "$f" --dir "$tmp/duplicate/dest"
  check "duplicate entry: fails" failed
  check "duplicate entry: says it is listed more than once" says "more than once"
  check "duplicate entry: installs nothing" absent_or_empty "$tmp/duplicate/dest"

  # No entry for the archive (and no partial-name match either).
  f="$(fake unlisted)"
  r="$f/releases/v1.3.0"
  sed "s/vault-1.3.0-$host.tar.gz/x-vault-1.3.0-$host.tar.gz/" "$r/SHA256SUMS" > "$r/s" && mv "$r/s" "$r/SHA256SUMS"
  sign "$r"
  run unlisted "$f" --dir "$tmp/unlisted/dest"
  check "unlisted archive: fails" failed
  check "unlisted archive: says SHA256SUMS does not list it" says "does not list"
  check "unlisted archive: installs nothing" absent_or_empty "$tmp/unlisted/dest"
}

install_rejects_foreign_signature() {
  local f r
  f="$(fake foreign)"
  r="$f/releases/v1.3.0"
  # An attacker swaps the archive, rewrites SHA256SUMS to match and signs with their key.
  make_archive "$r/vault-1.3.0-$host.tar.gz" 6.6.6 "$host"
  sums "$r"
  sign "$r" "$tmp/foreign"
  run foreign "$f" --dir "$tmp/foreign/dest"
  check "fails" failed
  check "says the signature check failed" says "signature check failed"
  check "installs nothing" absent_or_empty "$tmp/foreign/dest"
  check "never downloads the archive" bash -c "! grep -q 'vault-1.3.0-$host' '$log'"

  # A signature for another namespace (e.g. an ordinary SSH signature) is not accepted.
  f="$(fake namespace)"
  r="$f/releases/v1.3.0"
  rm "$r/SHA256SUMS.sig"
  ssh-keygen -q -Y sign -f "$tmp/key" -n file "$r/SHA256SUMS" > /dev/null 2>&1
  run namespace "$f" --dir "$tmp/namespace/dest"
  check "other namespace: fails" failed
  check "other namespace: says the signature check failed" says "signature check failed"
}

install_rejects_unsigned_release() {
  local f
  f="$(fake unsigned)"
  rm "$f/releases/v1.3.0/SHA256SUMS.sig"
  run unsigned "$f" --dir "$tmp/unsigned/dest"
  check "fails" failed
  check "says the release is not signed" says "not signed"
  check "installs nothing" absent_or_empty "$tmp/unsigned/dest"
  check "never downloads the archive" bash -c "! grep -q 'vault-1.3.0-$host' '$log'"
  check "temporary files are removed" empty_dir "$scratch"
}

install_refuses_placeholder_key() {
  local f
  f="$(fake placeholder)"
  signers_override="$placeholder" run placeholder "$f" --dir "$tmp/placeholder/dest"
  check "placeholder: fails" failed
  check "placeholder: says the release key is not configured" says "release key not configured"
  check "placeholder: downloads nothing" no_download
  check "placeholder: installs nothing" absent_or_empty "$tmp/placeholder/dest"
  signers_override="release@vault ssh-ed25519 AAAAgarbage" run placeholder "$f" --dir "$tmp/placeholder/dest"
  check "malformed line: says the release key is not configured" says "release key not configured"
  # The embedded line itself, while the repository still holds the placeholder.
  if grep -q PLACEHOLDER "$scripts/../keys/allowed_signers"; then
    signers_override="" run placeholder "$f" --dir "$tmp/placeholder/dest"
    check "embedded placeholder: refused" says "release key not configured"
    check "embedded placeholder: downloads nothing" no_download
  fi
}

install_requires_gh_login() {
  local f
  f="$(fake login)"
  extra="VAULT_FAKE_GH_AUTH_FAIL=1" run login "$f" --dir "$tmp/login/dest"
  check "fails" failed
  check "tells the user to run gh auth login" says "gh auth login"
  check "downloads nothing" no_download
  check "installs nothing" absent_or_empty "$tmp/login/dest"
}

install_pins_version() {
  local f d
  f="$(fake pin)"
  d="$tmp/pin/dest"
  run pin "$f" --version v1.2.3 --dir "$d"
  check "exits 0" succeeded
  check "installs the pinned version, not the latest" installed_version "$d" 1.2.3
  check "does not look up the latest release" bash -c "! grep -q $'^release\tview' '$log'"
  run pin "$f" --version=v1.2.3 --dir "$d"
  check "--version=TAG form works" succeeded
  local bad
  for bad in 1.2.3 v1.2 'v1.2.3*' 'v1.2.3 x' '../v1.2.3' ''; do
    run pin "$f" --version "$bad" --dir "$tmp/pin/bad"
    check "rejects version '$bad'" says "invalid version"
    check "rejects version '$bad' before downloading" no_download
  done
  run pin "$f" --version v9.9.9 --dir "$tmp/pin/bad"
  check "unknown version: fails" failed
  check "unknown version: installs nothing" absent_or_empty "$tmp/pin/bad"
  # A latest tag that is not a version is refused too.
  echo 'v1.3.0*' > "$f/latest"
  run pin "$f" --dir "$tmp/pin/bad"
  check "malformed latest tag: refused" says "invalid version"
}

install_cleans_up_on_failure() {
  local f r d
  # A previous install must survive a failed update untouched.
  f="$(fake cleanup)"
  d="$tmp/cleanup/dest"
  run cleanup "$f" --version v1.2.3 --dir "$d"
  check "previous install in place" installed_version "$d" 1.2.3
  r="$f/releases/v1.3.0"
  make_archive "$r/vault-1.3.0-$host.tar.gz" 1.3.0 "$host" vaultctl vault-dev
  sums "$r"
  sign "$r"
  run cleanup "$f" --dir "$d"
  check "archive without vault-relay: fails" failed
  check "archive without vault-relay: says it is missing" says "missing vault-relay"
  check "archive without vault-relay: previous install untouched" installed_version "$d" 1.2.3
  check "archive without vault-relay: no stray files in the destination" \
    test "$(listing "$d")" = "vault-dev vault-relay vaultctl "
  check "archive without vault-relay: temporary files are removed" empty_dir "$scratch"

  make_archive "$r/vault-1.3.0-$host.tar.gz" 6.6.6 "$host"
  run cleanup "$f" --dir "$d"
  check "tampered archive: previous install untouched" installed_version "$d" 1.2.3
  check "tampered archive: temporary files are removed" empty_dir "$scratch"

  rm "$r/SHA256SUMS.sig"
  run cleanup "$f" --dir "$d"
  check "unsigned release: temporary files are removed" empty_dir "$scratch"
}

test_override_ignored_without_test_mode() {
  local f
  f="$(fake notest)"
  # A valid release signed by the test key, and the override pointing at that key: without
  # VAULT_INSTALL_TEST=1 the embedded trust root is used, so this must not install.
  use_test_mode=0 run notest "$f" --dir "$tmp/notest/dest"
  check "fails" failed
  check "uses the embedded trust root (placeholder refused or test key rejected)" \
    says_either "release key not configured" "signature check failed"
  check "installs nothing" absent_or_empty "$tmp/notest/dest"
  use_test_mode=0 extra="VAULT_INSTALL_TEST=0" run notest "$f" --dir "$tmp/notest/dest"
  check "VAULT_INSTALL_TEST=0 is not test mode" absent_or_empty "$tmp/notest/dest"
  use_test_mode=0 extra="VAULT_INSTALL_TEST=yes" run notest "$f" --dir "$tmp/notest/dest"
  check "VAULT_INSTALL_TEST=yes is not test mode" absent_or_empty "$tmp/notest/dest"
}

install_detects_target() {
  local f u spec s m want
  f="$(fake target)"
  u="$tmp/target/uname"
  mkdir -p "$u"
  # shellcheck disable=SC2016 # expanded by the fake uname
  printf '#!/bin/sh\ncase "$1" in -s) echo "$FAKE_UNAME_S" ;; -m) echo "$FAKE_UNAME_M" ;; *) exit 1 ;; esac\n' > "$u/uname"
  chmod +x "$u/uname"
  for spec in Linux/x86_64/x86_64-unknown-linux-musl Linux/aarch64/aarch64-unknown-linux-musl \
    Linux/arm64/aarch64-unknown-linux-musl Darwin/arm64/aarch64-apple-darwin Darwin/x86_64/x86_64-apple-darwin; do
    s="${spec%%/*}"; m="${spec#*/}"; m="${m%%/*}"; want="${spec##*/}"
    path_prefix="$u" extra="FAKE_UNAME_S=$s FAKE_UNAME_M=$m" run target "$f" --dir "$tmp/target/$want"
    check "$s $m installs $want" installed_version "$tmp/target/$want" 1.3.0 "$want"
  done
  for spec in FreeBSD/amd64 Linux/armv7l Linux/i686 Darwin/ppc; do
    s="${spec%%/*}"; m="${spec#*/}"
    path_prefix="$u" extra="FAKE_UNAME_S=$s FAKE_UNAME_M=$m" run target "$f" --dir "$tmp/target/bad"
    check "$s $m: unsupported platform" says "unsupported platform"
    check "$s $m: downloads nothing" no_download
  done
}

all="install_happy_path install_rejects_tampered_archive install_rejects_foreign_signature
install_rejects_unsigned_release install_refuses_placeholder_key install_requires_gh_login
install_pins_version install_cleans_up_on_failure test_override_ignored_without_test_mode
install_detects_target"
# shellcheck disable=SC2086 # word list
[ $# -gt 0 ] || set -- $all
for t in "$@"; do
  echo "$t"
  "$t"
done
if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; exit 1; fi
