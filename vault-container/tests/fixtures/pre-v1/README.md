# Pre-v1 container fixture

A container directory written by the code from before the vault split (header without
the `vault` marker and format version), used by:

- `pre_v1_container_is_rejected_with_clear_error` (`vault-container/tests/engine.rs`),
- `vaultctl_pre_v1_dir_reports_unsupported_format` (`vault-container/tests/vaultctl.rs`),
- `vault_dev_pre_v1_dir_reports_unsupported_format` (`vault-server/tests/vault_dev.rs`).

The bytes were produced by running that code; do not edit them by hand. All files come
from one run (the secrets file belongs to this header and manifest).

How it was produced (2026-11-19; regenerated in Task 6 fix round 1 to add the secrets
file, replacing all files of the first run):

1. `git -C signal-web worktree add --detach /tmp/sw-base 4324d53` (signal-web,
   branch `vault-design`, commit `4324d53`, the baseline copied in Task 1).
2. In `/tmp/sw-base`: `CARGO_TARGET_DIR=/tmp/sw-base-target cargo build -p container --bin vaultctl --locked`.
3. `VAULTCTL_PASSPHRASE=fixture-passphrase /tmp/sw-base-target/debug/vaultctl --dir /tmp/prev1 create`
4. `VAULTCTL_PASSPHRASE=fixture-passphrase /tmp/sw-base-target/debug/vaultctl --dir /tmp/prev1 put greeting hello`
   (prints `committed seq 1`).
5. Copied `header`, `manifest` and `seg/` from `/tmp/prev1` here, and `/tmp/prev1/.vaultctl`
   as `vaultctl-secrets` (tests copy it back to `.vaultctl`); not `.lock` or the empty
   `probe/`. The worktree and the build directory were removed afterwards.

Passphrase of `vaultctl-secrets`: `fixture-passphrase`. Recovery key printed by step 3
(test data only, protects nothing else). It does not parse with v1 code (the checksum
label changed to `vault/v1/recovery-check`); the tools check the header before they use
any key:

    LDVL-GV7F-BKGR-V24N-MYYQ-CNT2-GDDI-2UCY-AAS6-OD7D-FI7O-2RO6-RJ5E-IEJS-MFBP-OKFJ

SHA-256:

    035e37b71913f4c24827b02bf388f9f3fffaeb30de030e68c2e612de2b82969c  header
    bbf1d1ef9c001fd8712a50b7dc82f79601b538f9279b98dd29ca9b2052a6f0a7  manifest
    b3bb4ef7efd44b2b3cae0dea69c536ad6041b7d946dd078d74c3d1fb2530884e  vaultctl-secrets
    ecb7724dea945bbb2f7f87563db7ef5ffa49c9916dcee5e53c923af1316a6eab  seg/86b84676654a7dfba1d6e030334a625a
