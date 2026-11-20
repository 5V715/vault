#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# The wrapper's distributionSha256Sum must equal the checksum Gradle publishes
# next to the distribution (<distributionUrl>.sha256). The wrapper itself then
# refuses a download whose SHA-256 differs from the pin. (gradle-wrapper.jar is
# checked separately by gradle/actions/setup-gradle's wrapper validation.)
set -euo pipefail
cd "$(dirname "$0")/.."
props=gradle/wrapper/gradle-wrapper.properties
url="$(sed -n 's/^distributionUrl=//p' "$props" | sed 's/\\:/:/g')"
want="$(sed -n 's/^distributionSha256Sum=//p' "$props")"
[[ "$url" =~ ^https://services\.gradle\.org/distributions/gradle-[0-9]+(\.[0-9]+)+-bin\.zip$ ]] \
  || { echo "unexpected distributionUrl: '$url'" >&2; exit 1; }
[[ "$want" =~ ^[0-9a-f]{64}$ ]] || { echo "distributionSha256Sum is not a SHA-256: '$want'" >&2; exit 1; }
published="$(curl -fsSL --retry 3 "$url.sha256" | tr -d '[:space:]')"
echo "distribution: $url"
echo "pinned:       $want"
echo "published:    $published"
[ "$published" = "$want" ] || { echo "the pinned Gradle checksum does not match Gradle's published one" >&2; exit 1; }
