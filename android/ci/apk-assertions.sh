#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Assertions over `aapt2 dump xmltree` output of a built APK, sourced by
# check-apks.sh and exercised against fixture dumps by check-apks-selftest.sh.
# Each function takes a dump file and a label, and exits 1 with a message that
# starts with "check-apks:" when the assertion does not hold.

fail() { echo "check-apks: $*" >&2; exit 1; }

# Attribute value test shared by the awk programs: aapt2 prints booleans as
# `true`/`false` or as `(type 0x12)0xffffffff`/`0x0`, and raw strings quoted.
# shellcheck disable=SC2016
AWK_ATTR='
function attr_name(line,   eq, qn, parts, n) {
  sub(/^ *A: /, "", line); eq = index(line, "="); qn = substr(line, 1, eq - 1)
  sub(/\(0x[0-9a-fA-F]+\)$/, "", qn); n = split(qn, parts, ":"); return parts[n]
}
function attr_val(line) { sub(/^ *A: /, "", line); return substr(line, index(line, "=") + 1) }
function is_true(v) { return v ~ /true|0xffffffff/ }
function quoted(v) { if (match(v, /"[^"]*"/)) return substr(v, RSTART + 1, RLENGTH - 2); return v }
function indent(line) { match(line, /^ */); return RLENGTH }
'

# Components (activity, receiver, ...) with their exported flag: "<element> <name> <exported>".
manifest_components() {
  awk "$AWK_ATTR"'
    function flush() { if (kind != "") print kind, name, exported; kind = ""; name = ""; exported = "false" }
    /^ *E: / { flush(); e = $0; sub(/^ *E: /, "", e); sub(/ .*/, "", e)
               if (e ~ /^(activity|activity-alias|service|receiver|provider)$/) kind = e; next }
    /^ *A: / && kind != "" { n = attr_name($0)
               if (n == "name") name = quoted(attr_val($0))
               if (n == "exported") exported = is_true(attr_val($0)) ? "true" : "false" }
    END { flush() }' "$1"
}

# exported=true appears only on the launcher MainActivity.
assert_only_launcher_exported() {
  local dump=$1 label=$2 comps exported
  comps="$(manifest_components "$dump")"
  [ -n "$comps" ] || fail "$label: found no components in the manifest dump (aapt2 output format changed?)"
  echo "$comps" | grep -qE '^activity ([A-Za-z0-9_.]*\.)?MainActivity ' || fail "$label: MainActivity is not in the manifest"
  exported="$(echo "$comps" | awk '$3 == "true" { print $1 " " $2 }')"
  echo "$exported" | grep -qxE 'activity ([A-Za-z0-9_.]*\.)?MainActivity' ||
    fail "$label: MainActivity is not exported (it is the launcher)"
  [ "$(echo "$exported" | wc -l)" -eq 1 ] ||
    fail "$label: exported=true is allowed only on MainActivity, but the merged manifest also exports: $(echo "$exported" | grep -vE 'MainActivity$' | tr '\n' ';')"
  grep -q 'android.intent.category.LAUNCHER' "$dump" || fail "$label: MainActivity has no LAUNCHER category"
}

# The APK is not debuggable.
assert_not_debuggable() {
  local dump=$1 label=$2
  grep -q 'A: .*android:debuggable\|A: .*debuggable' "$dump" || return 0
  awk "$AWK_ATTR"'/^ *A: / && attr_name($0) == "debuggable" && is_true(attr_val($0)) { bad = 1 } END { exit bad }' "$dump" ||
    fail "$label: the APK is debuggable"
}

# Data extraction rules: every domain excluded (path ".") for both cloud-backup
# and device-transfer, and no <include> that could carve an exception.
assert_extraction_rules_exclude_all() {
  local dump=$1 label=$2 section domain lines
  lines="$(awk "$AWK_ATTR"'
    /^ *E: / { e = $0; sub(/^ *E: /, "", e); sub(/ .*/, "", e); dom = ""; path = ""
               if (e == "cloud-backup" || e == "device-transfer") section = e
               if (e == "include") print section, "include"
               cur = e; next }
    /^ *A: / && cur == "exclude" { n = attr_name($0)
               if (n == "domain") dom = quoted(attr_val($0))
               if (n == "path") path = quoted(attr_val($0))
               if (dom != "" && path != "") { print section, "exclude", dom, path; dom = ""; path = "" } }' "$dump")"
  for section in cloud-backup device-transfer; do
    echo "$lines" | grep -q "^$section " || fail "$label: data extraction rules have no <$section> section"
    echo "$lines" | grep -q "^$section include" && fail "$label: <$section> has an <include>"
    for domain in root file database sharedpref external device_root device_file device_database device_sharedpref; do
      echo "$lines" | grep -qx "$section exclude $domain \\." ||
        fail "$label: <$section> does not exclude domain '$domain' (path \".\")"
    done
  done
}

# The debug network security config: user CAs trusted, cleartext only for
# loopback hosts, and never in base-config.
assert_debug_nsc_loopback_only() {
  local dump=$1 label=$2 bad
  grep -q '"user"' "$dump" || fail "$label: the debug NSC does not trust user CAs"
  bad="$(awk -v q="'" "$AWK_ATTR"'
    /^ *E: / { e = $0; sub(/^ *E: /, "", e); sub(/ .*/, "", e); cur = e
               if (e == "domain-config") cleartext = 0
               next }
    /^ *A: / && attr_name($0) == "cleartextTrafficPermitted" && is_true(attr_val($0)) {
               if (cur == "base-config") print "base-config permits cleartext"
               if (cur == "domain-config") cleartext = 1
               next }
    /^ *T: / && cleartext { v = $0; sub(/^ *T: /, "", v); gsub("^[" q "\"]|[" q "\"]$", "", v)
               if (v !~ /^(localhost|127\.0\.0\.1|::1|\[::1\])$/) print "cleartext permitted for non-loopback domain " v }
  ' "$dump")"
  [ -z "$bad" ] || fail "$label: debug NSC is not loopback-only: $(echo "$bad" | tr '\n' ';')"
  # Guards against a parser that sees nothing: the loopback domains must be listed.
  if ! grep -q 'localhost' "$dump" || ! grep -qE '127\.0\.0\.1' "$dump"; then
    fail "$label: debug NSC lists no loopback domain (aapt2 output format changed?)"
  fi
}
