#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Installs exactly the toolchain pinned in rust-toolchain.toml (plus any extra targets
# given as arguments) on a CI runner, so every job and every release uses one compiler.
set -euo pipefail
cd "$(dirname "$0")/.."
channel="$(sed -n 's/^channel *= *"\(.*\)"$/\1/p' rust-toolchain.toml)"
[[ "$channel" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "rust-toolchain.toml must pin an exact x.y.z version" >&2; exit 1; }
rustup toolchain install "$channel" --profile minimal --component rustfmt --component clippy --no-self-update
for t in "$@"; do rustup target add --toolchain "$channel" "$t"; done
rustc --version
test "$(rustc --version | cut -d' ' -f2)" = "$channel"
