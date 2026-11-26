#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Checks the built APKs (after assembleDebug and assembleRelease): the native
# libraries for every ABI, the release APK is unsigned, and the manifest and
# network-security hardening as packaged (release: system CAs only, no cleartext;
# debug: user CAs added, cleartext to loopback only), exported=true only on the
# launcher MainActivity, release not debuggable, data extraction rules excluding
# every domain, and the debug-only code (import CA setting, test hook) absent from release. The assertions live in apk-assertions.sh (self-tested by
# check-apks-selftest.sh). Needs ANDROID_HOME and ANDROID_BUILD_TOOLS.
set -euo pipefail
cd "$(dirname "$0")/.."
bt="$ANDROID_HOME/build-tools/$ANDROID_BUILD_TOOLS"
release=app/build/outputs/apk/release/app-release-unsigned.apk
debug=app/build/outputs/apk/debug/app-debug.apk
# shellcheck source=apk-assertions.sh
source ci/apk-assertions.sh

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

# An XML resource (xml/<name>) as packaged in an APK (resource paths may be shortened).
xmlres() {
  local path
  path="$("$bt/aapt2" dump resources "$1" | awk -v res=" xml/$2" '$0 ~ res "$" { found = 1; next } found && !done && /\(file\)/ { print $3; done = 1 }')"
  [ -n "$path" ] || fail "no xml/$2 in $1"
  "$bt/aapt2" dump xmltree --file "$path" "$1"
}
nsc() { xmlres "$1" network_security_config; }

for apk in "$release" "$debug"; do
  echo "== manifest of $apk"
  "$bt/aapt2" dump xmltree --file AndroidManifest.xml "$apk" > /tmp/manifest.txt
  grep -E 'allowBackup|dataExtractionRules|networkSecurityConfig|usesCleartextTraffic|debuggable' /tmp/manifest.txt
  grep -qE 'allowBackup.*false' /tmp/manifest.txt || fail "$apk: allowBackup is not false"
  grep -qE 'usesCleartextTraffic.*false' /tmp/manifest.txt || fail "$apk: usesCleartextTraffic is not false"
  grep -q 'dataExtractionRules' /tmp/manifest.txt || fail "$apk: no dataExtractionRules"
  grep -q 'networkSecurityConfig' /tmp/manifest.txt || fail "$apk: no networkSecurityConfig"
  assert_only_launcher_exported /tmp/manifest.txt "$apk"
  if [ "$apk" = "$release" ]; then
    assert_not_debuggable /tmp/manifest.txt "$apk"
  fi
done

echo "== release data extraction rules"
xmlres "$release" data_extraction_rules | tee /tmp/rules-release.txt
assert_extraction_rules_exclude_all /tmp/rules-release.txt "$release"

echo "== release network security config"
nsc "$release" | tee /tmp/nsc-release.txt
grep -q '"user"' /tmp/nsc-release.txt && fail "release trusts user CAs"
grep -q '"system"' /tmp/nsc-release.txt || fail "release does not trust system CAs"
grep -qiE 'cleartextTrafficPermitted.*true' /tmp/nsc-release.txt && fail "release permits cleartext"
grep -q 'domain-config' /tmp/nsc-release.txt && fail "release has a domain-config"

echo "== debug-only code: import CA setting, test hook (debug: present, release: absent)"
unzip -p "$debug" 'classes*.dex' > /tmp/dex-debug.bin
unzip -p "$release" 'classes*.dex' > /tmp/dex-release.bin
assert_dex_has_debug_only_code /tmp/dex-debug.bin "$debug"
assert_dex_lacks_debug_only_code /tmp/dex-release.bin "$release"

echo "== debug network security config"
nsc "$debug" | tee /tmp/nsc-debug.txt
assert_debug_nsc_loopback_only /tmp/nsc-debug.txt "$debug"
echo "APK checks passed"
