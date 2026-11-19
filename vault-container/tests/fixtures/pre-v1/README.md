# Pre-v1 container fixture

A container written by the code from before the vault split (header without the
`vault` marker and format version), used by `pre_v1_container_is_rejected_with_clear_error`
in `vault-container/tests/engine.rs`. The bytes were produced by running that code;
do not edit them by hand.

How it was produced (2026-11-19):

1. `git -C signal-web worktree add --detach /tmp/sw-base 4324d53` (signal-web,
   branch `vault-design`, commit `4324d53`, the baseline copied in Task 1).
2. In `/tmp/sw-base`: `CARGO_TARGET_DIR=/tmp/sw-base-target cargo build -p container --bin vaultctl --locked`.
3. `VAULTCTL_PASSPHRASE=fixture-passphrase /tmp/sw-base-target/debug/vaultctl --dir /tmp/prev1 create`
4. `VAULTCTL_PASSPHRASE=fixture-passphrase /tmp/sw-base-target/debug/vaultctl --dir /tmp/prev1 put greeting hello`
   (prints `committed seq 1`).
5. Copied `header`, `manifest` and `seg/` from `/tmp/prev1` here (not `.vaultctl`,
   `.lock` or `probe/`); the worktree was removed afterwards.

Recovery key printed by step 3 (test data only, protects nothing else). It does
not parse with v1 code (the checksum label changed to `vault/v1/recovery-check`),
so the test opens the fixture with an unrelated key; the header is refused first:

    C3ZF-KVIJ-MDXO-BNUH-B2RA-NZVT-6CPN-YK37-ZJAV-IDDS-FB4Y-HX6L-Y3S5-RWTO-SXTL-4SES

SHA-256:

    7a0791ce8464c3a2368b24056cc103e83a3a1b608c242bf5c5cc79cddccfc224  header
    2ef12a73829fe4bf7512fb72b08fde95815c17ee16099258c746355daf602478  manifest
    8ea21fbf9e255e69392d98fa98244f1f8daafd78d6e131c5d4409a05af9a354e  seg/975474d6d01ab8f60b8b00edbb635b46
