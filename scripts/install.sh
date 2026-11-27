#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Installs vaultctl, vault-dev and vault-relay from a signed GitHub release of 5V715/vault
# (Linux and macOS). See docs/install.md.
#
#   install.sh [--version vX.Y.Z] [--dir DIR]
#
# Downloads over HTTPS from github.com (no login, no GitHub CLI; curl, or else wget), then
# verifies before installing:
#   1. SHA256SUMS.sig is a valid `ssh-keygen -Y` signature over SHA256SUMS by the release
#      key embedded below (principal release@vault, namespace vault-release);
#   2. the archive's SHA-256 matches its single entry in SHA256SUMS.
# Only then is the archive extracted (in a temporary directory) and the three binaries
# installed to DIR (default ~/.local/bin) with mode 0755. Any failure installs nothing;
# the temporary directory is always removed. Needs curl or wget, ssh-keygen (OpenSSH 8.2+),
# tar and sha256sum or shasum. Downloads go only to github.com and GitHub's release-asset
# hosts, over TLS only; any other redirect is refused. Without --version, one or two
# unauthenticated GitHub API requests find the latest stable release.
set -euo pipefail

REPO=5V715/vault
BINARIES="vaultctl vault-dev vault-relay"
VERSION_RE='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
STABLE_RE='^v[0-9]+\.[0-9]+\.[0-9]+$'
WEB=https://github.com
API=https://api.github.com
# The hosts a download may be served from: github.com redirects release assets to these.
ASSET_HOSTS="github.com objects.githubusercontent.com release-assets.githubusercontent.com"

# The release trust root: must equal keys/allowed_signers (scripts/test/trust_root_test.sh
# enforces it). Replace it only together with keys/release.pub and keys/allowed_signers.
# BEGIN VAULT RELEASE ALLOWED SIGNERS
embedded_signers='PLACEHOLDER — owner must replace'
# END VAULT RELEASE ALLOWED SIGNERS

die() {
  echo "install.sh: error: $*" >&2
  exit 1
}

usage() {
  sed -n '6p' "$0" | sed 's/^# *//'
  exit "${1:-0}"
}

version=""
dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version="$2"; shift 2 || true; [ -n "$version" ] || die "invalid version '' (expected vX.Y.Z)" ;;
    --version=*) version="${1#--version=}"; shift; [ -n "$version" ] || die "invalid version '' (expected vX.Y.Z)" ;;
    --dir) [ $# -ge 2 ] || die "--dir needs a value"; dir="$2"; shift 2 ;;
    --dir=*) dir="${1#--dir=}"; shift ;;
    -h | --help) usage 0 ;;
    *) echo "install.sh: unknown argument: $1" >&2; usage 1 ;;
  esac
done
if [ -n "$version" ] && ! [[ "$version" =~ $VERSION_RE ]]; then
  die "invalid version '$version' (expected vX.Y.Z)"
fi
[ -n "$dir" ] || dir="${HOME:?HOME is not set}/.local/bin"

# Trust root. The test override is honoured only in test mode (VAULT_INSTALL_TEST=1).
signers="$embedded_signers"
if [ "${VAULT_INSTALL_TEST:-}" = 1 ] && [ -n "${VAULT_INSTALL_TEST_ALLOWED_SIGNERS:-}" ]; then
  signers="$VAULT_INSTALL_TEST_ALLOWED_SIGNERS"
fi
if ! [[ "$signers" =~ ^release@vault\ namespaces=\"vault-release\"\ ssh-ed25519\ AAAAC3NzaC1lZDI1NTE5[A-Za-z0-9+/]+=*$ ]]; then
  die "release key not configured: this copy of install.sh has no release key (the owner has not created one yet, see docs/install.md). Nothing was installed."
fi

# Test mode only: a local HTTPS stand-in for github.com (scripts/test/fake-github), and
# which downloader to use.
downloader=""
if [ "${VAULT_INSTALL_TEST:-}" = 1 ]; then
  if [ -n "${VAULT_INSTALL_TEST_BASE_URL:-}" ]; then
    [[ "$VAULT_INSTALL_TEST_BASE_URL" =~ ^https://[^/@]+(/[A-Za-z0-9._-]+)*$ ]] || die "VAULT_INSTALL_TEST_BASE_URL must be an https URL"
    WEB="$VAULT_INSTALL_TEST_BASE_URL"
    API="$VAULT_INSTALL_TEST_BASE_URL/api"
    ASSET_HOSTS="${VAULT_INSTALL_TEST_ASSET_HOSTS:-localhost 127.0.0.1}"
  fi
  downloader="${VAULT_INSTALL_TEST_DOWNLOADER:-}"
fi

for tool in ssh-keygen tar; do
  command -v "$tool" > /dev/null 2>&1 || die "$tool is required but not installed"
done
if [ -z "$downloader" ]; then
  if command -v curl > /dev/null 2>&1; then downloader=curl
  elif command -v wget > /dev/null 2>&1; then downloader=wget
  else die "curl or wget is required but neither is installed"
  fi
fi
command -v "$downloader" > /dev/null 2>&1 || die "$downloader is required but not installed"
if command -v sha256sum > /dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum > /dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "sha256sum or shasum is required but not installed"
fi

# Platform. macOS reports the hardware it runs on; no guessing under Rosetta.
os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) die "unsupported platform $os/$arch (prebuilt: Linux x86_64/aarch64, macOS arm64/x86_64; build from source instead)" ;;
esac

tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-install.XXXXXX")" || die "could not create a temporary directory"
# Install state, for cleanup and rollback. Only the fixed binary names are ever split on
# whitespace; every path is built from "$install_dir" and quoted.
install_dir=""   # set once files are staged in the destination
suffix=".vault-install.$$"
backed=""        # binaries whose previous version was moved aside to .<b>.vault-old<suffix>
placed=""        # binaries whose new version was renamed into place
installed=0
cleanup() {
  local b
  if [ -n "$install_dir" ]; then
    if [ "$installed" != 1 ]; then
      # Roll back: remove what this run placed, then put the previous versions back.
      for b in $placed; do rm -f "$install_dir/$b"; done
      for b in $backed; do
        mv -f "$install_dir/.$b.vault-old$suffix" "$install_dir/$b" ||
          echo "install.sh: error: could not restore $install_dir/$b from $install_dir/.$b.vault-old$suffix" >&2
      done
    fi
    for b in $BINARIES; do rm -f "$install_dir/.$b$suffix"; done
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$tmp/dl" "$tmp/x" "$tmp/http"

# fetch <url> <out> [api]: GET over TLS into <out>, following at most 3 redirects, each of
# which must stay https and on $ASSET_HOSTS. Sets $status to the final HTTP status and
# returns 0 only for 200. Connection and TLS failures stop the install.
status=""
host_allowed() {
  local rest="${1#https://}" host h
  [ "$rest" != "$1" ] || return 1
  host="${rest%%/*}"
  case "$host" in *@*) return 1 ;; esac
  host="${host%:*}"
  for h in $ASSET_HOSTS; do [ "$host" = "$h" ] && return 0; done
  case "$1" in "$WEB"/* | "$API"/*) return 0 ;; esac
  return 1
}
fetch() {
  local url="$1" out="$2" hops=0 res location
  local extra=()
  if [ "${3:-}" = api ]; then extra=(--header 'Accept: application/vnd.github+json' --header 'X-GitHub-Api-Version: 2022-11-28'); fi
  while :; do
    host_allowed "$url" || die "refusing to download from $url (not GitHub or not https); nothing was installed"
    rm -f "$out" "$tmp/http/headers"
    if [ "$downloader" = curl ]; then
      res="$(curl --silent --show-error --proto '=https' --tlsv1.2 --max-redirs 0 --connect-timeout 30 \
        --user-agent vault-install ${extra[@]+"${extra[@]}"} --dump-header "$tmp/http/headers" --output "$out" \
        --write-out '%{http_code} %{redirect_url}' "$url")" || die "could not connect to ${url%%\?*} (curl failed)"
      status="${res%% *}"
      location="${res#* }"
    else
      local wh=()
      if [ "${3:-}" = api ]; then wh=(--header='Accept: application/vnd.github+json' --header='X-GitHub-Api-Version: 2022-11-28'); fi
      # wget exits 8 for any HTTP error, including a redirect it did not follow: the status
      # line decides. Exit codes 4 (network) and 5 (TLS) stop the install.
      wget --quiet --server-response --https-only --secure-protocol=TLSv1_2 --max-redirect=0 --timeout=30 \
        --user-agent=vault-install ${wh[@]+"${wh[@]}"} --output-document="$out" "$url" 2> "$tmp/http/headers" || {
        local rc=$?
        [ "$rc" = 8 ] || die "could not connect to ${url%%\?*} (wget exit $rc)"
      }
      status="$(awk '$1 ~ /^HTTP\// { s = $2 } END { print s }' "$tmp/http/headers")"
      location="$(awk 'tolower($1) == "location:" { l = $2 } END { print l }' "$tmp/http/headers" | tr -d '\r')"
    fi
    case "$status" in
      200) return 0 ;;
      301 | 302 | 303 | 307 | 308)
        hops=$((hops + 1))
        [ "$hops" -le 3 ] || die "too many redirects for ${url%%\?*}"
        [ -n "$location" ] || die "redirect without a location for ${url%%\?*}"
        url="$location"
        ;;
      *) return 1 ;;
    esac
  done
}
rate_limited() {
  [ "$status" = 403 ] || [ "$status" = 429 ] || return 1
  tr -d '\r' < "$tmp/http/headers" | grep -qiE '^ *x-ratelimit-remaining: *0$'
}
api_get() { # api_get <path> <out>: an unauthenticated GitHub API request
  fetch "$API/repos/$REPO/$1" "$2" api && return 0
  if rate_limited; then
    die "the GitHub API rate limit for your address is used up (60 requests per hour without login); wait for it to reset, or name the release: --version vX.Y.Z (needs no API request)"
  fi
  return 1
}
# One "tag draft prerelease" line per release in a release (list) JSON document, from its
# "tag_name", "draft" and "prerelease" fields in document order (GitHub writes them in
# that order). Fields inside strings (a release's name or notes) have escaped quotes and
# do not match. No awk regex: the caller checks each tag against $STABLE_RE.
releases_in() {
  grep -oE '"(tag_name|draft|prerelease)"[[:space:]]*:[[:space:]]*("[^"\\]*"|true|false)' "$1" |
    sed -E 's/^"([a-z_]+)"[[:space:]]*:[[:space:]]*"?([^"]*)"?$/\1 \2/' |
    awk '$1 == "tag_name" { if (n++) print tag, draft, pre; tag = $2; draft = "?"; pre = "?" }
         $1 == "draft" { draft = $2 } $1 == "prerelease" { pre = $2 }
         END { if (n) print tag, draft, pre }'
}

if [ -z "$version" ]; then
  # The latest stable release: published (no drafts), not marked as a pre-release, and a
  # plain vX.Y.Z tag (an unmarked -rc tag is skipped too). The one GitHub marks latest if it
  # is one; otherwise the newest such release. Pre-releases install only with --version.
  if api_get releases/latest "$tmp/http/latest.json"; then
    while read -r t d p; do
      if [[ "$t" =~ $STABLE_RE ]] && [ "$d" = false ] && [ "$p" = false ]; then version="$t"; fi
      break
    done < <(releases_in "$tmp/http/latest.json")
  elif [ "$status" != 404 ]; then
    die "could not look up the latest release of $REPO (HTTP $status)"
  fi
  if [ -z "$version" ]; then
    api_get "releases?per_page=100" "$tmp/http/releases.json" ||
      die "could not list the releases of $REPO (HTTP $status)"
    while read -r t d p; do
      if [[ "$t" =~ $STABLE_RE ]] && [ "$d" = false ] && [ "$p" = false ]; then version="$t"; break; fi
    done < <(releases_in "$tmp/http/releases.json")
  fi
  [ -n "$version" ] || die "no published stable release of $REPO found (pre-releases install only with --version)"
fi
archive="vault-${version#v}-$target.tar.gz"
download() { # download <file>: the release asset into $tmp/dl
  fetch "$WEB/$REPO/releases/download/$version/$1" "$tmp/dl/$1"
}

echo "vault $version for $target"

# 1. The signature over SHA256SUMS, before anything else is downloaded.
if ! download SHA256SUMS; then
  [ "$status" = 404 ] && die "release $version not found (or it has no SHA256SUMS)"
  die "could not download SHA256SUMS of release $version (HTTP $status)"
fi
if ! download SHA256SUMS.sig; then
  [ "$status" = 404 ] && die "release $version is not signed (no SHA256SUMS.sig); nothing was installed"
  die "could not download SHA256SUMS.sig of release $version (HTTP $status)"
fi
printf '%s\n' "$signers" > "$tmp/allowed_signers"
if ! ssh-keygen -Y verify -f "$tmp/allowed_signers" -I release@vault -n vault-release \
  -s "$tmp/dl/SHA256SUMS.sig" < "$tmp/dl/SHA256SUMS" > "$tmp/verify.log" 2>&1; then
  cat "$tmp/verify.log" >&2
  die "SHA256SUMS.sig signature check failed: release $version was not signed by the vault release key; nothing was installed"
fi
echo "signature: ok (release@vault)"

# 2. The archive's hash: exactly one SHA256SUMS line must name it.
expected=""
count=0
while IFS= read -r line || [ -n "$line" ]; do
  line="${line%$'\r'}"
  [ -n "$line" ] || continue
  [[ "$line" =~ ^([0-9a-fA-F]{64})\ [\ *](.+)$ ]] || die "SHA256SUMS has a malformed line: $line"
  if [ "${BASH_REMATCH[2]}" = "$archive" ]; then
    expected="${BASH_REMATCH[1]}"
    count=$((count + 1))
  fi
done < "$tmp/dl/SHA256SUMS"
[ "$count" -gt 0 ] || die "SHA256SUMS does not list $archive (no build for $target in $version?)"
[ "$count" = 1 ] || die "SHA256SUMS lists $archive more than once; refusing an ambiguous release"
download "$archive" || die "could not download $archive (HTTP $status)"
[ -f "$tmp/dl/$archive" ] || die "could not download $archive"
actual="$(sha256 "$tmp/dl/$archive")"
expected="$(printf '%s' "$expected" | tr 'A-F' 'a-f')"
[ "$actual" = "$expected" ] || die "$archive does not match SHA256SUMS (got $actual, signed $expected); nothing was installed"
echo "sha256: ok ($archive)"

# 3. Extract (verified content only) and check the three binaries are there.
tar -xzf "$tmp/dl/$archive" -C "$tmp/x" || die "could not extract $archive"
for b in $BINARIES; do
  [ -f "$tmp/x/$b" ] && [ ! -L "$tmp/x/$b" ] || die "$archive is missing $b; nothing was installed"
done

# 4. Install, all or nothing: stage every binary next to its destination, move every
#    previous version aside, rename every new one into place; on any failure the cleanup
#    trap removes what was placed and restores the previous versions.
mkdir -p "$dir" || die "could not create $dir"
dir="$(cd "$dir" && pwd)"
for b in $BINARIES; do
  if [ -e "$dir/$b" ] && [ ! -f "$dir/$b" ]; then
    die "$dir/$b exists and is not a regular file; nothing was installed"
  fi
done
install_dir="$dir"
for b in $BINARIES; do
  if ! cp "$tmp/x/$b" "$dir/.$b$suffix" || ! chmod 0755 "$dir/.$b$suffix"; then
    die "could not write to $dir; nothing was installed"
  fi
done
for b in $BINARIES; do
  if [ -e "$dir/$b" ] || [ -L "$dir/$b" ]; then
    mv -f "$dir/$b" "$dir/.$b.vault-old$suffix" || die "could not move $dir/$b aside; nothing was installed"
    backed="$backed $b"
  fi
done
for b in $BINARIES; do
  if [ "${VAULT_INSTALL_TEST:-}" = 1 ] && [ "${VAULT_INSTALL_TEST_FAIL_RENAME:-}" = "$b" ]; then
    die "could not install $dir/$b (test hook); the previous version was restored"
  fi
  mv -f "$dir/.$b$suffix" "$dir/$b" || die "could not install $dir/$b; the previous version was restored"
  placed="$placed $b"
done
installed=1
for b in $backed; do rm -f "$dir/.$b.vault-old$suffix"; done
echo "installed $BINARIES ($version) to $dir"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "note: $dir is not on your PATH; add it, e.g. export PATH=\"$dir:\$PATH\"" ;;
esac
