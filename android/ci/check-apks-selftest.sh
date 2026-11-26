#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Self-test of apk-assertions.sh against small fixture dumps in testdata/ (shaped
# like `aapt2 dump xmltree` output): a good dump must pass and each bad one must
# fail with the expected message. Needs no Android tooling.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=apk-assertions.sh
source ./apk-assertions.sh
errors=0

expect_pass() { # <function> <fixture>
  if ! out="$("$1" "testdata/$2" "$2" 2>&1)"; then
    echo "selftest FAIL: $1 rejected good fixture $2: $out" >&2
    errors=$((errors + 1))
  fi
}
expect_fail() { # <function> <fixture> <message regex>
  if out="$( ("$1" "testdata/$2" "$2") 2>&1)"; then
    echo "selftest FAIL: $1 accepted bad fixture $2" >&2
    errors=$((errors + 1))
  elif ! echo "$out" | grep -qE "^check-apks: .*$3"; then
    echo "selftest FAIL: $1 on $2 failed with an unexpected message: $out" >&2
    errors=$((errors + 1))
  fi
}

expect_pass assert_only_launcher_exported manifest-good.txt
expect_fail assert_only_launcher_exported manifest-exported-receiver.txt 'ProfileInstallReceiver'
expect_fail assert_only_launcher_exported manifest-exported-receiver-hex.txt 'ProfileInstallReceiver'
expect_fail assert_only_launcher_exported nsc-debug-good.txt 'no components'

expect_pass assert_release_permissions manifest-good.txt
expect_fail assert_release_permissions manifest-extra-permission.txt 'not allowed: android.permission.READ_CONTACTS'
expect_fail assert_release_permissions manifest-extra-permission-sdk23.txt 'not allowed: android.permission.ACCESS_FINE_LOCATION'
expect_fail assert_release_permissions manifest-missing-permission.txt 'missing: android.permission.CAMERA'
expect_fail assert_release_permissions manifest-extra-declared.txt '[(]declares[)].*not allowed: dev.vault.android.EVIL'
expect_fail assert_release_permissions nsc-debug-good.txt 'no package attribute'

expect_pass assert_not_debuggable manifest-good.txt
expect_fail assert_not_debuggable manifest-debuggable.txt 'debuggable'

expect_pass assert_extraction_rules_exclude_all rules-good.txt
expect_fail assert_extraction_rules_exclude_all rules-missing-domain.txt "device-transfer.*'device_file'"
expect_fail assert_extraction_rules_exclude_all rules-include.txt 'has an <include>'
expect_fail assert_extraction_rules_exclude_all manifest-good.txt 'no <cloud-backup>'

expect_pass assert_debug_nsc_loopback_only nsc-debug-good.txt
expect_fail assert_debug_nsc_loopback_only nsc-debug-bad-domain.txt 'non-loopback domain example.com'
expect_fail assert_debug_nsc_loopback_only nsc-debug-bad-base.txt 'base-config permits cleartext'
expect_fail assert_debug_nsc_loopback_only nsc-debug-no-user-cas.txt 'does not trust user CAs'

expect_pass assert_dex_has_debug_only_code dex-debug.bin
expect_fail assert_dex_has_debug_only_code dex-release.bin "no 'Import a CA certificate'"
expect_fail assert_dex_has_debug_only_code dex-debug-no-hook.bin "no 'replacePairingForTest'"
expect_pass assert_dex_lacks_debug_only_code dex-release.bin
expect_fail assert_dex_lacks_debug_only_code dex-debug.bin "contains debug-only code \\('Import a CA certificate'\\)"
expect_fail assert_dex_lacks_debug_only_code dex-release-ca-file.bin "contains debug-only code \\('debug-extra-ca.sealed'\\)"
expect_fail assert_dex_lacks_debug_only_code dex-release-hook.bin "contains debug-only code \\('replacePairingForTest'\\)"
expect_fail assert_dex_lacks_debug_only_code empty.bin 'no dex content'

if [ "$errors" -ne 0 ]; then
  echo "check-apks-selftest: $errors failure(s)" >&2
  exit 1
fi
echo "check-apks-selftest: all assertions behave"
