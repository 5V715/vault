#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Signs and publishes a draft release (owner only, on the offline signing machine).
#
#   scripts/sign-release.sh <tag> <private-key-path>
#
# 1. Checks the trust root (keys/release.pub, keys/allowed_signers) is a real key, the
#    release <tag> is a draft and not yet signed, and the key lives outside the repository.
# 2. Downloads SHA256SUMS from the draft, checks it lists exactly the five archives of
#    <tag> and the unsigned and signed Android APKs (run scripts/sign-android-apk.sh
#    first), checks the signed APK on the draft matches its line, prints SHA256SUMS and
#    asks for confirmation.
# 3. Signs it: ssh-keygen -Y sign -n vault-release (asks for the key's passphrase).
# 4. Verifies the signature against keys/allowed_signers, as the install scripts will.
# 5. Uploads SHA256SUMS.sig and publishes the release (gh release edit --draft=false).
# The private key is only ever passed to ssh-keygen by path: never printed, never copied.
# See docs/install.md.
set -euo pipefail

REPO=5V715/vault
TAG_RE='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
TARGETS="aarch64-apple-darwin aarch64-unknown-linux-musl x86_64-apple-darwin x86_64-pc-windows-msvc x86_64-unknown-linux-musl"

die() {
  echo "sign-release.sh: error: $*" >&2
  exit 1
}

[ $# = 2 ] || { sed -n '5p' "$0" | sed 's/^# *//' >&2; exit 2; }
tag="$1"
key="$2"
[[ "$tag" =~ $TAG_RE ]] || die "invalid tag '$tag' (expected vX.Y.Z)"
version="${tag#v}"

root="$(cd "$(dirname "$0")/.." && pwd -P)"
"$root/scripts/check-key.sh" "$root" > /dev/null || exit 1

# The key: a regular file (not a symlink) outside the repository.
[ -e "$key" ] || die "no such key file: $key"
[ ! -L "$key" ] || die "the key path must not be a symlink: $key"
[ -f "$key" ] || die "not a regular file: $key"
key_dir="$(cd "$(dirname "$key")" && pwd -P)"
key="$key_dir/$(basename "$key")"
case "$key_dir/" in
  "$root"/*) die "the private key must not be inside the repository ($root); keep it offline" ;;
esac

for tool in gh ssh-keygen; do
  command -v "$tool" > /dev/null 2>&1 || die "$tool is required but not installed"
done
gh auth status > /dev/null 2>&1 || die "gh is not logged in to GitHub; run: gh auth login"

is_draft="$(gh release view "$tag" --repo "$REPO" --json isDraft --jq .isDraft)" ||
  die "release $tag not found"
[ "$is_draft" = true ] || die "release $tag is not a draft (already published?); refusing"
assets="$(gh release view "$tag" --repo "$REPO" --json assets --jq '.assets[].name')" ||
  die "could not list the assets of $tag"
if printf '%s\n' "$assets" | grep -qx 'SHA256SUMS.sig'; then
  die "release $tag is already signed (SHA256SUMS.sig exists); refusing"
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-sign.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$tmp/dl" "$tmp/check" "$tmp/apk"
gh release download "$tag" --repo "$REPO" -p SHA256SUMS -D "$tmp/dl" > /dev/null ||
  die "could not download SHA256SUMS of $tag"
sums="$tmp/dl/SHA256SUMS"
[ -f "$sums" ] || die "release $tag has no SHA256SUMS"

# Exactly the five archives and the two APKs of this version, each once, each an asset
# of the release.
apk="vault-android-$version.apk"
listed="$(awk '{print $2}' "$sums" | LC_ALL=C sort)"
expected="vault-android-$version-unsigned.apk
$apk"
for t in $TARGETS; do
  ext=tar.gz
  case "$t" in *windows*) ext=zip ;; esac
  expected="$expected
vault-$version-$t.$ext"
done
expected="$(printf '%s\n' "$expected" | sed '/^$/d' | LC_ALL=C sort)"
if ! printf '%s\n' "$listed" | grep -qxF "$apk" && printf '%s\n' "$assets" | grep -qxF "vault-android-$version-unsigned.apk"; then
  die "SHA256SUMS of $tag does not list the signed APK $apk: sign it first (scripts/sign-android-apk.sh $tag <keystore>); refusing"
fi
if grep -Eqv '^[0-9a-f]{64}  [^ ]+$' "$sums" || [ "$listed" != "$expected" ]; then
  cat "$sums" >&2
  die "SHA256SUMS of $tag does not list exactly the five archives and the two APKs of $version; refusing"
fi
for a in $listed; do
  printf '%s\n' "$assets" | grep -qxF "$a" || die "SHA256SUMS lists $a, which is not an asset of $tag"
done

# The signed APK is the one asset uploaded by hand: it must be the file SHA256SUMS lists.
sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}
gh release download "$tag" --repo "$REPO" -p "$apk" -D "$tmp/apk" > /dev/null ||
  die "could not download $apk of $tag"
want="$(awk -v f="$apk" '$2 == f {print $1}' "$sums")"
got="$(sha256_file "$tmp/apk/$apk")"
[ "$got" = "$want" ] || die "$apk on the draft does not match SHA256SUMS (got $got, listed $want); refusing"

echo "SHA256SUMS of $tag (draft):"
echo
cat "$sums"
echo
printf 'Sign this with %s and publish %s? [y/N] ' "$key" "$tag"
answer=""
read -r answer || true
case "$answer" in
  y | Y) ;;
  *) die "not confirmed; nothing was signed or uploaded" ;;
esac

ssh-keygen -q -Y sign -f "$key" -n vault-release "$sums" > /dev/null || die "signing failed; nothing was uploaded"
sig="$sums.sig"
[ -f "$sig" ] || die "ssh-keygen wrote no signature; nothing was uploaded"
if ! ssh-keygen -Y verify -f "$root/keys/allowed_signers" -I release@vault -n vault-release \
  -s "$sig" < "$sums" > "$tmp/verify.log" 2>&1; then
  cat "$tmp/verify.log" >&2
  die "the new signature does not verify against keys/allowed_signers (wrong key?); nothing was uploaded"
fi
echo "signature verifies against keys/allowed_signers"

gh release upload "$tag" --repo "$REPO" "$sig" > /dev/null || die "upload of SHA256SUMS.sig failed; the release is still a draft"
# The draft's SHA256SUMS must still be the one that was signed before it goes public.
gh release download "$tag" --repo "$REPO" -p SHA256SUMS -D "$tmp/check" > /dev/null ||
  die "could not re-check SHA256SUMS; the release is still a draft"
cmp -s "$sums" "$tmp/check/SHA256SUMS" ||
  die "SHA256SUMS of $tag changed while signing; the release is still a draft, investigate before publishing"
gh release edit "$tag" --repo "$REPO" --draft=false > /dev/null || die "publishing failed; the release is signed but still a draft"
echo "published $tag (signed SHA256SUMS)"
