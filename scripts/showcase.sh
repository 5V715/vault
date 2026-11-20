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
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEB_PORT=8080
RELAY_PORT=8787
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
  --web-port N      port of the page (default 8080); open it as http://localhost:N
  --relay-port N    port of the relay (default 8787)
  --dir DIR         demo directory (default .showcase in the repository, git-ignored)
  --reset           delete the demo vault, passphrase, page and logs first (asks y/N)
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
    --reset) RESET=1; shift ;;
    --yes) YES=1; shift ;;
    --install-deps) INSTALL_DEPS=1; shift ;;
    --no-build) NO_BUILD=1; shift ;;
    --check) CHECK=1; shift ;;
    *) usage_error "unknown option $1" ;;
  esac
done
[ "$WEB_PORT" != "$RELAY_PORT" ] || usage_error "--web-port and --relay-port must differ"

if [ -z "$DIR" ]; then DIR="$repo/.showcase"; fi
case "$DIR" in /*) ;; *) DIR="$PWD/$DIR" ;; esac
WEB_ORIGIN="http://localhost:$WEB_PORT"
RELAY_ORIGIN="http://127.0.0.1:$RELAY_PORT"
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
step "building the to-do example (dev build for the relay at $RELAY_ORIGIN)"
if [ "$NO_BUILD" != 1 ] || [ ! -d "$repo/examples/todo/node_modules" ]; then
  (cd "$repo/examples/todo" && npm ci) || die "npm ci failed in examples/todo"
fi
if [ ! -e "$DIR" ]; then
  mkdir -p "$(dirname "$DIR")" || die "cannot create $(dirname "$DIR")"
  mkdir -m 700 "$DIR" || die "cannot create $DIR"
  printf '%s\n' "$MARKER_TEXT" > "$DIR/$MARKER" || die "cannot write $DIR/$MARKER"
fi
mkdir -p "$DIR/logs" || die "cannot create $DIR/logs"
node "$repo/examples/todo/build.mjs" --relay "$RELAY_ORIGIN" --dev --out "$DIR/web" || die "building the page failed"

# ---- demo vault -------------------------------------------------------------------------
# The passphrase is a random file with mode 0600. It reaches vaultctl through the
# environment and vault-dev as a file; it is never an argument and never printed.
if [ -e "$DIR/vault" ]; then
  [ -s "$DIR/passphrase" ] || die "$DIR/vault exists but its passphrase file is missing: use --reset to start over"
  bound="$(cat "$DIR/origin" 2> /dev/null || true)"
  [ -n "$bound" ] || die "$DIR/origin is missing: use --reset to start over"
  [ "$bound" = "$WEB_ORIGIN" ] || die "the demo vault is bound to $bound; use --web-port to match it, or --reset"
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
wait_for() { # wait_for <name> <pid> <url> <log> <listening-text>
  local i
  for i in $(seq 1 150); do
    if ! kill -0 "$2" 2> /dev/null; then
      err "$1 exited; last lines of $4:"
      tail -n 20 "$4" >&2
      return 1
    fi
    if grep -q "$5" "$4" 2> /dev/null && curl -fsS -o /dev/null --max-time 2 "$3" 2> /dev/null; then
      sleep 0.2
      kill -0 "$2" 2> /dev/null && return 0
    fi
    sleep 0.2
  done
  err "$1 did not come up at $3 within 30 s; last lines of $4:"
  tail -n 20 "$4" >&2
  return 1
}

step "starting the relay on 127.0.0.1:$RELAY_PORT (logs: $DIR/logs/relay.log)"
"$BIN/vault-relay" --listen "127.0.0.1:$RELAY_PORT" --allow-origin "$WEB_ORIGIN" > "$DIR/logs/relay.log" 2>&1 < /dev/null &
RELAY_PID=$!
step "serving the page on 127.0.0.1:$WEB_PORT (logs: $DIR/logs/web.log)"
node "$repo/examples/todo/serve.mjs" --port "$WEB_PORT" --dir "$DIR/web" > "$DIR/logs/web.log" 2>&1 < /dev/null &
WEB_PID=$!

wait_for "the relay" "$RELAY_PID" "http://127.0.0.1:$RELAY_PORT/healthz" "$DIR/logs/relay.log" "listening" || exit 1
wait_for "the page server" "$WEB_PID" "http://127.0.0.1:$WEB_PORT/" "$DIR/logs/web.log" "serving" || exit 1

if [ "$CHECK" = 1 ]; then
  step "check ok: the relay answers on /healthz and the page on /"
  exit 0
fi

# ---- pairing ----------------------------------------------------------------------------
cat << EOF

  Ready.

  1. Open $WEB_ORIGIN in your browser (use "localhost", not 127.0.0.1).
  2. Click "Connect to vault", then "Copy pairing text".
  3. Paste the text below. vault-dev prints a 6-digit code and a prompt number such
     as [#1]. If the code is the same as the one in the browser, answer
     "y <prompt number>" (for [#1]: y 1), then click "Codes match" in the page.

  The pairing text is valid for about 60 seconds and is a secret while the QR is shown.
  The "Copy pairing text" button exists only in this development build, which is for
  local demos only. Do not expose these ports. Stop everything with Ctrl-C.

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
  "$BIN/vault-dev" serve --dir "$DIR/vault" --pair "$text" --passphrase-file "$DIR/passphrase" || rc=$?
  say "The vault session ended (exit $rc)."
  [ "$once" = 1 ] && exit "$rc"
  printf 'Pair again? [Y/n] '
  ans=""
  read -r ans || { say; break; }
  case "$ans" in n | N | no | NO) break ;; esac
done
exit 0
