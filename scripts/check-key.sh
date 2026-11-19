#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Refuses to continue unless the release trust root is a real key: keys/release.pub is a
# single OpenSSH Ed25519 public key line and keys/allowed_signers holds the matching
# `release@vault` line for the `vault-release` namespace. Used by the release workflow.
set -euo pipefail
root="${1:-$(dirname "$0")/..}"
pub="$root/keys/release.pub"
signers="$root/keys/allowed_signers"

fail() {
  echo "::error::release key not set up: $1" >&2
  echo "The owner must create the release key (see docs/install.md), commit keys/release.pub and keys/allowed_signers, then re-tag. Nothing was built." >&2
  exit 1
}

[ -f "$pub" ] && [ -f "$signers" ] || fail "keys/release.pub or keys/allowed_signers is missing"
if grep -qi 'placeholder' "$pub" "$signers"; then
  fail "keys/release.pub or keys/allowed_signers still holds the placeholder"
fi
[ "$(grep -c . "$pub")" = 1 ] || fail "keys/release.pub must hold exactly one line"
key="$(awk '{print $1 " " $2}' "$pub")"
[[ "$key" =~ ^ssh-ed25519\ AAAAC3NzaC1lZDI1NTE5[A-Za-z0-9+/]+$ ]] || fail "keys/release.pub is not an OpenSSH Ed25519 public key"
[ "$(grep -c . "$signers")" = 1 ] || fail "keys/allowed_signers must hold exactly one line"
[ "$(cat "$signers")" = "release@vault namespaces=\"vault-release\" $key" ] ||
  fail "keys/allowed_signers does not match keys/release.pub"
echo "release key present: $key"
