#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Packages the unsigned release APK for a release, after
# `gradle.sh --no-build-cache assembleDebug assembleRelease`:
#
#   android/ci/release-apk.sh <version|""> <out-dir>
#
# Runs the APK checker's self-test and the APK checks (check-apks.sh), checks that
# the APK's versionName equals <version> (the tag without the leading v; empty:
# take the APK's own versionName, as ci.yml does on every push), and copies the
# APK to <out-dir>/vault-android-<version>-unsigned.apk. The owner signs it
# offline (scripts/sign-android-apk.sh, docs/android.md "Releases").
# Needs ANDROID_HOME and ANDROID_BUILD_TOOLS.
set -euo pipefail
[ $# = 2 ] || { echo "usage: release-apk.sh <version|\"\"> <out-dir>" >&2; exit 2; }
version="$1"
out="$2"
here="$(cd "$(dirname "$0")" && pwd -P)"
apk="$here/../app/build/outputs/apk/release/app-release-unsigned.apk"
aapt2="$ANDROID_HOME/build-tools/$ANDROID_BUILD_TOOLS/aapt2"

"$here/check-apks-selftest.sh"
"$here/check-apks.sh"

badging="$("$aapt2" dump badging "$apk" | sed -n 1p)"
echo "$badging"
name="$(sed -nE "s/^package: .* versionName='([^']*)'.*/\\1/p" <<<"$badging")"
code="$(sed -nE "s/^package: .* versionCode='([0-9]+)'.*/\\1/p" <<<"$badging")"
[ -n "$name" ] && [ -n "$code" ] || { echo "release-apk.sh: no versionName/versionCode in the APK" >&2; exit 1; }
if [ -z "$version" ]; then
  version="$name"
elif [ "$name" != "$version" ]; then
  echo "::error::the APK's versionName is '$name' but the tag is v$version: set versionName (and raise versionCode) in android/app/build.gradle.kts, then re-tag" >&2
  exit 1
fi
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "release-apk.sh: '$version' is not a version" >&2; exit 1; }

mkdir -p "$out"
dest="$out/vault-android-$version-unsigned.apk"
cp "$apk" "$dest"
echo "versionCode $code, versionName $name"
(cd "$out" && sha256sum "$(basename "$dest")")
