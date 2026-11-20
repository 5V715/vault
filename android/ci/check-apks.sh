#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Checks the built APKs (after assembleDebug and assembleRelease): the native
# libraries for every ABI, the release APK is unsigned, and the manifest and
# network-security hardening as packaged (release: system CAs only, no cleartext;
# debug: user CAs added). Needs ANDROID_HOME and ANDROID_BUILD_TOOLS.
set -euo pipefail
cd "$(dirname "$0")/.."
bt="$ANDROID_HOME/build-tools/$ANDROID_BUILD_TOOLS"
release=app/build/outputs/apk/release/app-release-unsigned.apk
debug=app/build/outputs/apk/debug/app-debug.apk
fail() { echo "check-apks: $*" >&2; exit 1; }

for apk in "$release" "$debug"; do
  test -s "$apk" || fail "missing $apk"
  ls -l "$apk"
  unzip -l "$apk" > /tmp/apk-list.txt
  for abi in arm64-v8a armeabi-v7a x86_64; do
    for lib in libvault_ffi.so libjnidispatch.so; do
      grep -q " lib/$abi/$lib\$" /tmp/apk-list.txt || fail "$apk lacks lib/$abi/$lib"
    done
  done
done

# Unsigned: apksigner must refuse it (the owner signs offline).
if "$bt/apksigner" verify "$release" > /dev/null 2>&1; then
  fail "$release is signed"
fi

# The network security config as packaged in an APK (resource paths may be shortened).
nsc() {
  local path
  path="$("$bt/aapt2" dump resources "$1" | awk '/ xml\/network_security_config$/ { found = 1; next } found && !done && /\(file\)/ { print $3; done = 1 }')"
  [ -n "$path" ] || fail "no xml/network_security_config in $1"
  "$bt/aapt2" dump xmltree --file "$path" "$1"
}

for apk in "$release" "$debug"; do
  echo "== manifest of $apk"
  "$bt/aapt2" dump xmltree --file AndroidManifest.xml "$apk" > /tmp/manifest.txt
  grep -E 'allowBackup|dataExtractionRules|networkSecurityConfig|usesCleartextTraffic|debuggable' /tmp/manifest.txt
  grep -qE 'allowBackup.*false' /tmp/manifest.txt || fail "$apk: allowBackup is not false"
  grep -qE 'usesCleartextTraffic.*false' /tmp/manifest.txt || fail "$apk: usesCleartextTraffic is not false"
  grep -q 'dataExtractionRules' /tmp/manifest.txt || fail "$apk: no dataExtractionRules"
  grep -q 'networkSecurityConfig' /tmp/manifest.txt || fail "$apk: no networkSecurityConfig"
done

echo "== release network security config"
nsc "$release" | tee /tmp/nsc-release.txt
grep -q '"user"' /tmp/nsc-release.txt && fail "release trusts user CAs"
grep -q '"system"' /tmp/nsc-release.txt || fail "release does not trust system CAs"
grep -qiE 'cleartextTrafficPermitted.*true' /tmp/nsc-release.txt && fail "release permits cleartext"
grep -q 'domain-config' /tmp/nsc-release.txt && fail "release has a domain-config"

echo "== debug network security config"
nsc "$debug" | tee /tmp/nsc-debug.txt
grep -q '"user"' /tmp/nsc-debug.txt || fail "debug does not trust user CAs"
echo "APK checks passed"
