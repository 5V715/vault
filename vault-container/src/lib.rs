// SPDX-License-Identifier: MIT

//! Encrypted, replicated container for all persistent user data.

pub mod backend;
pub mod engine;
pub mod keys;
pub mod manifest;
pub mod replicate;
#[cfg(feature = "cli")]
pub mod secrets;
pub mod segment;

pub use keys::{Crk, RecoveryKey, Subkeys, unwrap_crk, wrap_crk};

/// Errors returned by the container crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The recovery key text failed its checksum (most likely a typo).
    #[error("recovery key checksum mismatch (typo?)")]
    RecoveryKeyTypo,
    /// Authentication failed or the ciphertext/key material is invalid.
    #[error("cryptographic operation failed")]
    Crypto,
    /// A concurrent modification conflicted with this operation.
    #[error("conflict")]
    Conflict,
    /// The stored manifest is older than, or forks from, the caller's high-water mark.
    #[error("manifest rollback or fork detected")]
    Rollback,
    /// Replicas hold different, individually valid manifests at the same `seq`; the
    /// user must choose one (e.g. restore again from only that replica). Carries the
    /// ids of the replicas involved.
    #[error("replicas diverged: {0:?}")]
    DivergedReplicas(Vec<String>),
    /// A header exists but no manifest: `create` was interrupted. The caller may wipe
    /// the backend and create again only if it never held a high-water mark for it
    /// (otherwise the manifest was deleted and opening reports [`Error::Rollback`]).
    #[error("incomplete container creation (header without manifest)")]
    IncompleteCreate,
    /// A fresh random segment id collided twice: broken RNG or a hostile backend.
    #[error("segment id collided twice")]
    SegmentIdCollision,
    /// The data uses an unsupported format version or feature.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Underlying I/O failure.
    #[error("i/o error: {0}")]
    Io(String),
    /// Malformed data.
    #[error("format error: {0}")]
    Format(String),
    /// A passphrase-sealed secrets file did not open (wrong passphrase, corruption or tampering).
    #[error("wrong passphrase or corrupted secrets file")]
    BadSecrets,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Compile-time check that the futures the vault spawns on a multi-threaded runtime
/// are `Send`, for any backend (never called).
#[allow(dead_code, clippy::too_many_arguments)]
fn _futures_are_send<B: backend::ContainerBackend + 'static>(
    b1: B,
    b2: B,
    b3: B,
    c: &mut engine::Container<B>,
    crk: Crk,
    rk: &RecoveryKey,
    set: &mut replicate::ReplicaSet,
    replicas: &[(
        replicate::ReplicaId,
        std::sync::Arc<dyn backend::ContainerBackend>,
    )],
) {
    fn send<T: Send>(_: T) {}
    send(engine::Container::create(b1));
    send(engine::Container::open_with_crk(b2, crk, None));
    send(engine::Container::open_with_recovery(b3, rk, None));
    send(c.commit());
    send(c.compact());
    send(c.gc_orphans());
    send(c.reload());
    send(set.replicate_once(&[0u8; 32], |_: &replicate::ReplicaState| Ok(())));
    let target = backend::MemoryBackend::new();
    send(replicate::restore(
        replicas,
        rk,
        &replicate::ReplicaId(String::new()),
        &target,
        None,
    ));
}
