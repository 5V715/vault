#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Known-answer values for the vault v1 labels, computed independently of the Rust code.

Follows the definitions in docs/protocol/vault.md (SAS) and docs/vault/README.md
("Format v1": subkeys, recovery wrap key, recovery checksum and key text).
Needs the `cryptography` package. Run: python3 -I tools/kat/kat.py
"""

import base64
import hashlib

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

SAS_INFO = b"vault/v1/sas"
SUBKEY_INFOS = [
    ("enc", b"vault/v1/enc"),
    ("mac", b"vault/v1/mac"),
    ("manifest", b"vault/v1/manifest"),
    ("manifest_auth", b"vault/v1/manifest-auth"),
    ("replica_id", b"vault/v1/replica-id"),
]
RECOVERY_INFO = b"vault/v1/recovery"
RECOVERY_CHECK_DOMAIN = b"vault/v1/recovery-check"
RECOVERY_QR_PREFIX = "vault-recovery:v1:"


def hkdf_sha256(ikm: bytes, info: bytes, length: int) -> bytes:
    # salt=None is RFC 5869's default (HashLen zero bytes).
    return HKDF(algorithm=hashes.SHA256(), length=length, salt=None, info=info).derive(ikm)


def sas_from_hash(handshake_hash: bytes) -> tuple[bytes, str]:
    okm = hkdf_sha256(handshake_hash, SAS_INFO, 4)
    return okm, "%06d" % (int.from_bytes(okm, "big") % 1_000_000)


def recovery_checksum(key: bytes) -> bytes:
    return hashlib.sha256(RECOVERY_CHECK_DOMAIN + key).digest()[:8]


def recovery_text(key: bytes) -> tuple[str, str]:
    enc = base64.b32encode(key + recovery_checksum(key)).decode("ascii")
    assert len(enc) == 64 and "=" not in enc
    display = "-".join(enc[i : i + 4] for i in range(0, 64, 4))
    return display, RECOVERY_QR_PREFIX + enc


def keyset(label: str, key: bytes) -> None:
    print(f"## key {label} = {key.hex()}")
    for name, info in SUBKEY_INFOS:
        print(f"subkey {name:<13} info={info.decode()!r:<26} {hkdf_sha256(key, info, 32).hex()}")
    print(f"recovery wrap key  info={RECOVERY_INFO.decode()!r:<26} {hkdf_sha256(key, RECOVERY_INFO, 32).hex()}")
    print(f"recovery checksum  {recovery_checksum(key).hex()}")
    display, qr = recovery_text(key)
    print(f"recovery display   {display}")
    print(f"recovery qr        {qr}")
    print()


def main() -> None:
    okm, sas = sas_from_hash(bytes(32))
    print("## SAS")
    print(f"sas_from_hash([0; 32]) info={SAS_INFO.decode()!r} okm={okm.hex()} sas={sas}")
    print()
    keyset("[0x11; 32]", bytes([0x11] * 32))
    # The inputs of the older known-answer tests in vault-container/src/keys.rs.
    keyset("1..=32", bytes(range(1, 33)))
    keyset("0xa0..=0xbf", bytes(range(0xA0, 0xC0)))


if __name__ == "__main__":
    main()
