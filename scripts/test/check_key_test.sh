#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# release_workflow_requires_key: scripts/check-key.sh (the release workflow's first job)
# must refuse the committed placeholder trust root and any malformed or mismatched
# key, and accept only a real-looking Ed25519 key with its matching allowed_signers line.
set -uo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
check="$here/check-key.sh"
repo="$here/.."
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fails=0

expect() { # expect <0|1> <name> <root>
  local want="$1" name="$2" root="$3" out rc=0
  out="$("$check" "$root" 2>&1)" || rc=$?
  if { [ "$want" = 0 ] && [ "$rc" = 0 ]; } || { [ "$want" = 1 ] && [ "$rc" != 0 ]; }; then
    echo "ok   $name"
  else
    echo "FAIL $name (exit $rc)"; echo "$out"; fails=$((fails + 1))
  fi
}
mkroot() { # mkroot <dir> <pubfile-content> <signers-content>
  mkdir -p "$1/keys"; printf '%s\n' "$2" > "$1/keys/release.pub"; printf '%s\n' "$3" > "$1/keys/allowed_signers"
}

# A throwaway key generated here (never a real release key, never kept).
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/k"
pub="$(cat "$tmp/k.pub")"
body="$(awk '{print $1 " " $2}' "$tmp/k.pub")"
line="release@vault namespaces=\"vault-release\" $body"

# The committed files are the placeholder until the owner creates the key; if they
# are not, they must pass (the check is then the real gate).
if grep -qi placeholder "$repo/keys/release.pub" "$repo/keys/allowed_signers"; then
  expect 1 "committed placeholder is refused" "$repo"
else
  expect 0 "committed real key is accepted" "$repo"
fi
placeholder='PLACEHOLDER — owner must replace'
mkroot "$tmp/a" "$placeholder" "$placeholder";            expect 1 "placeholder in both files" "$tmp/a"
mkroot "$tmp/b" "$pub" "$placeholder";                    expect 1 "placeholder allowed_signers" "$tmp/b"
mkroot "$tmp/c" "$placeholder" "$line";                   expect 1 "placeholder release.pub" "$tmp/c"
mkroot "$tmp/d" "not a key" "release@vault namespaces=\"vault-release\" not a key"; expect 1 "garbage key" "$tmp/d"
mkroot "$tmp/e" "$pub" "release@vault namespaces=\"vault-release\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"; expect 1 "mismatched allowed_signers" "$tmp/e"
mkroot "$tmp/f" "$pub" "release@vault $body";             expect 1 "missing namespaces restriction" "$tmp/f"
mkdir -p "$tmp/g"; expect 1 "missing files" "$tmp/g"
mkroot "$tmp/h" "$pub" "$line";                           expect 0 "real-looking key accepted" "$tmp/h"

if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; exit 1; fi
