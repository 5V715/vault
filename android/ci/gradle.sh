#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Runs the Gradle wrapper in android/ with dependency verification: strict when
# gradle/verification-metadata.xml is committed, lenient (with a loud warning)
# until then. Bootstrap the file with the "android verification metadata"
# workflow (.github/workflows/android-verification-metadata.yml) and commit it.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ -f gradle/verification-metadata.xml ]; then
  echo "Gradle dependency verification: strict (gradle/verification-metadata.xml)"
  mode=strict
else
  msg="android/gradle/verification-metadata.xml is missing, so Gradle dependency verification is LENIENT: downloaded artifacts are NOT checked. Run the 'android verification metadata' workflow and commit its artifact."
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::warning title=Gradle dependency verification is lenient::$msg"
  fi
  printf '\n%s\n%s\n%s\n\n' '!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!' "WARNING: $msg" '!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!' >&2
  mode=lenient
fi
exec ./gradlew --dependency-verification="$mode" "$@"
