#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Tests for scripts/showcase.sh against a fake repository tree and stub tools (cargo,
# rustup, wasm-bindgen, npm, vault-relay, vault-dev, vaultctl, and the example's
# build.mjs/serve.mjs). Nothing is built; the script runs under a minimal PATH, so a
# missing tool is really missing. Needs node (the stub servers) and curl.
# Usage: scripts/test/showcase_test.sh [test-name...]
# shellcheck disable=SC2319,SC2013,SC2012
# (SC2319: `$?` after a test is read on purpose; SC2013/SC2012: stub output and pids are
# simple words, and ls -l is how the file mode is read portably.)
set -uo pipefail
scripts="$(cd "$(dirname "$0")/.." && pwd)"
script="$scripts/showcase.sh"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/vault-showcase-test.XXXXXX")"
fails=0
selected="$*"
WB_VERSION=0.2.99

cleanup() {
  local p
  if [ -d "$tmp" ]; then
    for p in $(cat "$tmp"/*/pids 2> /dev/null | awk '{print $2}'); do kill "$p" 2> /dev/null; done
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; shift; [ $# -gt 0 ] && printf '     %s\n' "$@"; fails=$((fails + 1)); }

# check <name> <cond-exit-status> [detail...]
check() {
  local name="$1" st="$2"
  shift 2
  if [ "$st" = 0 ]; then ok "$name"; else bad "$name" "$@"; fi
}

# freeport: a random port in 20000-59999 that can be bound right now.
freeport() {
  local p
  while :; do
    p=$((RANDOM % 40000 + 20000))
    node -e 'const s=require("net").createServer().on("error",()=>process.exit(1)).listen(Number(process.argv[1]),"127.0.0.1",()=>s.close())' "$p" 2> /dev/null && { echo "$p"; return 0; }
  done
}

# pickports: sets wp and rp to two different free ports.
pickports() {
  wp="$(freeport)"
  rp="$(freeport)"
  while [ "$rp" = "$wp" ]; do rp="$(freeport)"; done
}

# ---- stub tools (templates under $tmp/stubs) ------------------------------------------
stubs="$tmp/stubs"
mkdir -p "$stubs/bins"

cat > "$stubs/bins/vault-relay" << 'EOF'
#!/usr/bin/env node
const http = require("node:http"), fs = require("node:fs");
const a = process.argv.slice(2);
fs.appendFileSync(process.env.SHOWCASE_TEST_LOG, `vault-relay ${a.join(" ")}\n`);
fs.appendFileSync(process.env.SHOWCASE_TEST_PIDS, `relay ${process.pid}\n`);
const port = Number(a[a.indexOf("--listen") + 1].split(":").pop());
http.createServer((q, r) => { if (q.url === "/healthz") r.end("ok\n"); else { r.statusCode = 404; r.end(); } })
  .listen(port, "127.0.0.1", () => console.log("vault-relay listening"));
EOF

cat > "$stubs/bins/vaultctl" << 'EOF'
#!/usr/bin/env bash
echo "vaultctl $*" >> "$SHOWCASE_TEST_LOG"
dir=""
prev=""
for a in "$@"; do [ "$prev" = "--dir" ] && dir="$a"; prev="$a"; done
pf="$(dirname "$dir")/passphrase"
if [ "${VAULTCTL_PASSPHRASE:-}" = "$(head -n1 "$pf")" ] && [ -n "${VAULTCTL_PASSPHRASE:-}" ]; then
  echo "vaultctl passphrase-from-env-matches-file" >> "$SHOWCASE_TEST_LOG"
fi
mkdir -p "$dir"
echo "Container created in $dir"
echo "Recovery key: RECOVERY-KEY-SECRET-DO-NOT-PRINT"
EOF

cat > "$stubs/bins/vault-dev" << 'EOF'
#!/usr/bin/env bash
echo "vault-dev $*" >> "$SHOWCASE_TEST_LOG"
exit "${STUB_VAULT_DEV_RC:-0}"
EOF

cat > "$stubs/build.mjs" << 'EOF'
import fs from "node:fs";
import path from "node:path";
const a = process.argv.slice(2);
fs.appendFileSync(process.env.SHOWCASE_TEST_LOG, `todo-build ${a.join(" ")}\n`);
const out = a[a.indexOf("--out") + 1];
fs.mkdirSync(out, { recursive: true });
fs.writeFileSync(path.join(out, "index.html"), "<!doctype html>\n");
fs.writeFileSync(path.join(out, ".build.json"), JSON.stringify({ relayOrigin: a[a.indexOf("--relay") + 1] }));
EOF

cat > "$stubs/serve.mjs" << 'EOF'
import http from "node:http";
import fs from "node:fs";
const a = process.argv.slice(2);
fs.appendFileSync(process.env.SHOWCASE_TEST_LOG, `todo-serve ${a.join(" ")}\n`);
fs.appendFileSync(process.env.SHOWCASE_TEST_PIDS, `web ${process.pid}\n`);
const port = Number(a[a.indexOf("--port") + 1]);
http.createServer((q, r) => r.end("<!doctype html>\n")).listen(port, "127.0.0.1", () => console.log("serving"));
EOF

cat > "$stubs/cargo" << 'EOF'
#!/usr/bin/env bash
echo "cargo $*" >> "$SHOWCASE_TEST_LOG"
case "$1" in
  build)
    mkdir -p target/release
    for b in vault-relay vault-dev vaultctl; do cp "$STUB_TEMPLATES/bins/$b" "target/release/$b"; done ;;
  install)
    cp "$STUB_TEMPLATES/wasm-bindgen" "$(dirname "$0")/wasm-bindgen" ;;
esac
EOF

cat > "$stubs/rustup" << 'EOF'
#!/usr/bin/env bash
echo "rustup $*" >> "$SHOWCASE_TEST_LOG"
if [ "$1 $2 $3" = "target list --installed" ]; then
  echo x86_64-unknown-linux-gnu
  [ "${STUB_NO_WASM_TARGET:-0}" = 1 ] || echo wasm32-unknown-unknown
fi
EOF

cat > "$stubs/wasm-bindgen" << 'EOF'
#!/usr/bin/env bash
echo "wasm-bindgen ${STUB_WB_VERSION:-0.2.99}"
EOF

cat > "$stubs/npm" << 'EOF'
#!/usr/bin/env bash
echo "npm ${PWD##*/repo/} $*" >> "$SHOWCASE_TEST_LOG"
EOF

cat > "$stubs/node-old" << 'EOF'
#!/usr/bin/env bash
echo 20
EOF
chmod +x "$stubs"/cargo "$stubs"/rustup "$stubs"/wasm-bindgen "$stubs"/npm "$stubs"/node-old "$stubs"/bins/*

# ---- fixtures -------------------------------------------------------------------------
# mkbin <name> [tool...]: a PATH directory with only the system tools the script may use,
# plus the named stubs (copied, so a stub can add tools next to itself). "node" is the real
# node; "node-old" installs a stub that reports Node 20 as "node".
mkbin() {
  local name="$1" b t
  shift
  b="$tmp/$name/bin"
  rm -rf "$b"
  mkdir -p "$b"
  for t in bash env sh awk sed grep tr head tail cat od base64 mktemp rm mkdir rmdir chmod ls cut \
    sleep date dirname basename curl wc sort uname id mv cp touch tee xargs cmp seq stat; do
    command -v "$t" 2> /dev/null | grep -q '^/' && ln -sf "$(command -v "$t")" "$b/$t"
  done
  for t in "$@"; do
    case "$t" in
      node) ln -sf "$(command -v node)" "$b/node" ;;
      node-old) rm -f "$b/node"; cp "$stubs/node-old" "$b/node" ;;
      *) cp "$stubs/$t" "$b/$t" ;;
    esac
  done
  echo "$b"
}

# mkrepo <name> [with-bins]: a fake checkout; with-bins also has built binaries/vault-web.
mkrepo() {
  local r="$tmp/$1/repo"
  mkdir -p "$r/scripts" "$r/examples/todo/node_modules" "$r/vault-web/js/pkg/web" "$r/vault-web/js/dist"
  cp "$script" "$r/scripts/showcase.sh"
  printf '[[package]]\nname = "wasm-bindgen"\nversion = "%s"\nsource = "registry+x"\n' "$WB_VERSION" > "$r/Cargo.lock"
  cp "$stubs/build.mjs" "$stubs/serve.mjs" "$r/examples/todo/"
  touch "$r/vault-web/js/pkg/web/vault_web_bg.wasm" "$r/vault-web/js/dist/index.js"
  if [ "${2:-}" = with-bins ]; then
    mkdir -p "$r/target/release"
    cp "$stubs"/bins/* "$r/target/release/"
  fi
  echo "$r"
}

# sc <name> <repo> <bin> [args...]: runs the repo's copy of the script with stdin closed;
# sets $out and $rc. The log of every stub call is $tmp/<name>/log (appended across runs).
sc() {
  local name="$1" r="$2" b="$3"
  shift 3
  mkdir -p "$tmp/$name"
  out="$(PATH="$b" CARGO_HOME="${STUB_CARGO_HOME:-$tmp/$name/no-cargo-home}" \
    SHOWCASE_TEST_LOG="$tmp/$name/log" SHOWCASE_TEST_PIDS="$tmp/$name/pids" \
    STUB_TEMPLATES="$stubs" STUB_WB_VERSION="${STUB_WB_VERSION:-$WB_VERSION}" \
    /bin/bash "$r/scripts/showcase.sh" "$@" < /dev/null 2>&1)"
  rc=$?
}

has() { printf '%s' "$out" | grep -qF -- "$1"; }
logged() { grep -qF -- "$1" "$tmp/$2/log" 2> /dev/null; }
count_log() { grep -cF -- "$1" "$tmp/$2/log" 2> /dev/null || true; }
want() { [ -z "$selected" ] || case " $selected " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ---- tests ------------------------------------------------------------------------------
t_help() {
  local n=help r b
  r="$(mkrepo $n)"; b="$(mkbin $n node)"
  sc $n "$r" "$b" --help
  check "$n: exit 0" "$rc" "rc=$rc"
  local o
  for o in --web-port --relay-port --dir --reset --yes --install-deps --no-build --check --pair; do
    has "$o"; check "$n: mentions $o" $? "$out"
  done
  sc $n "$r" "$b" -h
  check "$n: -h works" "$rc"
}

t_args() {
  local n=args r b
  r="$(mkrepo $n)"; b="$(mkbin $n node)"
  sc $n "$r" "$b" --bogus
  { [ "$rc" = 2 ] && has "unknown option"; }; check "$n: unknown option is exit 2" $? "rc=$rc $out"
  sc $n "$r" "$b" --web-port abc
  { [ "$rc" = 2 ] && has "--web-port"; }; check "$n: non-numeric port" $? "rc=$rc $out"
  sc $n "$r" "$b" --web-port 70000
  [ "$rc" = 2 ]; check "$n: port out of range" $? "rc=$rc $out"
  sc $n "$r" "$b" --relay-port
  { [ "$rc" = 2 ] && has "needs a value"; }; check "$n: missing value" $? "rc=$rc $out"
  sc $n "$r" "$b" --web-port 9000 --relay-port 9000
  { [ "$rc" = 2 ] && has "differ"; }; check "$n: same ports refused" $? "rc=$rc $out"
  [ ! -e "$tmp/$n/log" ]; check "$n: nothing was run" $?
}

t_preflight() {
  local n=preflight r b
  # No cargo at all.
  r="$(mkrepo $n)"; b="$(mkbin $n node npm)"
  sc $n "$r" "$b" --check
  { [ "$rc" != 0 ] && has "cargo" && has "sh.rustup.rs"; }; check "$n: missing cargo names the fix" $? "rc=$rc $out"
  # cargo but no wasm32 target.
  b="$(mkbin $n node npm cargo rustup wasm-bindgen)"
  STUB_NO_WASM_TARGET=1 sc $n "$r" "$b" --check
  { [ "$rc" != 0 ] && has "rustup target add wasm32-unknown-unknown"; }; check "$n: missing wasm32 target" $? "rc=$rc $out"
  # No wasm-bindgen: the exact command with the Cargo.lock version, and nothing is installed.
  b="$(mkbin $n node npm cargo rustup)"
  : > "$tmp/$n/log"
  sc $n "$r" "$b" --check
  { [ "$rc" != 0 ] && has "cargo install --locked wasm-bindgen-cli --version $WB_VERSION" && has "--install-deps"; }
  check "$n: missing wasm-bindgen names the command" $? "rc=$rc $out"
  ! logged "cargo install" $n; check "$n: nothing installed silently" $?
  # wasm-bindgen installed by cargo into $CARGO_HOME/bin, which is not on PATH: found anyway.
  b="$(mkbin $n node npm cargo rustup)"
  rm -rf "$tmp/$n/cargo-home"; mkdir -p "$tmp/$n/cargo-home/bin"
  cp "$stubs/wasm-bindgen" "$tmp/$n/cargo-home/bin/wasm-bindgen"
  STUB_CARGO_HOME="$tmp/$n/cargo-home" sc $n "$r" "$b" --check
  { ! has "wasm-bindgen CLI not found" && has "$tmp/$n/cargo-home/bin"; }
  check "$n: wasm-bindgen in CARGO_HOME/bin is found" $? "rc=$rc $out"
  # Wrong wasm-bindgen version.
  b="$(mkbin $n node npm cargo rustup wasm-bindgen)"
  STUB_WB_VERSION=0.2.1 sc $n "$r" "$b" --check
  { [ "$rc" != 0 ] && has "0.2.1" && has "$WB_VERSION"; }; check "$n: wrong wasm-bindgen version" $? "rc=$rc $out"
  # Node too old.
  b="$(mkbin $n node-old npm cargo rustup wasm-bindgen)"
  sc $n "$r" "$b" --check
  { [ "$rc" != 0 ] && has "Node.js 22"; }; check "$n: node too old" $? "rc=$rc $out"
  # No node, no npm: both reported in one run.
  b="$(mkbin $n cargo rustup wasm-bindgen)"
  sc $n "$r" "$b" --check
  { [ "$rc" != 0 ] && has "node" && has "npm"; }; check "$n: missing node and npm" $? "rc=$rc $out"
  # --install-deps installs only after being asked, then goes on to the build.
  r="$(mkrepo ${n}2)"; b="$(mkbin ${n}2 node npm cargo rustup)"
  local wp rp
  pickports
  sc ${n}2 "$r" "$b" --check --install-deps --dir "$tmp/${n}2/demo" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" = 0 ] && logged "cargo install --locked wasm-bindgen-cli --version $WB_VERSION" ${n}2; }
  check "$n: --install-deps installs wasm-bindgen" $? "rc=$rc $out"
  # --no-build with nothing built.
  r="$(mkrepo ${n}3)"; b="$(mkbin ${n}3 node npm)"
  sc ${n}3 "$r" "$b" --check --no-build
  { [ "$rc" != 0 ] && has "without --no-build"; }; check "$n: --no-build needs existing builds" $? "rc=$rc $out"
}

t_check_flow() {
  local n=flow r b wp rp d pass
  r="$(mkrepo $n)"; b="$(mkbin $n node npm cargo rustup wasm-bindgen)"
  pickports; d="$tmp/$n/demo"
  sc $n "$r" "$b" --check --dir "$d" --web-port "$wp" --relay-port "$rp"
  check "$n: --check exits 0" "$rc" "$out"
  logged "cargo build --release --locked --bin vault-relay --bin vault-dev --bin vaultctl" $n
  check "$n: cargo release build of the three binaries" $?
  logged "npm vault-web/js ci" $n && logged "npm vault-web/js run build:wasm" $n
  check "$n: vault-web npm ci + build:wasm" $?
  logged "todo-build --relay http://127.0.0.1:$rp --dev --out $d/web" $n
  check "$n: dev build for the relay port" $?
  logged "vaultctl --dir $d/vault init --app vault-todo --origin http://localhost:$wp" $n
  check "$n: vault created for app vault-todo at the web origin" $?
  logged "vaultctl passphrase-from-env-matches-file" $n
  check "$n: passphrase passed through VAULTCTL_PASSPHRASE" $?
  logged "vault-relay --listen 127.0.0.1:$rp --allow-origin http://localhost:$wp" $n
  check "$n: relay flags" $?
  logged "todo-serve --port $wp --dir $d/web" $n
  check "$n: page server flags" $?
  [ "$(ls -l "$d/passphrase" | cut -c1-10)" = "-rw-------" ]
  check "$n: passphrase file is 0600" $? "$(ls -l "$d/passphrase")"
  pass="$(head -n1 "$d/passphrase")"
  { [ "${#pass}" -ge 24 ] && ! has "$pass" && ! grep -qF -- "$pass" "$tmp/$n/log"; }
  check "$n: passphrase is long, not printed, not on any argv" $?
  ! has "RECOVERY-KEY-SECRET"; check "$n: init output (recovery key) is not shown" $?
  [ -d "$d/logs" ]; check "$n: logs directory" $?
  # Nothing is left running after --check.
  sleep 0.5
  local p dead=0
  for p in $(awk '{print $2}' "$tmp/$n/pids"); do kill -0 "$p" 2> /dev/null && dead=1; done
  [ "$dead" = 0 ]; check "$n: --check leaves no processes behind" $?

  # Second run reuses the vault and passphrase; --no-build skips cargo and vault-web.
  : > "$tmp/$n/log"
  cp "$d/passphrase" "$tmp/$n/pass.before"
  sc $n "$r" "$b" --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  check "$n: rerun with --no-build exits 0" "$rc" "$out"
  ! logged "vaultctl" $n; check "$n: existing vault is not re-created" $?
  cmp -s "$tmp/$n/pass.before" "$d/passphrase"; check "$n: passphrase reused" $?
  ! logged "cargo build" $n; check "$n: --no-build skips cargo" $?
  ! logged "npm vault-web/js" $n; check "$n: --no-build skips vault-web" $?
  logged "todo-build" $n; check "$n: --no-build still rebuilds the page for the ports" $?

  # A vault bound to another web port is not silently reused.
  local wp2
  wp2="$(freeport)"
  while [ "$wp2" = "$wp" ] || [ "$wp2" = "$rp" ]; do wp2="$(freeport)"; done
  sc $n "$r" "$b" --check --no-build --dir "$d" --web-port "$wp2" --relay-port "$rp"
  { [ "$rc" != 0 ] && has "--reset"; }; check "$n: vault bound to another origin is refused" $? "rc=$rc $out"
}

t_reset() {
  local n=reset r b wp rp d
  r="$(mkrepo $n with-bins)"; b="$(mkbin $n node npm)"
  pickports; d="$tmp/$n/demo"
  sc $n "$r" "$b" --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  check "$n: setup run" "$rc" "$out"
  cp "$d/passphrase" "$tmp/$n/pass.before"
  # stdin closed: no confirmation, nothing deleted.
  sc $n "$r" "$b" --reset --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" != 0 ] && [ -d "$d/vault" ] && cmp -s "$tmp/$n/pass.before" "$d/passphrase"; }
  check "$n: --reset without confirmation (EOF) refuses" $? "rc=$rc $out"
  # An explicit "n".
  out="$(printf 'n\n' | PATH="$b" SHOWCASE_TEST_LOG="$tmp/$n/log" SHOWCASE_TEST_PIDS="$tmp/$n/pids" STUB_TEMPLATES="$stubs" \
    /bin/bash "$r/scripts/showcase.sh" --reset --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp" 2>&1)"
  rc=$?
  { [ "$rc" != 0 ] && [ -d "$d/vault" ]; }; check "$n: answering n keeps the vault" $? "rc=$rc $out"
  # Only the demo's own files are deleted, never the whole --dir.
  echo keep > "$d/unrelated.txt"
  : > "$tmp/$n/log"
  sc $n "$r" "$b" --reset --yes --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" = 0 ] && logged "vaultctl --dir $d/vault init" $n && ! cmp -s "$tmp/$n/pass.before" "$d/passphrase"; }
  check "$n: --reset --yes wipes and re-creates with a new passphrase" $? "rc=$rc $out"
  [ -f "$d/unrelated.txt" ]; check "$n: --reset leaves unrelated files alone" $?
}

t_reset_refuses_unmarked_dir() {
  local n=reset_unmarked r b wp rp d
  r="$(mkrepo $n with-bins)"; b="$(mkbin $n node npm)"
  pickports; d="$tmp/$n/mine"
  mkdir -p "$d/vault" "$d/web" "$d/logs"; echo keep > "$d/vault/x"; echo keep > "$d/passphrase"
  chmod 755 "$d"
  sc $n "$r" "$b" --reset --yes --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" != 0 ] && [ -f "$d/vault/x" ] && [ -f "$d/passphrase" ] && [ -d "$d/web" ]; }
  check "$n: --reset refuses a dir the script did not create, deletes nothing" $? "rc=$rc $out"
  sc $n "$r" "$b" --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" != 0 ] && [ "$(ls -ld "$d" | cut -c1-10)" = "drwxr-xr-x" ] && [ ! -e "$d/.showcase-dir" ]; }
  check "$n: a foreign existing dir is refused without --reset too, mode untouched" $? "rc=$rc $out"
  sc $n "$r" "$b" --check --no-build --dir "$tmp/$n/new" --web-port "$wp" --relay-port "$rp"
  check "$n: a fresh dir is created" "$rc" "$out"
  [ "$(ls -ld "$tmp/$n/new" | cut -c1-10)" = "drwx------" ] && [ -f "$tmp/$n/new/.showcase-dir" ]
  check "$n: created dir is 0700 and marked" $?
  ln -s "$tmp/$n/new" "$tmp/$n/link"
  sc $n "$r" "$b" --reset --yes --check --no-build --dir "$tmp/$n/link" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" != 0 ] && has "symlink" && [ -d "$tmp/$n/new/vault" ]; }; check "$n: a symlinked --dir is refused" $? "rc=$rc $out"
  sc $n "$r" "$b" --check --no-build --dir "" --web-port "$wp" --relay-port "$rp"
  [ "$rc" = 2 ]; check "$n: empty --dir is a usage error" $? "rc=$rc $out"
}

t_reset_refuses_home_and_root() {
  local n=reset_home r b wp rp h
  r="$(mkrepo $n with-bins)"; b="$(mkbin $n node npm)"
  pickports; h="$tmp/$n/home"
  mkdir -p "$h/vault" "$h/web" "$h/logs"; echo keep > "$h/vault/x"; echo keep > "$h/passphrase"
  touch "$h/.showcase-dir"
  HOME="$h" sc $n "$r" "$b" --reset --yes --check --no-build --dir "$h" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" != 0 ] && [ -f "$h/vault/x" ] && [ -f "$h/passphrase" ]; }
  check "$n: HOME is refused even with a marker file" $? "rc=$rc $out"
  sc $n "$r" "$b" --reset --yes --check --no-build --dir / --web-port "$wp" --relay-port "$rp"
  [ "$rc" != 0 ]; check "$n: / is refused" $? "rc=$rc $out"
  touch "$r/.showcase-dir"
  sc $n "$r" "$b" --reset --yes --check --no-build --dir "$r" --web-port "$wp" --relay-port "$rp"
  { [ "$rc" != 0 ] && [ -d "$r/target" ]; }; check "$n: the repository root is refused" $? "rc=$rc $out"
  ! logged "vault-relay" $n; check "$n: nothing was started" $?
}

t_busy_port() {
  local n=busy r b wp rp d lp which
  r="$(mkrepo $n with-bins)"; b="$(mkbin $n node npm)"
  pickports; d="$tmp/$n/demo"
  for which in web relay; do
    lp="$wp"; [ "$which" = relay ] && lp="$rp"
    node -e 'require("http").createServer((q,r)=>r.end("foreign\n")).listen(Number(process.argv[1]),"127.0.0.1")' "$lp" &
    local npid=$!
    sleep 0.5
    sc $n "$r" "$b" --check --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
    { [ "$rc" != 0 ] && has "already in use" && has "$lp"; }; check "$n: busy $which port refused with a clear message" $? "rc=$rc $out"
    ! logged "vault-relay" $n; check "$n: nothing was launched ($which busy)" $?
    kill "$npid" 2> /dev/null; wait "$npid" 2> /dev/null
  done
}

# start_interactive <name> <repo> <bin> <args...>: runs the script with a fifo as stdin;
# sets $spid, $fifo, $outf and keeps fd 4 open to the fifo.
start_interactive() {
  local name="$1" r="$2" b="$3"
  shift 3
  fifo="$tmp/$name/in"; outf="$tmp/$name/out"
  mkdir -p "$tmp/$name"
  rm -f "$fifo"; mkfifo "$fifo"
  PATH="$b" SHOWCASE_TEST_LOG="$tmp/$name/log" SHOWCASE_TEST_PIDS="$tmp/$name/pids" STUB_TEMPLATES="$stubs" \
    /bin/bash "$r/scripts/showcase.sh" "$@" < "$fifo" > "$outf" 2>&1 &
  spid=$!
  exec 4> "$fifo"
}

wait_for_out() { # wait_for_out <file> <text>
  for _ in $(seq 1 100); do grep -qF -- "$2" "$1" 2> /dev/null && return 0; sleep 0.1; done
  return 1
}

pids_dead() { # pids_dead <name>
  local p alive
  for _ in $(seq 1 50); do
    alive=0
    for p in $(awk '{print $2}' "$tmp/$1/pids" 2> /dev/null); do kill -0 "$p" 2> /dev/null && alive=1; done
    [ "$alive" = 0 ] && return 0
    sleep 0.1
  done
  return 1
}

t_pair() {
  local n=pair r b wp rp d
  r="$(mkrepo $n with-bins)"; b="$(mkbin $n node npm)"
  pickports; d="$tmp/$n/demo"
  start_interactive $n "$r" "$b" --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  wait_for_out "$outf" "Pairing text"; check "$n: banner asks for the pairing text" $? "$(cat "$outf")"
  grep -qF "http://localhost:$wp" "$outf"; check "$n: banner shows the localhost URL" $?
  grep -qF "y <prompt number>" "$outf"; check "$n: banner explains the prompt number answer" $?
  grep -qi "local demos only" "$outf"; check "$n: banner warns the copy button is for local demos" $?
  printf 'not-a-pairing-text\n' >&4
  wait_for_out "$outf" "vault-pair:v1:"; check "$n: bad text is rejected with the expected prefix" $?
  ! logged "vault-dev" $n; check "$n: vault-dev not started for a bad text" $?
  printf '  vault-pair:v1:AAAABBBB  \n' >&4
  wait_for_out "$outf" "Pair again"; check "$n: after the session it offers to pair again" $?
  logged "vault-dev serve --dir $d/vault --pair vault-pair:v1:AAAABBBB --passphrase-file $d/passphrase" $n
  check "$n: vault-dev gets dir, trimmed pair text and the passphrase file" $?
  printf 'n\n' >&4
  wait "$spid"; rc=$?
  exec 4>&-
  check "$n: declining ends the script with 0" "$rc"
  pids_dead $n; check "$n: relay and page server are stopped" $?
}

t_cleanup_signal() {
  local n=signal r b wp rp d
  r="$(mkrepo $n with-bins)"; b="$(mkbin $n node npm)"
  pickports; d="$tmp/$n/demo"
  start_interactive $n "$r" "$b" --no-build --dir "$d" --web-port "$wp" --relay-port "$rp"
  wait_for_out "$outf" "Pairing text"; check "$n: running" $? "$(cat "$outf")"
  [ "$(wc -l < "$tmp/$n/pids" | tr -d ' ')" = 2 ]; check "$n: relay and page server are up" $?
  kill -TERM "$spid"
  wait "$spid"; rc=$?
  exec 4>&-
  [ "$rc" = 143 ]; check "$n: SIGTERM exits 143" $? "rc=$rc"
  pids_dead $n; check "$n: SIGTERM leaves no orphans" $?

  # Same for a failing start: the page server dies, the relay must not be left behind.
  local n2=startfail
  r="$(mkrepo $n2 with-bins)"; b="$(mkbin $n2 node npm)"
  printf 'process.exit(3)\n' > "$r/examples/todo/serve.mjs"
  pickports
  sc $n2 "$r" "$b" --check --no-build --dir "$tmp/$n2/demo" --web-port "$wp" --relay-port "$rp"
  [ "$rc" != 0 ]; check "$n2: a dead page server fails the run" $? "rc=$rc $out"
  has "web.log"; check "$n2: the log location is shown" $? "$out"
  pids_dead $n2; check "$n2: the relay is stopped again" $?
}

for t in help args preflight check_flow reset reset_refuses_unmarked_dir reset_refuses_home_and_root busy_port pair cleanup_signal; do
  want "$t" && "t_$t"
done

if [ "$fails" -gt 0 ]; then echo "$fails test(s) failed"; exit 1; fi
echo "all showcase tests passed"
