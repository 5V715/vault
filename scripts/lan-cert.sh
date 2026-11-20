#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# A local certificate authority for the to-do showcase on the home network
# (scripts/showcase.sh --lan), and a server certificate from it for one IPv4 address.
# Linux and macOS (bash 3.2; OpenSSL 1.1+/3 or LibreSSL).
#
#   scripts/lan-cert.sh --dir CA_DIR --ip IPV4
#
# CA_DIR (mode 0700) holds:
#   ca.key, ca.pem          the CA (ECDSA P-256, 10 years, CA:true pathlen:0,
#                           keyCertSign/cRLSign, name-constrained, see below)
#   ca.scope                the IPv4 range the CA's name constraints permit
#   leaf-<ip>.key/.pem      the server certificate for <ip> (ECDSA P-256, 825 days,
#                           SAN IP:<ip> and DNS:localhost, serverAuth)
# The CA is created once and reused; a certificate is issued when there is none for
# the IP yet (or it no longer verifies or expires within 30 days). Key files are
# 0600 and are never printed; a key file with wider permissions is refused.
#
# Name constraints: the CA may only sign for this one IP (/32) and DNS:localhost, so a
# leaked CA key cannot be used against any other host on the network on the devices
# that trust it. A new IP therefore needs a new CA (said loudly: re-install it on the
# phone); a DHCP reservation for the computer avoids that. If this openssl cannot add
# name constraints the CA is made without them, with a warning and openssl's error.
set -uo pipefail
umask 077

DIR=""
IP=""
CA_DAYS=3650
LEAF_DAYS=825
RENEW_S=$((30 * 86400))

err() { printf 'lan-cert.sh: %s\n' "$*" >&2; }
die() { err "$*"; exit 1; }
usage_error() { err "$*"; err "usage: scripts/lan-cert.sh --dir CA_DIR --ip IPV4"; exit 2; }
say() { printf '%s\n' "$*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --dir) [ $# -ge 2 ] || usage_error "--dir needs a value"; DIR="$2"; shift 2 ;;
    --ip) [ $# -ge 2 ] || usage_error "--ip needs a value"; IP="$2"; shift 2 ;;
    -h | --help) sed -n '3,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) usage_error "unknown option $1" ;;
  esac
done
[ -n "$DIR" ] || usage_error "--dir is required"
[ -n "$IP" ] || usage_error "--ip is required"

# ---- IPv4 arithmetic ------------------------------------------------------------------
# is_ipv4 <s>: four decimal octets 0-255, no leading zeros.
is_ipv4() {
  local IFS=. o n=0
  case "$1" in *[!0-9.]* | .* | *. | *..*) return 1 ;; esac
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
ip_int() { # ip_int <a.b.c.d>
  local IFS=.
  # shellcheck disable=SC2086
  set -- $1
  echo $((($1 << 24) + ($2 << 16) + ($3 << 8) + $4))
}
int_ip() { echo "$((($1 >> 24) & 255)).$((($1 >> 16) & 255)).$((($1 >> 8) & 255)).$(($1 & 255))"; }
mask_int() { [ "$1" = 0 ] && echo 0 || echo $(((0xFFFFFFFF << (32 - $1)) & 0xFFFFFFFF)); }
in_cidr() { # in_cidr <ip> <net/len>
  local m
  m="$(mask_int "${2#*/}")"
  [ $(($(ip_int "$1") & m)) = $(($(ip_int "${2%/*}") & m)) ]
}
# The range the CA is constrained to for this IP: the IP alone.
scope_for() { echo "$1/32"; }

is_ipv4 "$IP" || usage_error "--ip: not an IPv4 address: $IP"
command -v openssl > /dev/null 2>&1 || die "openssl not found: install it (apt install openssl, brew install openssl)"

# ---- the CA directory -------------------------------------------------------------------
# check_private <path> <what>: not a symlink, owned by us, nothing for group or other.
# shellcheck disable=SC2012  # ls -l is how the mode is read portably (GNU and BSD)
mode_of() { ls -ld "$1" 2> /dev/null | cut -c1-10; }
check_private() {
  local m
  [ ! -L "$1" ] || die "$1 is a symlink: refusing to use it"
  [ -O "$1" ] || die "$1 is not owned by you: refusing to use it"
  m="$(mode_of "$1")"
  [ "$(printf '%s' "$m" | cut -c5-10)" = "------" ] ||
    die "$2 $1 has mode $m, wider than owner-only: refusing to reuse it. If nobody else could have copied it, run: chmod $([ -d "$1" ] && echo 700 || echo 600) '$1'; otherwise delete $DIR and install the new CA instead of the old one."
}

case "$DIR" in /*) ;; *) DIR="$PWD/$DIR" ;; esac
if [ -e "$DIR" ] || [ -L "$DIR" ]; then
  [ -d "$DIR" ] && [ ! -L "$DIR" ] || die "$DIR exists and is not a directory"
  check_private "$DIR" "the CA directory"
else
  mkdir -p "$(dirname "$DIR")" || die "cannot create $(dirname "$DIR")"
  mkdir -m 700 "$DIR" || die "cannot create $DIR"
fi
for f in "$DIR"/*.key; do
  [ -e "$f" ] || [ -L "$f" ] || continue
  check_private "$f" "the key file"
done

work="$(mktemp -d "${TMPDIR:-/tmp}/vault-lan-cert.XXXXXX")" || die "cannot create a temporary directory"
trap 'rm -rf "$work"' EXIT

host="$(uname -n 2> /dev/null | tr -cd 'A-Za-z0-9.-' | cut -c1-40)"
[ -n "$host" ] || host="this computer"

pubkey_of_cert() { openssl x509 -noout -pubkey -in "$1" 2> /dev/null; }
pubkey_of_key() { openssl pkey -pubout -in "$1" 2> /dev/null; }
same_key() { # same_key <cert> <key>
  local a b
  a="$(pubkey_of_cert "$1")" && b="$(pubkey_of_key "$2")" && [ -n "$a" ] && [ "$a" = "$b" ]
}
new_key() { # new_key <path>: ECDSA P-256 (SEC1 PEM), 0600
  rm -f "$1"
  openssl ecparam -name prime256v1 -genkey -noout -out "$1" 2> "$work/err" || {
    cat "$work/err" >&2
    die "openssl could not create a key"
  }
  chmod 600 "$1"
}

# ---- the CA -----------------------------------------------------------------------------
make_ca() { # make_ca <scope>
  local scope="$1" net len constraints=1
  net="${scope%/*}"
  len="${scope#*/}"
  new_key "$DIR/ca.key.new"
  write_ca_cfg() {
    {
      printf '[req]\ndistinguished_name = dn\n[dn]\n[v3_ca]\n'
      printf 'basicConstraints = critical, CA:true, pathlen:0\n'
      printf 'keyUsage = critical, keyCertSign, cRLSign\n'
      printf 'subjectKeyIdentifier = hash\n'
      if [ "$1" = 1 ]; then
        printf 'nameConstraints = critical, permitted;IP:%s/%s, permitted;DNS:localhost\n' \
          "$net" "$(int_ip "$(mask_int "$len")")"
      fi
    } > "$work/ca.cnf"
  }
  write_ca_cfg 1
  if ! openssl req -new -x509 -key "$DIR/ca.key.new" -sha256 -days "$CA_DAYS" \
    -subj "/CN=vault showcase CA ($host)" -config "$work/ca.cnf" -extensions v3_ca \
    -out "$DIR/ca.pem.new" 2> "$work/err"; then
    constraints=0
    cp "$work/err" "$work/nc-err"
    write_ca_cfg 0
    openssl req -new -x509 -key "$DIR/ca.key.new" -sha256 -days "$CA_DAYS" \
      -subj "/CN=vault showcase CA ($host)" -config "$work/ca.cnf" -extensions v3_ca \
      -out "$DIR/ca.pem.new" 2> "$work/err" || {
      cat "$work/err" >&2
      rm -f "$DIR/ca.key.new" "$DIR/ca.pem.new"
      die "openssl could not create the CA certificate"
    }
    err "warning: this openssl ($(openssl version 2> /dev/null)) cannot add name constraints:"
    sed 's/^/    /' "$work/nc-err" >&2
    err "  the CA is not limited to $scope and localhost. Remove it from your devices after the demo."
    scope=any
  fi
  if ! { mv -f "$DIR/ca.key.new" "$DIR/ca.key" && mv -f "$DIR/ca.pem.new" "$DIR/ca.pem"; }; then
    die "cannot store the CA in $DIR"
  fi
  printf '%s\n' "$scope" > "$DIR/ca.scope"
  rm -f "$DIR"/leaf-*.pem "$DIR"/leaf-*.key
  if [ "$constraints" = 1 ]; then
    say "created a new local CA in $DIR (limited to $scope and localhost)"
  else
    say "created a new local CA in $DIR"
  fi
}

scope="$(scope_for "$IP")"
if [ ! -e "$DIR/ca.key" ] && [ ! -e "$DIR/ca.pem" ]; then
  make_ca "$scope"
else
  [ -f "$DIR/ca.key" ] && [ -f "$DIR/ca.pem" ] ||
    die "$DIR has only one of ca.key and ca.pem: delete $DIR to start over"
  same_key "$DIR/ca.pem" "$DIR/ca.key" || die "$DIR/ca.key does not belong to $DIR/ca.pem: delete $DIR to start over"
  have_scope="$(cat "$DIR/ca.scope" 2> /dev/null || true)"
  if ! openssl x509 -noout -checkend "$RENEW_S" -in "$DIR/ca.pem" > /dev/null 2>&1; then
    err "the CA in $DIR expires within 30 days: making a new one."
    err "  Install the new CA on your devices and remove the old one."
    make_ca "$scope"
  elif [ "$have_scope" != any ] && ! { [ -n "$have_scope" ] && in_cidr "$IP" "$have_scope"; }; then
    err ""
    err "*** The IP changed: the CA in $DIR only vouches for ${have_scope:-another address}, not $IP."
    err "*** Making a NEW CA: re-install the CA on your phone (and browsers), remove the old one."
    err "*** A DHCP reservation for this computer in your router keeps the IP (and the CA) stable."
    err ""
    make_ca "$scope"
  else
    say "reusing the local CA in $DIR"
  fi
fi

# ---- the server certificate -------------------------------------------------------------
leaf="$DIR/leaf-$IP.pem"
leaf_key="$DIR/leaf-$IP.key"
leaf_ok() {
  [ -f "$leaf" ] && [ -f "$leaf_key" ] &&
    same_key "$leaf" "$leaf_key" &&
    openssl verify -CAfile "$DIR/ca.pem" "$leaf" > /dev/null 2>&1 &&
    openssl x509 -noout -checkend "$RENEW_S" -in "$leaf" > /dev/null 2>&1
}
if leaf_ok; then
  say "reusing the certificate for $IP"
else
  new_key "$leaf_key.new"
  {
    printf '[req]\ndistinguished_name = dn\n[dn]\n[v3_leaf]\n'
    printf 'basicConstraints = critical, CA:false\n'
    printf 'keyUsage = critical, digitalSignature\n'
    printf 'extendedKeyUsage = serverAuth\n'
    printf 'subjectAltName = IP:%s, DNS:localhost\n' "$IP"
    printf 'subjectKeyIdentifier = hash\n'
    printf 'authorityKeyIdentifier = keyid\n'
  } > "$work/leaf.cnf"
  serial="0x$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
  if ! { openssl req -new -key "$leaf_key.new" -subj "/CN=vault showcase server" -config "$work/leaf.cnf" \
    -out "$work/leaf.csr" 2> "$work/err" &&
    openssl x509 -req -in "$work/leaf.csr" -CA "$DIR/ca.pem" -CAkey "$DIR/ca.key" -set_serial "$serial" \
      -days "$LEAF_DAYS" -sha256 -extfile "$work/leaf.cnf" -extensions v3_leaf -out "$leaf.new" 2> "$work/err"; }; then
    cat "$work/err" >&2
    rm -f "$leaf_key.new" "$leaf.new"
    die "openssl could not issue the certificate for $IP"
  fi
  if ! { mv -f "$leaf_key.new" "$leaf_key" && mv -f "$leaf.new" "$leaf"; }; then
    die "cannot store the certificate in $DIR"
  fi
  openssl verify -CAfile "$DIR/ca.pem" "$leaf" > "$work/err" 2>&1 || {
    cat "$work/err" >&2
    die "the new certificate for $IP does not verify against the CA"
  }
  say "issued a certificate for $IP (valid $LEAF_DAYS days)"
fi
chmod 600 "$DIR"/*.key
exit 0
