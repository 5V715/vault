// SPDX-License-Identifier: MIT
//! The one error type crossing the FFI boundary.
//!
//! Every variant carries a human-readable `detail`. Details are built only
//! from error kinds, paths the app chose and static text: never from key
//! material, recovery keys, replica credentials or vault data (the container
//! and server errors this maps from never carry them either).

use vault_container::Error as CError;
use vault_server::ServerError;

/// Why a vault call failed. In Kotlin: `FfiException.<Variant>(detail)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    /// An argument is malformed (app id, origin, key length, PEM, target).
    #[error("invalid input: {detail}")]
    InvalidInput { detail: String },
    /// No vault (container header) at the given directory.
    #[error("not found: {detail}")]
    NotFound { detail: String },
    /// The target directory already holds something.
    #[error("already exists: {detail}")]
    AlreadyExists { detail: String },
    /// The call needs an unlocked vault.
    #[error("vault is locked")]
    Locked { detail: String },
    /// A session is active (compaction and replication wait), or another
    /// replication is running.
    #[error("busy: {detail}")]
    Busy { detail: String },
    /// The recovery key text failed its checksum: most likely a typo.
    #[error("recovery key checksum mismatch (typo?)")]
    RecoveryKeyTypo { detail: String },
    /// The key (CRK, recovery key or manifest key) does not open this vault,
    /// or the data failed authentication.
    #[error("wrong key or corrupted data")]
    WrongKey { detail: String },
    /// The stored manifest is older than, or forks from, the high-water mark.
    #[error("rollback or fork detected")]
    Rollback { detail: String },
    /// Replicas hold different valid manifests at the same seq.
    #[error("replicas diverged: {detail}")]
    Diverged { detail: String },
    /// The pairing text is not a usable `vault-pair:v1:` offer.
    #[error("not a vault pairing code: {detail}")]
    BadOffer { detail: String },
    /// The relay could not be reached (or refused the connection).
    #[error("relay: {detail}")]
    Relay { detail: String },
    /// TLS setup failed (e.g. the extra CA does not parse).
    #[error("TLS: {detail}")]
    Tls { detail: String },
    /// The relay mailbox was already taken: someone else may hold the pairing
    /// text. The approver was warned; nothing was approved.
    #[error("relay mailbox already taken")]
    MailboxTaken { detail: String },
    /// A time limit elapsed (relay connect, handshake).
    #[error("timed out")]
    Timeout { detail: String },
    /// Storage failure (I/O, malformed objects, conflicting writes).
    #[error("storage: {detail}")]
    Storage { detail: String },
    /// Unsupported container format or feature.
    #[error("unsupported: {detail}")]
    Unsupported { detail: String },
    /// A Kotlin callback (high-water store) failed.
    #[error("callback failed: {detail}")]
    Callback { detail: String },
    /// A bug or a panicked task.
    #[error("internal error: {detail}")]
    Internal { detail: String },
}

impl FfiError {
    pub(crate) fn invalid(detail: impl Into<String>) -> Self {
        Self::InvalidInput {
            detail: detail.into(),
        }
    }

    pub(crate) fn locked() -> Self {
        Self::Locked {
            detail: "unlock the vault (approve a session) first".into(),
        }
    }

    pub(crate) fn busy(detail: impl Into<String>) -> Self {
        Self::Busy {
            detail: detail.into(),
        }
    }

    pub(crate) fn storage(detail: impl Into<String>) -> Self {
        Self::Storage {
            detail: detail.into(),
        }
    }

    pub(crate) fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    pub(crate) fn io(e: std::io::Error) -> Self {
        // `io::Error` text names the OS error, never file contents.
        Self::storage(e.to_string())
    }
}

impl From<CError> for FfiError {
    fn from(e: CError) -> Self {
        let detail = e.to_string();
        match e {
            CError::RecoveryKeyTypo => Self::RecoveryKeyTypo { detail },
            CError::Crypto | CError::BadSecrets => Self::WrongKey { detail },
            CError::Rollback => Self::Rollback { detail },
            CError::DivergedReplicas(_) => Self::Diverged { detail },
            CError::InvalidBinding(_) => Self::InvalidInput { detail },
            CError::Conflict => Self::AlreadyExists { detail },
            CError::UnsupportedFormatVersion(_)
            | CError::PreV1Container
            | CError::Unsupported(_) => Self::Unsupported { detail },
            CError::IncompleteCreate
            | CError::SegmentIdCollision
            | CError::Io(_)
            | CError::Format(_) => Self::Storage { detail },
        }
    }
}

impl From<ServerError> for FfiError {
    fn from(e: ServerError) -> Self {
        match e {
            ServerError::Container(c) => c.into(),
            ServerError::Rng => Self::internal("no randomness for the handshake key"),
            ServerError::BadOffer => Self::BadOffer {
                detail: "the pairing code names a relay this vault will not use".into(),
            },
            ServerError::Relay(m) => Self::Relay { detail: m },
            ServerError::MailboxTaken => Self::MailboxTaken {
                detail: vault_server::MAILBOX_TAKEN_WARNING.into(),
            },
            ServerError::Tls(m) => Self::Tls { detail: m.into() },
            ServerError::Handshake => Self::Relay {
                detail: "pairing handshake failed".into(),
            },
            ServerError::Timeout => Self::Timeout {
                detail: "relay connection or handshake timed out".into(),
            },
        }
    }
}

impl From<vault_proto::ProtoError> for FfiError {
    fn from(e: vault_proto::ProtoError) -> Self {
        Self::BadOffer {
            detail: e.to_string(),
        }
    }
}

/// A Kotlin callback threw something that is not an `FfiException`.
impl From<uniffi::UnexpectedUniFFICallbackError> for FfiError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Callback { detail: e.reason }
    }
}
