#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Tests for scripts/sign-android-apk.sh against the fake gh (scripts/test/fake-gh), and
# the whole owner procedure (sign-android-apk.sh, then sign-release.sh) on one draft.
# No network, no real key. apksigner is the fake (scripts/test/fake-apksigner) unless
# VAULT_TEST_APKSIGNER (a real apksigner) and VAULT_TEST_APK (a real unsigned APK) are
# set: then a throwaway keystore is made with keytool and the real tool signs (CI does
# this in android-release-apk.yml with the APK it just built).
# Usage: scripts/test/sign_android_apk_test.sh [test-name...]
set -uo pipefail
scripts="$(cd "$(dirname "$0")/.." && pwd)"
shim="$scripts/test/fake-gh"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-apk-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
fails=0
version=1.3.0
tag="v$version"
archives="vault-$version-aarch64-apple-darwin.tar.gz vault-$version-aarch64-unknown-linux-musl.tar.gz
vault-$version-x86_64-apple-darwin.tar.gz vault-$version-x86_64-pc-windows-msvc.zip vault-$version-x86_64-unknown-linux-musl.tar.gz"
unsigned="vault-android-$version-unsigned.apk"
signed="vault-android-$version.apk"
pass=test-only-password

sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# Throwaway keys outside the repository copies (deleted with $tmp).
mkdir -p "$tmp/offline"
ssh-keygen -q -t ed25519 -N '' -C 'release@vault' -f "$tmp/offline/release"
line="release@vault namespaces=\"vault-release\" $(awk '{print $1 " " $2}' "$tmp/offline/release.pub")"
# keystore <path>: prints the certificate's SHA-256 (as apksigner reports it).
if [ -n "${VAULT_TEST_APKSIGNER:-}" ]; then
  real=1
  apksigner="$VAULT_TEST_APKSIGNER"
  keystore() {
    keytool -genkeypair -keystore "$1" -storetype PKCS12 -storepass "$pass" -alias vault \
      -keyalg RSA -keysize 3072 -validity 10000 -dname 'CN=vault test' > /dev/null 2>&1 || return 1
    keytool -exportcert -keystore "$1" -storepass "$pass" -alias vault | sha256_stdin
  }
  echo "real apksigner: $apksigner, APK: $VAULT_TEST_APK"
else
  real=0
  apksigner="$scripts/test/fake-apksigner/apksigner"
  keystore() { head -c 64 /dev/urandom > "$1"; sha256_file "$1"; }
fi
sha256_stdin() {
  if command -v sha256sum > /dev/null 2>&1; then sha256sum | awk '{print $1}'; else shasum -a 256 | awk '{print $1}'; fi
}
pin="$(keystore "$tmp/offline/android.p12")" || { echo "could not create the test keystore"; exit 1; }
other_pin="$(keystore "$tmp/offline/other.p12")" || exit 1
[ "$pin" != "$other_pin" ] || { echo "test keystores agree?"; exit 1; }

# repo <name> [pin]: a copy of the scripts with the given trust root and APK pin.
repo() {
  local r="$tmp/$1/repo"
  mkdir -p "$r/scripts" "$r/keys"
  cp "$scripts/sign-android-apk.sh" "$scripts/sign-release.sh" "$scripts/check-key.sh" "$r/scripts/"
  printf '%s\n' "$line" > "$r/keys/allowed_signers"
  cat "$tmp/offline/release.pub" > "$r/keys/release.pub"
  printf '%s\n' "${2:-$pin}" > "$r/keys/android-cert.sha256"
  echo "$r"
}

# fake <name>: a fake GitHub holding the draft release $tag, as release.yml leaves it.
fake() {
  local f="$tmp/$1/gh" d a
  d="$f/releases/$tag"
  mkdir -p "$d"
  for a in $archives; do echo "archive $a" > "$d/$a"; done
  if [ "$real" = 1 ]; then cp "$VAULT_TEST_APK" "$d/$unsigned"; else echo "unsigned apk" > "$d/$unsigned"; fi
  (cd "$d" && for a in $archives $unsigned; do printf '%s  %s\n' "$(sha256_file "$a")" "$a"; done) > "$d/SHA256SUMS"
  touch "$d/.draft"
  echo "$f"
}

# run <name> <repo> <fake> <script> <stdin> [args...]: sets $out, $rc, $log.
run() {
  local name="$1" r="$2" f="$3" script="$4" input="$5"
  shift 5
  log="$tmp/$name/gh.log"
  : > "$log"
  rc=0
  out="$(printf '%s\n' "$input" | env PATH="$shim:$PATH" VAULT_FAKE_GH_DIR="$f" VAULT_FAKE_GH_LOG="$log" \
    APKSIGNER="$apksigner" ${schemes:+VAULT_FAKE_APKSIGNER_SCHEMES="$schemes"} \
    "$BASH" "$r/scripts/$script" "$@" 2>&1)" || rc=$?
}
sign_apk() { local n="$1" r="$2" f="$3"; shift 3; run "$n" "$r" "$f" sign-android-apk.sh "$pass" "$@"; }

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
no_upload() { ! grep -q $'^release\tupload' "$log" && ! grep -q $'^release\tedit' "$log"; }
no_signed_apk() { [ ! -e "$1/releases/$tag/$signed" ]; }
sums_unchanged() { [ "$(cat "$1/releases/$tag/SHA256SUMS")" = "$2" ]; }
# The draft's SHA256SUMS lists exactly the archives and both APKs, each matching its asset.
sums_match_assets() {
  local d="$1/releases/$tag" n a h
  n="$(awk '{print $2}' "$d/SHA256SUMS" | LC_ALL=C sort | tr '\n' ' ')"
  # shellcheck disable=SC2086 # word lists
  [ "$n" = "$(printf '%s\n' $archives $unsigned $signed | LC_ALL=C sort | tr '\n' ' ')" ] || return 1
  [ "$(awk '{print $2}' "$d/SHA256SUMS" | tr '\n' ' ')" = "$n" ] || return 1 # sorted by name
  while read -r h a; do [ "$(sha256_file "$d/$a")" = "$h" ] || return 1; done < "$d/SHA256SUMS"
}
signed_apk_cert_is() {
  local v
  v="$("$apksigner" verify --verbose --print-certs "$1/releases/$tag/$signed" 2>&1)" || return 1
  grep -qx "Signer #1 certificate SHA-256 digest: $2" <<<"$v" &&
    grep -qx 'Verified using v3 scheme (APK Signature Scheme v3): true' <<<"$v"
}

apk_happy_path_then_sign_release() {
  local r f before
  r="$(repo happy)"; f="$(fake happy)"
  before="$(grep -F "$unsigned" "$f/releases/$tag/SHA256SUMS")"
  sign_apk happy "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "exits 0" succeeded
  check "uploads the signed APK" test -s "$f/releases/$tag/$signed"
  check "signed with the pinned certificate (v3)" signed_apk_cert_is "$f" "$pin"
  check "SHA256SUMS lists the archives and both APKs, matching the assets" sums_match_assets "$f"
  check "keeps the unsigned APK's line" grep -qxF "$before" "$f/releases/$tag/SHA256SUMS"
  check "uploads the APK before SHA256SUMS" test "$(grep -E $'^release\tupload' "$log" | grep -oE "$signed|SHA256SUMS\$" | tr '\n' ' ')" = "$signed SHA256SUMS "
  check "names the next step" says "scripts/sign-release.sh $tag"
  check "never prints the keystore password" bash -c "! grep -qF '$pass' <<<\"\$1\"" _ "$out"
  check "still a draft" test -e "$f/releases/$tag/.draft"
  # Then the release signature covers the signed APK.
  run happy "$r" "$f" sign-release.sh y "$tag" "$tmp/offline/release"
  check "sign-release.sh then signs and publishes" test "$rc" = 0 -a ! -e "$f/releases/$tag/.draft"
  check "the signature covers the published SHA256SUMS" ssh-keygen -Y verify -f "$r/keys/allowed_signers" \
    -I release@vault -n vault-release -s "$f/releases/$tag/SHA256SUMS.sig" -q < "$f/releases/$tag/SHA256SUMS"
  check "which lists the signed APK" grep -qE "  $signed\$" "$f/releases/$tag/SHA256SUMS"
}

apk_key_alias_is_passed() {
  [ "$real" = 0 ] || return 0
  local r f
  r="$(repo alias)"; f="$(fake alias)"
  : > "$tmp/alias.log"
  VAULT_FAKE_APKSIGNER_LOG="$tmp/alias.log" sign_apk alias "$r" "$f" "$tag" "$tmp/offline/android.p12" mykey
  check "exits 0" succeeded
  check "passes --ks-key-alias" grep -qF -- $'--ks-key-alias\tmykey' "$tmp/alias.log"
}

apk_refuses_placeholder_pin() {
  local r f
  r="$(repo placeholder 'PLACEHOLDER — owner must replace')"; f="$(fake placeholder)"
  sign_apk placeholder "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "fails" failed
  check "says the certificate is not set up" says "Android signing certificate not set up"
  check "talks to nothing" test ! -s "$log"
}

apk_refuses_wrong_keystore() {
  local r f s
  r="$(repo wrong)"; f="$(fake wrong)"
  s="$(cat "$f/releases/$tag/SHA256SUMS")"
  sign_apk wrong "$r" "$f" "$tag" "$tmp/offline/other.p12"
  check "fails" failed
  check "names both certificates" says "$other_pin"
  check "uploads nothing" no_upload
  check "SHA256SUMS unchanged" sums_unchanged "$f" "$s"
}

apk_refuses_tampered_unsigned_apk() {
  local r f
  r="$(repo tampered)"; f="$(fake tampered)"
  echo "changed" >> "$f/releases/$tag/$unsigned"
  sign_apk tampered "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "fails" failed
  check "says it does not match SHA256SUMS" says "does not match SHA256SUMS"
  check "uploads nothing" no_upload
}

apk_refuses_missing_scheme() {
  [ "$real" = 0 ] || return 0
  local r f
  r="$(repo nov3)"; f="$(fake nov3)"
  schemes=v2 sign_apk nov3 "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "no v3: fails" failed
  check "no v3: says so" says "v3 scheme"
  check "no v3: uploads nothing" no_upload
}

apk_refuses_signed_or_published_release() {
  local r f
  r="$(repo again)"; f="$(fake again)"
  echo "signed apk" > "$f/releases/$tag/$signed"
  sign_apk again "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "signed APK present: fails" failed
  check "signed APK present: says how to redo it" says "delete-asset"
  check "signed APK present: uploads nothing" no_upload
  r="$(repo sig)"; f="$(fake sig)"
  echo "sig" > "$f/releases/$tag/SHA256SUMS.sig"
  sign_apk sig "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "SHA256SUMS.sig present: fails" failed
  check "SHA256SUMS.sig present: uploads nothing" no_upload
  r="$(repo pub)"; f="$(fake pub)"
  rm "$f/releases/$tag/.draft"
  sign_apk pub "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "published: fails" failed
  check "published: says not a draft" says "not a draft"
  check "published: uploads nothing" no_upload
  r="$(repo nounsigned)"; f="$(fake nounsigned)"
  rm "$f/releases/$tag/$unsigned"
  sign_apk nounsigned "$r" "$f" "$tag" "$tmp/offline/android.p12"
  check "no unsigned APK: fails" failed
  check "no unsigned APK: uploads nothing" no_upload
}

apk_refuses_keystore_inside_repo() {
  local r f
  r="$(repo inrepo)"; f="$(fake inrepo)"
  cp "$tmp/offline/android.p12" "$r/keys/android.p12"
  sign_apk inrepo "$r" "$f" "$tag" "$r/keys/android.p12"
  check "fails" failed
  check "says the keystore must not be inside the repository" says "inside the repository"
  check "uploads nothing" no_upload
  ln -s "$tmp/offline/android.p12" "$tmp/inrepo/link.p12"
  sign_apk inrepo "$r" "$f" "$tag" "$tmp/inrepo/link.p12"
  check "symlink: refused" says "symlink"
  sign_apk inrepo "$r" "$f" "$tag" "$tmp/offline/missing.p12"
  check "missing keystore: refused" failed
}

apk_validates_arguments() {
  local r f
  r="$(repo args)"; f="$(fake args)"
  sign_apk args "$r" "$f" "1.3.0" "$tmp/offline/android.p12"
  check "tag without v: refused" says "invalid tag"
  sign_apk args "$r" "$f" "$tag"
  check "no keystore: usage" failed
  check "no keystore: talks to nothing" test ! -s "$log"
}

all="apk_happy_path_then_sign_release apk_key_alias_is_passed apk_refuses_placeholder_pin apk_refuses_wrong_keystore
apk_refuses_tampered_unsigned_apk apk_refuses_missing_scheme apk_refuses_signed_or_published_release
apk_refuses_keystore_inside_repo apk_validates_arguments"
# shellcheck disable=SC2086 # word list
[ $# -gt 0 ] || set -- $all
for t in "$@"; do
  echo "$t"
  "$t"
done
if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; exit 1; fi
