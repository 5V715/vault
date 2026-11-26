#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Signs the draft release's Android APK offline and adds it to SHA256SUMS (owner only,
# on the offline signing machine, BEFORE scripts/sign-release.sh).
#
#   scripts/sign-android-apk.sh <tag> <keystore-path> [<key-alias>]
#
# 1. Checks keys/android-cert.sha256 holds the signing certificate's SHA-256 (not the
#    placeholder), the release <tag> is a draft, not yet signed (no SHA256SUMS.sig) and
#    has no signed APK yet, and the keystore lives outside the repository.
# 2. Downloads SHA256SUMS and vault-android-<version>-unsigned.apk and checks the APK's
#    hash against its one SHA256SUMS line (as CI wrote it).
# 3. Signs it: apksigner sign --ks <keystore> with the v2 and v3 schemes only (minSdk 30
#    needs no v1; no v4 file). apksigner asks for the keystore password.
# 4. Verifies it: apksigner verify --print-certs must report v2 and v3, one signer, and
#    that signer's certificate SHA-256 must equal keys/android-cert.sha256.
# 5. Uploads vault-android-<version>.apk, then SHA256SUMS with its line added, and checks
#    the draft now serves exactly that SHA256SUMS. sign-release.sh signs it next.
# The keystore is only ever passed to apksigner by path. Set APKSIGNER to the apksigner
# of the SDK build tools if it is not on PATH. See docs/android.md ("Releases").
set -euo pipefail

REPO=5V715/vault
TAG_RE='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

die() {
  echo "sign-android-apk.sh: error: $*" >&2
  exit 1
}

[ $# = 2 ] || [ $# = 3 ] || { sed -n '6p' "$0" | sed 's/^# *//' >&2; exit 2; }
tag="$1"
ks="$2"
alias_args=()
if [ $# = 3 ]; then alias_args=(--ks-key-alias "$3"); fi
[[ "$tag" =~ $TAG_RE ]] || die "invalid tag '$tag' (expected vX.Y.Z)"
version="${tag#v}"
unsigned="vault-android-$version-unsigned.apk"
signed="vault-android-$version.apk"

root="$(cd "$(dirname "$0")/.." && pwd -P)"
pin_file="$root/keys/android-cert.sha256"
[ -f "$pin_file" ] || die "keys/android-cert.sha256 is missing"
pin="$(tr -d ' \r\n' < "$pin_file")"
[[ "$pin" =~ ^[0-9a-f]{64}$ ]] ||
  die "Android signing certificate not set up: keys/android-cert.sha256 must hold the certificate's SHA-256 (64 lowercase hex digits; see docs/android.md)"

# The keystore: a regular file (not a symlink) outside the repository.
[ -e "$ks" ] || die "no such keystore: $ks"
[ ! -L "$ks" ] || die "the keystore path must not be a symlink: $ks"
[ -f "$ks" ] || die "not a regular file: $ks"
ks_dir="$(cd "$(dirname "$ks")" && pwd -P)"
ks="$ks_dir/$(basename "$ks")"
case "$ks_dir/" in
  "$root"/*) die "the keystore must not be inside the repository ($root); keep it offline" ;;
esac

apksigner="${APKSIGNER:-apksigner}"
for tool in gh "$apksigner"; do
  command -v "$tool" > /dev/null 2>&1 || die "$tool is required but not installed (apksigner: Android SDK build-tools; or set APKSIGNER)"
done
gh auth status > /dev/null 2>&1 || die "gh is not logged in to GitHub; run: gh auth login"

is_draft="$(gh release view "$tag" --repo "$REPO" --json isDraft --jq .isDraft)" ||
  die "release $tag not found"
[ "$is_draft" = true ] || die "release $tag is not a draft (already published?); refusing"
assets="$(gh release view "$tag" --repo "$REPO" --json assets --jq '.assets[].name')" ||
  die "could not list the assets of $tag"
has() { printf '%s\n' "$assets" | grep -qxF "$1"; }
has SHA256SUMS.sig && die "release $tag is already signed (SHA256SUMS.sig exists); refusing"
has "$signed" && die "release $tag already has $signed; to redo it, delete that asset first (gh release delete-asset $tag $signed --repo $REPO) and check SHA256SUMS"
has "$unsigned" || die "release $tag has no $unsigned"

sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-apk.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$tmp/dl" "$tmp/out" "$tmp/check"
gh release download "$tag" --repo "$REPO" -p SHA256SUMS -p "$unsigned" -D "$tmp/dl" > /dev/null ||
  die "could not download SHA256SUMS and $unsigned of $tag"
sums="$tmp/dl/SHA256SUMS"
[ -f "$sums" ] || die "release $tag has no SHA256SUMS"
grep -Eqv '^[0-9a-f]{64}  [^ ]+$' "$sums" && die "SHA256SUMS of $tag has a malformed line; refusing"
awk '{print $2}' "$sums" | grep -qxF "$signed" && die "SHA256SUMS of $tag already lists $signed; refusing"
[ "$(awk -v f="$unsigned" '$2 == f' "$sums" | wc -l | tr -d ' ')" = 1 ] ||
  die "SHA256SUMS of $tag does not list $unsigned exactly once; refusing"
want="$(awk -v f="$unsigned" '$2 == f {print $1}' "$sums")"
got="$(sha256_file "$tmp/dl/$unsigned")"
[ "$got" = "$want" ] || die "$unsigned does not match SHA256SUMS (got $got, listed $want); refusing"
echo "$unsigned matches SHA256SUMS ($got)"

echo "signing with $ks (apksigner asks for the keystore password)"
"$apksigner" sign --ks "$ks" "${alias_args[@]}" \
  --v1-signing-enabled false --v2-signing-enabled true --v3-signing-enabled true --v4-signing-enabled false \
  --out "$tmp/out/$signed" "$tmp/dl/$unsigned" || die "apksigner sign failed; nothing was uploaded"
[ -s "$tmp/out/$signed" ] || die "apksigner wrote no APK; nothing was uploaded"

"$apksigner" verify --verbose --print-certs "$tmp/out/$signed" > "$tmp/verify.txt" 2>&1 || {
  cat "$tmp/verify.txt" >&2
  die "apksigner verify failed; nothing was uploaded"
}
grep -qx 'Verified using v2 scheme (APK Signature Scheme v2): true' "$tmp/verify.txt" ||
  { cat "$tmp/verify.txt" >&2; die "not signed with the v2 scheme; nothing was uploaded"; }
grep -qx 'Verified using v3 scheme (APK Signature Scheme v3): true' "$tmp/verify.txt" ||
  { cat "$tmp/verify.txt" >&2; die "not signed with the v3 scheme; nothing was uploaded"; }
grep -qx 'Number of signers: 1' "$tmp/verify.txt" ||
  { cat "$tmp/verify.txt" >&2; die "expected exactly one signer; nothing was uploaded"; }
cert="$(sed -nE 's/^Signer #1 certificate SHA-256 digest: ([0-9a-f]{64})$/\1/p' "$tmp/verify.txt")"
[ "$cert" = "$pin" ] ||
  die "the APK's signing certificate SHA-256 is '${cert:-none}', but keys/android-cert.sha256 holds $pin (wrong keystore or alias?); nothing was uploaded"
echo "signed: v2 + v3, certificate SHA-256 $cert (= keys/android-cert.sha256)"

# SHA256SUMS with the signed APK's line, sorted by file name like the workflow's.
printf '%s  %s\n' "$(sha256_file "$tmp/out/$signed")" "$signed" >> "$sums"
LC_ALL=C sort -k2,2 "$sums" > "$tmp/out/SHA256SUMS"

gh release upload "$tag" --repo "$REPO" "$tmp/out/$signed" > /dev/null ||
  die "upload of $signed failed; SHA256SUMS is unchanged"
gh release upload "$tag" --repo "$REPO" --clobber "$tmp/out/SHA256SUMS" > /dev/null ||
  die "upload of the new SHA256SUMS failed: delete $signed from the draft (gh release delete-asset $tag $signed --repo $REPO) and run this again"
gh release download "$tag" --repo "$REPO" -p SHA256SUMS -D "$tmp/check" > /dev/null ||
  die "could not re-check SHA256SUMS of $tag"
cmp -s "$tmp/out/SHA256SUMS" "$tmp/check/SHA256SUMS" ||
  die "the draft's SHA256SUMS is not the one just uploaded; investigate before signing"
echo "uploaded $signed; SHA256SUMS of $tag now:"
echo
cat "$tmp/out/SHA256SUMS"
echo
echo "next: scripts/sign-release.sh $tag <release-key>"
