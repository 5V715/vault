# Known-answer values

`kat.py` computes the known-answer test (KAT) values for the vault v1 labels with the
Python `cryptography` package, independently of the Rust code. It follows the written
definitions, not the Rust: the SAS in `docs/protocol/vault.md` and the container key
derivation, recovery checksum and recovery key text in `docs/vault/README.md`
("Format v1").

```sh
pip install cryptography   # tested with 46
python3 -I tools/kat/kat.py
```

It prints the SAS for `handshake_hash = [0; 32]`, the HKDF subkeys and recovery wrap key
for the key `[0x11; 32]`, its recovery checksum, display text and QR payload, and the
same values for the inputs of the older `known_answers` test (`1..=32`, `0xa0..=0xbf`).
The Rust tests pin these values (`sas_kat_v1`, `subkey_kats_v1`,
`recovery_checksum_kat_v1`, `known_answers`); the table in `docs/protocol/vault.md`
("Known answers") lists them. If a definition changes, change the doc first, then this
script, then paste its output into the tests.
