#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# trust_root_files_agree: keys/release.pub, keys/allowed_signers and the allowed-signers
# line embedded in scripts/install.sh and scripts/install.ps1 (between the BEGIN/END
# markers) must agree. While the owner key does not exist all of them hold the
# placeholder; once it does, keys/allowed_signers must be derived from keys/release.pub
# (scripts/check-key.sh) and both scripts must embed exactly that line.
# Usage: scripts/test/trust_root_test.sh [repo-root]
set -uo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
check_key="$here/check-key.sh"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-trust-root-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
fails=0

# embedded <script>: prints the quoted value between the markers; fails unless the
# markers appear exactly once and enclose exactly one non-comment line.
embedded() {
  local f="$1" begins ends body
  begins="$(grep -c '^# BEGIN VAULT RELEASE ALLOWED SIGNERS' "$f")"
  ends="$(grep -c '^# END VAULT RELEASE ALLOWED SIGNERS' "$f")"
  [ "$begins" = 1 ] && [ "$ends" = 1 ] || { echo "$f: expected one BEGIN and one END marker" >&2; return 1; }
  body="$(tr -d '\r' < "$f" | sed -n '/^# BEGIN VAULT RELEASE ALLOWED SIGNERS/,/^# END VAULT RELEASE ALLOWED SIGNERS/p' | sed '1d;$d' | grep -v '^[[:space:]]*#')"
  [ "$(printf '%s\n' "$body" | grep -c .)" = 1 ] || { echo "$f: expected one line between the markers" >&2; return 1; }
  # The value is a single-quoted literal: the text between the first and last quote.
  case "$body" in *\'*\'*) ;; *) echo "$f: no single-quoted value between the markers" >&2; return 1 ;; esac
  body="${body#*\'}"
  printf '%s\n' "${body%\'*}"
}

# agree <root>: 0 when the four trust-root copies under <root> agree.
agree() {
  local root="$1" signers s e
  signers="$(tr -d '\r' < "$root/keys/allowed_signers")" || return 1
  [ "$(printf '%s\n' "$signers" | grep -c .)" = 1 ] || { echo "keys/allowed_signers must be one line" >&2; return 1; }
  for s in install.sh install.ps1; do
    e="$(embedded "$root/scripts/$s")" || return 1
    [ "$e" = "$signers" ] || { echo "scripts/$s embeds '$e', keys/allowed_signers holds '$signers'" >&2; return 1; }
  done
  if printf '%s\n' "$signers" | grep -q PLACEHOLDER; then
    grep -q PLACEHOLDER "$root/keys/release.pub" || { echo "keys/release.pub is set but keys/allowed_signers is the placeholder" >&2; return 1; }
  else
    "$check_key" "$root" > /dev/null || return 1
  fi
}

expect() { # expect <0|1> <name> <root>
  local want="$1" name="$2" rc=0 err
  err="$(agree "$3" 2>&1)" || rc=$?
  if { [ "$want" = 0 ] && [ "$rc" = 0 ]; } || { [ "$want" = 1 ] && [ "$rc" != 0 ]; }; then
    echo "ok   $name"
  else
    echo "FAIL $name (exit $rc)"; [ -z "$err" ] || echo "$err"; fails=$((fails + 1))
  fi
}

# copy <dir> <signers-line> <pub-line> [install.sh-line] [install.ps1-line]: a copy of
# the repository's scripts with the given trust-root values.
copy() {
  local d="$1" sig="$2" pub="$3" sh="${4-$2}" ps="${5-$2}"
  mkdir -p "$d/keys" "$d/scripts"
  cp "$check_key" "$d/scripts/"
  printf '%s\n' "$sig" > "$d/keys/allowed_signers"
  printf '%s\n' "$pub" > "$d/keys/release.pub"
  awk -v v="$sh" '/^# END VAULT RELEASE ALLOWED SIGNERS/{skip=0} skip{next} {print} /^# BEGIN VAULT RELEASE ALLOWED SIGNERS/{print "embedded_signers='\''" v "'\''"; skip=1}' \
    "$repo/scripts/install.sh" > "$d/scripts/install.sh"
  awk -v v="$ps" '/^# END VAULT RELEASE ALLOWED SIGNERS/{skip=0} skip{next} {print} /^# BEGIN VAULT RELEASE ALLOWED SIGNERS/{print "$EmbeddedSigners = '\''" v "'\''"; skip=1}' \
    "$repo/scripts/install.ps1" > "$d/scripts/install.ps1"
}

repo="${1:-$here/..}"
echo "trust_root_files_agree"
expect 0 "committed trust root agrees" "$repo"

# The check itself must catch disagreement (throwaway keys, never kept).
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/k"
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/other"
pub="$(cat "$tmp/k.pub")"
line="release@vault namespaces=\"vault-release\" $(awk '{print $1 " " $2}' "$tmp/k.pub")"
other="release@vault namespaces=\"vault-release\" $(awk '{print $1 " " $2}' "$tmp/other.pub")"
placeholder='PLACEHOLDER — owner must replace'
copy "$tmp/a" "$line" "$pub";                               expect 0 "real key in all four places" "$tmp/a"
copy "$tmp/b" "$placeholder" "$placeholder";                expect 0 "placeholder in all four places" "$tmp/b"
copy "$tmp/c" "$line" "$pub" "$other";                      expect 1 "install.sh embeds another key" "$tmp/c"
copy "$tmp/d" "$line" "$pub" "$line" "$other";              expect 1 "install.ps1 embeds another key" "$tmp/d"
copy "$tmp/e" "$line" "$pub" "$placeholder" "$placeholder"; expect 1 "scripts still hold the placeholder" "$tmp/e"
copy "$tmp/f" "$other" "$pub";                              expect 1 "allowed_signers does not match release.pub" "$tmp/f"
copy "$tmp/g" "$placeholder" "$pub";                        expect 1 "release.pub set, allowed_signers placeholder" "$tmp/g"
copy "$tmp/h" "$line" "$pub"
printf '# BEGIN VAULT RELEASE ALLOWED SIGNERS\n# END VAULT RELEASE ALLOWED SIGNERS\n' >> "$tmp/h/scripts/install.sh"
expect 1 "markers appear twice" "$tmp/h"

if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; exit 1; fi
