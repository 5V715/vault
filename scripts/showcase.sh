#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# One command that builds and serves everything to show the to-do example
# (examples/todo) with a real vault: relay, page server and vault-dev. Linux and macOS
# (bash 3.2 compatible). See docs/getting-started.md.
#
#   scripts/showcase.sh [options]
#
# Local demo only: the page is a --dev build (it has the "Copy pairing text" button) on
# localhost, and the vault is a throwaway one under .showcase/.
#
# With --lan the page and the relay are served over https/wss on the laptop's LAN IP for
# a phone on the home network, with a certificate from a local CA (scripts/lan-cert.sh,
# kept in DIR/ca). The page is then the QR-only release build (--dev keeps the button).
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEB_PORT=""
RELAY_PORT=8787
LAN=0
LAN_IP=""
LAN_IP_GIVEN=0
DEV=0
DIR=""
RESET=0
YES=0
INSTALL_DEPS=0
NO_BUILD=0
CHECK=0
PAIR=""
TARGET=wasm32-unknown-unknown
APP=vault-todo
RELAY_PID=""
WEB_PID=""

usage() {
  cat << 'EOF'
Usage: scripts/showcase.sh [options]

Builds the vault binaries, vault-web and the to-do example, creates a demo vault,
starts the relay and the page server, then pairs a browser with the vault.

Options:
  --lan             serve on this computer's LAN IP for a phone on the home network:
                    https page (port 8443) and wss relay (8787) on that IP only, with
                    a certificate from a local CA in DIR/ca (limited to that IP) that
                    you install on your devices (the script prints the steps and its
                    fingerprint; a new IP means a new CA). The page is the QR-only
                    release build; the phone's vault app pairs.
  --lan-ip IP       with --lan: use this IPv4 address instead of the detected one
                    (any address; detection only accepts 10/8, 172.16/12 and
                    192.168/16, and skips VPN/tunnel interfaces)
  --dev             with --lan: keep the "Copy pairing text" button and pair with a
                    demo vault (vault-dev) here instead of a phone
  --web-port N      port of the page (default 8080, with --lan 8443); open it as
                    http://localhost:N (with --lan: https://<LAN IP>:N)
  --relay-port N    port of the relay (default 8787)
  --dir DIR         demo directory (default .showcase in the repository, git-ignored)
  --reset           delete the demo vault, passphrase, page and logs first (asks y/N;
                    the local CA in DIR/ca is kept: delete that directory yourself)
  --yes             do not ask (with --reset)
  --install-deps    install a missing wasm32 target and wasm-bindgen-cli (never done
                    without this flag)
  --no-build        reuse the existing cargo and vault-web builds (the page is still
                    built for the chosen ports)
  --pair TEXT       pair once with this pairing text instead of asking for it (the text
                    lands in your shell history; prefer the prompt)
  --check           non-interactive: build, create, start, health-check, then exit 0
  -h, --help        show this help

Environment: VAULT_BIN_DIR is the directory of vault-relay, vault-dev and vaultctl
with --no-build (default target/release).
EOF
}

say() { printf '%s\n' "$*"; }
step() { printf '==> %s\n' "$*"; }
err() { printf 'showcase.sh: %s\n' "$*" >&2; }
die() { err "$*"; exit 1; }
usage_error() { err "$* (see --help)"; exit 2; }
have() { command -v "$1" > /dev/null 2>&1; }

# ---- arguments ------------------------------------------------------------------------
need_value() { [ "$2" -ge 2 ] || usage_error "$1 needs a value"; }
port_arg() {
  case "$2" in '' | *[!0-9]*) usage_error "$1: not a port number: $2" ;; esac
  [ "$2" -ge 1 ] && [ "$2" -le 65535 ] || usage_error "$1: port out of range: $2"
}

while [ $# -gt 0 ]; do
  case "$1" in
    -h | --help) usage; exit 0 ;;
    --web-port) need_value "$1" $#; port_arg "$1" "$2"; WEB_PORT="$2"; shift 2 ;;
    --relay-port) need_value "$1" $#; port_arg "$1" "$2"; RELAY_PORT="$2"; shift 2 ;;
    --dir) need_value "$1" $#; [ -n "$2" ] || usage_error "--dir must not be empty"; DIR="$2"; shift 2 ;;
    --pair) need_value "$1" $#; PAIR="$2"; shift 2 ;;
    --lan) LAN=1; shift ;;
    --lan-ip) need_value "$1" $#; LAN_IP="$2"; LAN_IP_GIVEN=1; shift 2 ;;
    --dev) DEV=1; shift ;;
    --reset) RESET=1; shift ;;
    --yes) YES=1; shift ;;
    --install-deps) INSTALL_DEPS=1; shift ;;
    --no-build) NO_BUILD=1; shift ;;
    --check) CHECK=1; shift ;;
    *) usage_error "unknown option $1" ;;
  esac
done
# ---- IPv4 addresses (for --lan) -----------------------------------------------------------
is_ipv4() { # four decimal octets 0-255, no leading zeros
  local IFS=. o n=0
  case "$1" in '' | *[!0-9.]* | .* | *. | *..*) return 1 ;; esac
  # shellcheck disable=SC2086  # split on dots on purpose
  set -- $1
  [ $# = 4 ] || return 1
  for o in "$@"; do
    case "$o" in 0) ;; 0*) return 1 ;; esac
    [ "${#o}" -le 3 ] && [ "$o" -le 255 ] || return 1
    n=$((n + 1))
  done
  [ "$n" = 4 ]
}
ip_int() {
  local IFS=.
  # shellcheck disable=SC2086
  set -- $1
  echo $((($1 << 24) + ($2 << 16) + ($3 << 8) + $4))
}
in_cidr() { # in_cidr <ip> <net/len>
  local m=$(((0xFFFFFFFF << (32 - ${2#*/})) & 0xFFFFFFFF))
  [ $(($(ip_int "$1") & m)) = $(($(ip_int "${2%/*}") & m)) ]
}
# A home-network address: RFC 1918. (Carrier-grade NAT space, 100.64/10, is Tailscale
# or a mobile uplink far more often than a home LAN: only with an explicit --lan-ip.)
is_lan_ip() {
  local c
  for c in 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16; do
    in_cidr "$1" "$c" && return 0
  done
  return 1
}
# VPN and tunnel interfaces: a phone on the home Wi-Fi cannot reach their addresses.
is_vpn_iface() {
  case "$1" in tun* | tap* | wg* | utun* | tailscale* | zt* | ipsec* | ppp*) return 0 ;; *) return 1 ;; esac
}
# Prints "<ip> <interface>": the source of the route to the internet (no packet is
# sent); when that goes through a VPN or tunnel, the first home-network address of
# another interface instead. Prints "<ip> <vpn-interface>" if there is none.
detect_lan_ip() {
  local ip="" iface="" a dev
  case "$(uname -s 2> /dev/null)" in
    Darwin)
      iface="$(route -n get default 2> /dev/null | awk '$1 == "interface:" { print $2; exit }')"
      [ -n "$iface" ] && ip="$(ipconfig getifaddr "$iface" 2> /dev/null)"
      if is_vpn_iface "$iface"; then
        for dev in en0 en1 en2 en3 en4 en5 en6 en7 en8 en9; do
          a="$(ipconfig getifaddr "$dev" 2> /dev/null | head -n1)"
          if is_ipv4 "$a" && is_lan_ip "$a"; then echo "$a $dev"; return; fi
        done
      fi
      ;;
    *)
      # shellcheck disable=SC2046  # two words: source address and interface
      set -- $(ip -4 route get 1.1.1.1 2> /dev/null |
        awk '{ for (i = 1; i < NF; i++) { if ($i == "src") s = $(i + 1); if ($i == "dev") d = $(i + 1) } print s, d; exit }')
      ip="${1:-}"
      iface="${2:-}"
      if is_vpn_iface "$iface"; then
        while read -r dev a; do
          is_vpn_iface "$dev" && continue
          if is_ipv4 "$a" && is_lan_ip "$a"; then echo "$a $dev"; return; fi
        done << EOF
$(ip -4 -o addr show scope global 2> /dev/null | awk '{ sub(/\/.*/, "", $4); print $2, $4 }')
EOF
      fi
      ;;
  esac
  echo "$ip $iface"
}

[ "$LAN" = 1 ] || [ "$LAN_IP_GIVEN" = 0 ] || usage_error "--lan-ip needs --lan"
if [ "$LAN_IP_GIVEN" = 1 ]; then
  is_ipv4 "$LAN_IP" || usage_error "--lan-ip: not an IPv4 address: $LAN_IP"
fi
if [ -z "$WEB_PORT" ]; then
  if [ "$LAN" = 1 ]; then WEB_PORT=8443; else WEB_PORT=8080; fi
fi
[ "$WEB_PORT" != "$RELAY_PORT" ] || usage_error "--web-port and --relay-port must differ"

if [ "$LAN" = 1 ]; then
  if [ "$LAN_IP_GIVEN" = 0 ]; then
    detected="$(detect_lan_ip | head -n1)"
    LAN_IP="${detected%% *}"
    LAN_IFACE="${detected#* }"
    if ! is_ipv4 "$LAN_IP"; then
      die "cannot detect this computer's LAN IPv4 address: give it with --lan-ip <IP> (see: ip -4 addr, or ipconfig getifaddr en0 on macOS)"
    fi
    if is_vpn_iface "$LAN_IFACE"; then
      die "the route to the internet goes through the VPN/tunnel interface $LAN_IFACE ($LAN_IP), and no other interface has a home-network address: give the right one with --lan-ip <IP>"
    fi
    is_lan_ip "$LAN_IP" ||
      die "the detected address $LAN_IP${LAN_IFACE:+ ($LAN_IFACE)} is not a home-network address (10/8, 172.16/12, 192.168/16): connect to your LAN, or give the right one with --lan-ip <IP>"
    step "using $LAN_IP${LAN_IFACE:+ ($LAN_IFACE)} as this computer's LAN IP (override with --lan-ip)"
  fi
fi

if [ -z "$DIR" ]; then DIR="$repo/.showcase"; fi
case "$DIR" in /*) ;; *) DIR="$PWD/$DIR" ;; esac
if [ "$LAN" = 1 ]; then
  WEB_ORIGIN="https://$LAN_IP:$WEB_PORT"
  RELAY_ORIGIN="wss://$LAN_IP:$RELAY_PORT"
  CA_DIR="$DIR/ca"
  LEAF="$CA_DIR/leaf-$LAN_IP.pem"
  LEAF_KEY="$CA_DIR/leaf-$LAN_IP.key"
  # A demo vault on this computer only with --dev; otherwise the phone is the vault.
  LOCAL_VAULT=$DEV
  # Only the LAN IP: not a VPN, a second network or a public interface.
  BIND="$LAN_IP"
else
  WEB_ORIGIN="http://localhost:$WEB_PORT"
  RELAY_ORIGIN="http://127.0.0.1:$RELAY_PORT"
  LOCAL_VAULT=1
  BIND=127.0.0.1
fi
if [ "$NO_BUILD" = 1 ]; then BIN="${VAULT_BIN_DIR:-$repo/target/release}"; else BIN="$repo/target/release"; fi

# ---- cleanup: no orphans ----------------------------------------------------------------
# shellcheck disable=SC2329  # run from the traps below
stop_pid() {
  local pid="$1" i
  [ -n "$pid" ] || return 0
  kill "$pid" 2> /dev/null || return 0
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    kill -0 "$pid" 2> /dev/null || break
    sleep 0.2
    [ "$i" = 15 ] && kill -9 "$pid" 2> /dev/null
  done
  wait "$pid" 2> /dev/null
  return 0
}
# shellcheck disable=SC2329  # run from the traps below
cleanup() {
  trap - EXIT INT TERM HUP
  stop_pid "$WEB_PID"
  stop_pid "$RELAY_PID"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
trap 'cleanup; exit 129' HUP

# ---- the demo directory -----------------------------------------------------------------
# The script only works in a directory it created itself (it holds MARKER). It refuses
# "/", $HOME, the repository, symlinks, and any existing directory without the marker, so
# neither --reset nor the later steps can touch the user's own files.
MARKER=".showcase-dir"
MARKER_TEXT="vault-showcase-dir v1"
physical() { (cd "$1" 2> /dev/null && pwd -P); }

if [ -L "$DIR" ]; then die "$DIR is a symlink: refusing to use it"; fi
if [ -e "$DIR" ]; then
  [ -d "$DIR" ] || die "$DIR exists and is not a directory"
  real="$(physical "$DIR")" || die "cannot resolve $DIR"
  for forbidden in / "$(physical "${HOME:-/nonexistent}")" "$(physical "$repo")"; do
    [ -n "$forbidden" ] || continue
    [ "$real" != "$forbidden" ] || die "refusing to use $real as the demo directory"
  done
  [ "$(cat "$DIR/$MARKER" 2> /dev/null)" = "$MARKER_TEXT" ] ||
    die "$DIR was not created by this script (no $MARKER file): use another --dir, or an empty path that does not exist yet"
else
  case "$DIR" in / | "$HOME" | "$repo") die "refusing to use $DIR as the demo directory" ;; esac
fi

if [ "$RESET" = 1 ]; then
  if [ -e "$DIR/vault" ] || [ -e "$DIR/passphrase" ] || [ -e "$DIR/web" ] || [ -e "$DIR/logs" ]; then
    if [ "$YES" != 1 ]; then
      printf 'Delete the demo vault and passphrase in %s? [y/N] ' "$DIR"
      ans=""
      read -r ans || ans=""
      case "$ans" in
        y | Y | yes | YES) ;;
        *) say; die "not confirmed: nothing was deleted" ;;
      esac
    fi
    # Only the known entries; the directory and its marker stay.
    rm -rf "$DIR/vault" "$DIR/passphrase" "$DIR/origin" "$DIR/web" "$DIR/logs"
    step "demo vault deleted"
  else
    step "nothing to reset in $DIR"
  fi
fi

# ---- preflight --------------------------------------------------------------------------
problems=0
problem() { # problem <what> <fix>
  err "$1"
  printf '    fix: %s\n' "$2" >&2
  problems=$((problems + 1))
}

locked_wasm_bindgen() {
  awk '$0 == "name = \"wasm-bindgen\"" { getline; gsub(/version = |"/, ""); print; exit }' "$repo/Cargo.lock" 2> /dev/null
}

preflight() {
  local want have_v major cargo_bin=""
  # `cargo install` puts tools into $CARGO_HOME/bin (default ~/.cargo/bin), which is
  # often not on PATH in the current shell. Use it for this run, so an installed
  # (or just installed) wasm-bindgen is found.
  if [ -n "${CARGO_HOME:-}" ]; then
    cargo_bin="$CARGO_HOME/bin"
  elif [ -n "${HOME:-}" ]; then
    cargo_bin="$HOME/.cargo/bin"
  fi
  if [ -n "$cargo_bin" ] && [ -d "$cargo_bin" ]; then
    case ":$PATH:" in
      *":$cargo_bin:"*) ;;
      *)
        PATH="$cargo_bin:$PATH"
        export PATH
        step "using $cargo_bin for this run (add it to your PATH to make it permanent)"
        ;;
    esac
  fi
  have curl || problem "curl not found" "install curl with your package manager (apt install curl, brew install curl)"
  if [ "$LAN" = 1 ]; then
    have openssl || problem "openssl not found (--lan makes its certificates with it)" "install openssl with your package manager (apt install openssl, brew install openssl)"
  fi
  if ! have node; then
    problem "Node.js not found (22 or newer is needed)" "install Node.js 22+: https://nodejs.org/ (or: nvm install 22, brew install node@22)"
  else
    major="$(node -p 'process.versions.node.split(".")[0]' 2> /dev/null || echo 0)"
    case "$major" in '' | *[!0-9]*) major=0 ;; esac
    [ "$major" -ge 22 ] || problem "Node.js $major found, but Node.js 22 or newer is needed" "install Node.js 22+: https://nodejs.org/ (or: nvm install 22, brew install node@22)"
  fi
  if [ "$NO_BUILD" != 1 ] || [ ! -d "$repo/examples/todo/node_modules" ]; then
    have npm || problem "npm not found" "npm comes with Node.js 22+: https://nodejs.org/"
  fi

  if [ "$NO_BUILD" != 1 ]; then
    if ! have cargo; then
      problem "cargo (Rust) not found" "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   (then open a new shell)"
    elif ! have rustup; then
      problem "rustup not found (the wasm32 target is managed with it)" "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    else
      if ! (cd "$repo" && rustup target list --installed 2> /dev/null | grep -qx "$TARGET"); then
        if [ "$INSTALL_DEPS" = 1 ]; then
          step "installing the $TARGET target"
          (cd "$repo" && rustup target add "$TARGET") || true
        fi
        if ! (cd "$repo" && rustup target list --installed 2> /dev/null | grep -qx "$TARGET"); then
          problem "Rust target $TARGET is not installed" "rustup target add $TARGET   (or re-run with --install-deps)"
        fi
      fi
      want="$(locked_wasm_bindgen)"
      if [ -z "$want" ]; then
        problem "cannot read the wasm-bindgen version from Cargo.lock" "run this from a complete checkout of the repository"
      else
        have_v="$(wasm-bindgen --version 2> /dev/null | awk '{print $2}')"
        if [ "$have_v" != "$want" ] && [ "$INSTALL_DEPS" = 1 ]; then
          step "installing wasm-bindgen-cli $want"
          cargo install --locked wasm-bindgen-cli --version "$want" || true
          have_v="$(wasm-bindgen --version 2> /dev/null | awk '{print $2}')"
        fi
        if [ "$have_v" != "$want" ]; then
          if [ -z "$have_v" ]; then
            problem "wasm-bindgen CLI not found (version $want is needed)" "cargo install --locked wasm-bindgen-cli --version $want   (or re-run with --install-deps)"
          else
            problem "wasm-bindgen CLI is $have_v, but Cargo.lock needs exactly $want" "cargo install --locked wasm-bindgen-cli --version $want   (or re-run with --install-deps)"
          fi
        fi
      fi
    fi
  else
    local b missing=0
    for b in vault-relay vault-dev vaultctl; do
      [ -x "$BIN/$b" ] || { err "$BIN/$b not found"; missing=1; }
    done
    [ "$missing" = 0 ] || problem "the binaries are not built (--no-build)" "run once without --no-build (or set VAULT_BIN_DIR)"
    if [ ! -f "$repo/vault-web/js/pkg/web/vault_web_bg.wasm" ] || [ ! -f "$repo/vault-web/js/dist/index.js" ]; then
      problem "vault-web is not built (--no-build)" "run once without --no-build, or: (cd vault-web/js && npm ci && npm run build && npm run build:wasm)"
    fi
  fi
  [ "$problems" = 0 ] || { err "$problems problem(s) found; nothing was built"; exit 1; }
}
preflight

# Both ports must be free before anything is built or started (curl exit 7 = refused).
port_busy() {
  local rc=0
  curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$1/" 2> /dev/null || rc=$?
  [ "$rc" != 7 ]
}
for pp in "$WEB_PORT:--web-port" "$RELAY_PORT:--relay-port"; do
  if port_busy "${pp%%:*}"; then
    die "port ${pp%%:*} is already in use by another program: stop it or choose another port with ${pp#*:}"
  fi
done

# ---- build ------------------------------------------------------------------------------
if [ "$NO_BUILD" != 1 ]; then
  step "building vault-relay, vault-dev and vaultctl (release)"
  (cd "$repo" && cargo build --release --locked --bin vault-relay --bin vault-dev --bin vaultctl) || die "cargo build failed"
  step "building vault-web"
  (cd "$repo/vault-web/js" && npm ci && npm run build && npm run build:wasm) || die "vault-web build failed"
fi
if [ "$LAN" = 1 ] && [ "$DEV" != 1 ]; then
  step "building the to-do example (release build for the relay at $RELAY_ORIGIN)"
else
  step "building the to-do example (dev build for the relay at $RELAY_ORIGIN)"
fi
if [ "$NO_BUILD" != 1 ] || [ ! -d "$repo/examples/todo/node_modules" ]; then
  (cd "$repo/examples/todo" && npm ci) || die "npm ci failed in examples/todo"
fi
if [ ! -e "$DIR" ]; then
  mkdir -p "$(dirname "$DIR")" || die "cannot create $(dirname "$DIR")"
  mkdir -m 700 "$DIR" || die "cannot create $DIR"
  printf '%s\n' "$MARKER_TEXT" > "$DIR/$MARKER" || die "cannot write $DIR/$MARKER"
fi
mkdir -p "$DIR/logs" || die "cannot create $DIR/logs"
if [ "$LAN" = 1 ]; then
  step "local CA and certificate for $LAN_IP (in $CA_DIR)"
  /bin/bash "$repo/scripts/lan-cert.sh" --dir "$CA_DIR" --ip "$LAN_IP" || die "cannot set up the local CA (see above)"
  [ -f "$LEAF" ] && [ -f "$LEAF_KEY" ] || die "lan-cert.sh did not leave a certificate for $LAN_IP in $CA_DIR"
  if is_lan_ip "$LAN_IP"; then :; else
    err "note: $LAN_IP is not a private LAN address; a phone on your home network may not reach it."
  fi
  if [ "$DEV" = 1 ]; then
    node "$repo/examples/todo/build.mjs" --relay "$RELAY_ORIGIN" --page-origin "$WEB_ORIGIN" --dev --out "$DIR/web" ||
      die "building the page failed"
  else
    node "$repo/examples/todo/build.mjs" --relay "$RELAY_ORIGIN" --page-origin "$WEB_ORIGIN" --out "$DIR/web" ||
      die "building the page failed"
  fi
else
  node "$repo/examples/todo/build.mjs" --relay "$RELAY_ORIGIN" --dev --out "$DIR/web" || die "building the page failed"
fi

# ---- demo vault -------------------------------------------------------------------------
# The passphrase is a random file with mode 0600. It reaches vaultctl through the
# environment and vault-dev as a file; it is never an argument and never printed.
if [ "$LOCAL_VAULT" != 1 ]; then
  : # --lan without --dev: the phone's vault app is the vault
elif [ -e "$DIR/vault" ]; then
  [ -s "$DIR/passphrase" ] || die "$DIR/vault exists but its passphrase file is missing: use --reset to start over"
  bound="$(cat "$DIR/origin" 2> /dev/null || true)"
  [ -n "$bound" ] || die "$DIR/origin is missing: use --reset to start over"
  [ "$bound" = "$WEB_ORIGIN" ] || die "the demo vault is bound to $bound, not $WEB_ORIGIN; use the same mode, IP and --web-port, or --reset"
  step "using the existing demo vault in $DIR/vault"
else
  if [ ! -s "$DIR/passphrase" ]; then
    gen="$(head -c 48 /dev/urandom | base64 | tr -d '/+=\n' | cut -c1-32)"
    [ "${#gen}" -ge 24 ] || die "could not generate a passphrase"
    (umask 077 && printf '%s\n' "$gen" > "$DIR/passphrase") || die "cannot write $DIR/passphrase"
    gen=""
  fi
  chmod 600 "$DIR/passphrase"
  step "creating the demo vault for app $APP at $WEB_ORIGIN"
  # init prints the recovery key and a QR code once; this is a throwaway demo vault, so
  # that output is not kept (it would otherwise end up on screen or in scrollback).
  VAULTCTL_PASSPHRASE="$(head -n1 "$DIR/passphrase")" "$BIN/vaultctl" --dir "$DIR/vault" init --app "$APP" --origin "$WEB_ORIGIN" \
    > /dev/null < /dev/null || die "vaultctl init failed"
  printf '%s\n' "$WEB_ORIGIN" > "$DIR/origin"
fi

# ---- start the relay and the page server -----------------------------------------------
# A server counts as up only when the process we started is alive, its log says it is
# listening (so it, and not another program on the port, owns the port), and the URL answers.
# With --lan the URL is the https one on the LAN IP, checked against the local CA (so the
# certificate is checked too).
wait_for() { # wait_for <name> <pid> <url> <log> <listening-text>
  local i
  for i in $(seq 1 150); do
    if ! kill -0 "$2" 2> /dev/null; then
      err "$1 exited; last lines of $4:"
      tail -n 20 "$4" >&2
      return 1
    fi
    if grep -q "$5" "$4" 2> /dev/null && probe "$3"; then
      sleep 0.2
      kill -0 "$2" 2> /dev/null && return 0
    fi
    sleep 0.2
  done
  err "$1 did not come up at $3 within 30 s; last lines of $4:"
  tail -n 20 "$4" >&2
  return 1
}

probe() { # probe <url>
  if [ "$LAN" = 1 ]; then
    local hp="${1#https://}"
    hp="${hp%%/*}"
    # SHOWCASE_TEST_CONNECT_HOST: for scripts/test/showcase_test.sh only, whose stub
    # servers cannot listen on a made-up LAN IP. The certificate is checked either way.
    curl -fsS -o /dev/null --max-time 2 --cacert "$CA_DIR/ca.pem" \
      --connect-to "$hp:${SHOWCASE_TEST_CONNECT_HOST:-$LAN_IP}:${hp##*:}" "$1" 2> /dev/null
  else
    curl -fsS -o /dev/null --max-time 2 "$1" 2> /dev/null
  fi
}

if [ "$LAN" = 1 ]; then
  step "starting the relay (wss) on $BIND:$RELAY_PORT (logs: $DIR/logs/relay.log)"
  "$BIN/vault-relay" --listen "$BIND:$RELAY_PORT" --allow-origin "$WEB_ORIGIN" --tls-cert "$LEAF" --tls-key "$LEAF_KEY" \
    > "$DIR/logs/relay.log" 2>&1 < /dev/null &
  RELAY_PID=$!
  step "serving the page (https) on $BIND:$WEB_PORT (logs: $DIR/logs/web.log)"
  node "$repo/examples/todo/serve.mjs" --port "$WEB_PORT" --dir "$DIR/web" --host "$BIND" --tls-cert "$LEAF" --tls-key "$LEAF_KEY" \
    > "$DIR/logs/web.log" 2>&1 < /dev/null &
  WEB_PID=$!
  wait_for "the relay" "$RELAY_PID" "https://$LAN_IP:$RELAY_PORT/healthz" "$DIR/logs/relay.log" "listening" || exit 1
  wait_for "the page server" "$WEB_PID" "$WEB_ORIGIN/" "$DIR/logs/web.log" "serving" || exit 1
else
  step "starting the relay on 127.0.0.1:$RELAY_PORT (logs: $DIR/logs/relay.log)"
  "$BIN/vault-relay" --listen "127.0.0.1:$RELAY_PORT" --allow-origin "$WEB_ORIGIN" > "$DIR/logs/relay.log" 2>&1 < /dev/null &
  RELAY_PID=$!
  step "serving the page on 127.0.0.1:$WEB_PORT (logs: $DIR/logs/web.log)"
  node "$repo/examples/todo/serve.mjs" --port "$WEB_PORT" --dir "$DIR/web" > "$DIR/logs/web.log" 2>&1 < /dev/null &
  WEB_PID=$!
  wait_for "the relay" "$RELAY_PID" "http://127.0.0.1:$RELAY_PORT/healthz" "$DIR/logs/relay.log" "listening" || exit 1
  wait_for "the page server" "$WEB_PID" "http://127.0.0.1:$WEB_PORT/" "$DIR/logs/web.log" "serving" || exit 1
fi

# ---- LAN: how to trust the CA -------------------------------------------------------------
lan_banner() {
  local fp os scope limit
  fp="$(openssl x509 -noout -fingerprint -sha256 -in "$CA_DIR/ca.pem" 2> /dev/null | sed 's/^[^=]*=//')"
  case "$fp" in
    *:*:*) ;;
    *) die "cannot compute the SHA-256 fingerprint of $CA_DIR/ca.pem: without it the CA cannot be checked before you trust it" ;;
  esac
  os="$(uname -s 2> /dev/null)"
  scope="$(cat "$CA_DIR/ca.scope" 2> /dev/null || true)"
  case "$scope" in
    '' | any) limit="The CA is not name-constrained (this openssl cannot do it): remove it from your devices after the demo." ;;
    *) limit="The CA can only vouch for $scope and localhost; its key stays in $CA_DIR." ;;
  esac
  cat << EOF

  The to-do page:  $WEB_ORIGIN   (relay: $RELAY_ORIGIN)

  Trust the local CA once on each device that opens the page or runs the vault:
    CA file:      $CA_DIR/ca.pem
    SHA-256:      $fp
  Compare this fingerprint with the one your device shows before you trust it.
  $limit

  Android phone (the vault app): copy ca.pem to the phone, then Settings -> Security
    -> Encryption & credentials -> Install a certificate -> CA certificate (the path
    differs a little by vendor; search Settings for "CA certificate"). The Android vault
    app is not released yet; it is planned to trust user-installed CAs only in its
    debug build, so a release build would not connect to this relay.
EOF
  if [ "$os" = Darwin ]; then
    cat << EOF
  This Mac's browser: Safari and Chrome use the keychain:
      security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db '$CA_DIR/ca.pem'
    Firefox: Settings -> Privacy & Security -> Certificates -> View Certificates ->
    Authorities -> Import (tick "identify websites").
EOF
  else
    cat << EOF
  This computer's browser (Linux): Chrome/Chromium: Settings -> Privacy and security ->
    Security -> Manage certificates (the menu differs between versions), or:
      certutil -d sql:\$HOME/.pki/nssdb -A -t C,, -n 'vault showcase CA' -i '$CA_DIR/ca.pem'
    Firefox: Settings -> Privacy & Security -> Certificates -> View Certificates ->
    Authorities -> Import (tick "identify websites").
EOF
  fi
  cat << EOF
  Remove the CA from your devices when you are done with the demo.

  The phone's vault must be bound to this page:
      vaultctl init --app $APP --origin $WEB_ORIGIN

  Both ports ($WEB_PORT and $RELAY_PORT) listen on $LAN_IP only. If a host firewall blocks
  them, allow them from your home network only (for example:
  sudo ufw allow from <your LAN>/24 to any port $WEB_PORT,$RELAY_PORT proto tcp), never
  forward them from your router, and stop the demo with Ctrl-C when you are done.
  Tip: a DHCP reservation for this computer in your router keeps $LAN_IP, and so the CA
  on your phone, valid across restarts.
EOF
}

if [ "$LAN" = 1 ]; then lan_banner; fi

if [ "$CHECK" = 1 ]; then
  step "check ok: the relay answers on /healthz and the page on /"
  exit 0
fi

if [ "$LAN" = 1 ] && [ "$DEV" != 1 ]; then
  cat << EOF

  Ready. On this computer (or any device on your LAN that trusts the CA) open
  $WEB_ORIGIN, click "Connect to vault" and scan the QR code with the vault app on
  your phone. Compare the codes on both screens before you approve.

  Press Enter or Ctrl-C to stop.
EOF
  ans=""
  read -r ans || true
  exit 0
fi

# ---- pairing ----------------------------------------------------------------------------
cat << EOF

  Ready.

  1. Open $WEB_ORIGIN in your browser$([ "$LAN" = 1 ] || printf ' (use "localhost", not 127.0.0.1)').
  2. Click "Connect to vault", then "Copy pairing text".
  3. Paste the text below. vault-dev prints a 6-digit code and a prompt number such
     as [#1]. If the code is the same as the one in the browser, answer
     "y <prompt number>" (for [#1]: y 1), then click "Codes match" in the page.

  The pairing text is valid for about 60 seconds and is a secret while the QR is shown.
  The "Copy pairing text" button exists only in this development build, which is for
  local demos only. $([ "$LAN" = 1 ] && printf 'Keep these ports inside your home network.' || printf 'Do not expose these ports.') Stop everything with Ctrl-C.

EOF

once=0
[ -z "$PAIR" ] || once=1
rc=0
while :; do
  if [ -n "$PAIR" ]; then
    text="$PAIR"
    PAIR=""
  else
    printf 'Pairing text: '
    text=""
    IFS= read -r text || { say; break; }
  fi
  text="$(printf '%s' "$text" | tr -d '[:space:]')"
  case "$text" in
    vault-pair:v1:?*) ;;
    *)
      say "That is not a pairing text: it must start with vault-pair:v1: (use \"Copy pairing text\")."
      [ "$once" = 1 ] && exit 2
      continue
      ;;
  esac
  rc=0
  if [ "$LAN" = 1 ]; then
    "$BIN/vault-dev" serve --dir "$DIR/vault" --pair "$text" --passphrase-file "$DIR/passphrase" --ca-file "$CA_DIR/ca.pem" || rc=$?
  else
    "$BIN/vault-dev" serve --dir "$DIR/vault" --pair "$text" --passphrase-file "$DIR/passphrase" || rc=$?
  fi
  say "The vault session ended (exit $rc)."
  [ "$once" = 1 ] && exit "$rc"
  printf 'Pair again? [Y/n] '
  ans=""
  read -r ans || { say; break; }
  case "$ans" in n | N | no | NO) break ;; esac
done
exit 0
