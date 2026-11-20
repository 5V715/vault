// SPDX-License-Identifier: MIT
//! Records, enums and callback interfaces of the FFI surface.

use std::fmt;
use std::sync::Arc;

use crate::error::FfiError;
use crate::handle::VaultHandle;

/// Settings shared by every vault the factory opens.
#[derive(Clone, uniffi::Record)]
pub struct FactoryConfig {
    /// Extra trusted CA certificate(s), PEM, for the relay's TLS and the
    /// S3/WebDAV replicas, on top of the bundled webpki (Mozilla) roots.
    /// `None` (release builds): only the bundled roots. Debug builds may set
    /// it for the LAN showcase.
    #[uniffi(default = None)]
    pub extra_ca_pem: Option<String>,
    /// How long one approval prompt may stay open (and the wait for the
    /// browser's request before it); the spec's offer lifetime is 60 s.
    #[uniffi(default = 60)]
    pub approval_timeout_secs: u64,
    /// An approved session ends after this long without a browser request;
    /// `None` leaves idle detection to the app.
    #[uniffi(default = None)]
    pub idle_timeout_secs: Option<u64>,
}

impl fmt::Debug for FactoryConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FactoryConfig")
            .field("extra_ca_pem", &self.extra_ca_pem.is_some())
            .field("approval_timeout_secs", &self.approval_timeout_secs)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .finish()
    }
}

/// Where a replica lives. Credentials are secrets: Kotlin keeps them sealed
/// under the background Keystore key and passes them per call.
#[derive(Clone, uniffi::Enum)]
pub enum ReplicaTarget {
    /// A directory on this device (tests, or removable storage).
    Local { path: String },
    /// An S3-compatible bucket. `endpoint` is the base URL (https; http only
    /// for loopback). `path_style`: `true` for `endpoint/bucket/key` (MinIO,
    /// most self-hosted providers), `false` for `bucket.endpoint/key`.
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        access_key_id: String,
        secret_access_key: String,
        path_style: bool,
    },
    /// A WebDAV collection (e.g. Nextcloud), which must already exist.
    WebDav {
        url: String,
        user: String,
        password: String,
    },
}

impl fmt::Debug for ReplicaTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { path } => f.debug_struct("Local").field("path", path).finish(),
            Self::S3 {
                endpoint, bucket, ..
            } => f
                .debug_struct("S3")
                .field("endpoint", endpoint)
                .field("bucket", bucket)
                .finish_non_exhaustive(),
            Self::WebDav { url, .. } => f
                .debug_struct("WebDav")
                .field("url", url)
                .finish_non_exhaustive(),
        }
    }
}

/// A replica with the stable id its anti-rollback marks are kept under
/// (e.g. the id of the app's replica config entry).
#[derive(Clone, Debug, uniffi::Record)]
pub struct NamedReplica {
    pub id: String,
    pub target: ReplicaTarget,
}

/// The app binding of a vault: its identity on the phone.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BindingInfo {
    pub app_id: String,
    /// Canonical origin (lower-case host, default port dropped).
    pub origin: String,
}

/// What a scanned pairing code asks for (shown before anything is dialed).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct OfferInfo {
    pub app_id: String,
    pub origin: String,
    /// Host (and port) of the relay.
    pub relay_host: String,
}

/// Vault statistics for the settings screen. Container figures are known only
/// while unlocked.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct VaultStats {
    pub locked: bool,
    /// Manifest seq: of the open container, else the last persisted mark.
    pub seq: Option<u64>,
    pub segments: Option<u64>,
    pub stored_bytes: Option<u64>,
    pub live_bytes: Option<u64>,
    /// The approved session, if any.
    pub active_session: Option<u64>,
    /// Latest successful replication to any replica (ms since the epoch).
    pub last_replication_ms: Option<u64>,
}

/// A paired session (Noise handshake done; approval pending).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SessionInfo {
    pub session_id: u64,
    /// The 6-digit code the browser also shows.
    pub sas: String,
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum EndReasonFfi {
    Closed,
    Disconnected,
    Denied,
    ApprovalTimeout,
    NotApproved,
    Replaced,
    Locked,
    Timeout,
    IdleTimeout,
    ProtocolError,
    Busy,
    Internal,
    WrongApp,
    UnlockFailed,
}

impl From<vault_server::EndReason> for EndReasonFfi {
    fn from(r: vault_server::EndReason) -> Self {
        use vault_server::EndReason as E;
        match r {
            E::Closed => Self::Closed,
            E::Disconnected => Self::Disconnected,
            E::Denied => Self::Denied,
            E::ApprovalTimeout => Self::ApprovalTimeout,
            E::NotApproved => Self::NotApproved,
            E::Replaced => Self::Replaced,
            E::Locked => Self::Locked,
            E::Timeout => Self::Timeout,
            E::IdleTimeout => Self::IdleTimeout,
            E::ProtocolError => Self::ProtocolError,
            E::Busy => Self::Busy,
            E::Internal => Self::Internal,
            E::WrongApp => Self::WrongApp,
            E::UnlockFailed => Self::UnlockFailed,
        }
    }
}

/// Session state changes reported to a [`SessionListener`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SessionStateFfi {
    /// Approved: the browser is served (the vault is unlocked).
    Active,
    /// The session is over.
    Ended { reason: EndReasonFfi },
}

/// One replica's result of [`VaultHandle::replicate`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ReplicaOutcomeFfi {
    /// The replica holds the vault's manifest `seq`.
    Synced {
        seq: u64,
        uploaded: u64,
        deleted: u64,
    },
    /// The replica served an older manifest than last written there (rolled
    /// back); it was repaired to `seq`.
    Rollback {
        found_seq: Option<u64>,
        last_seen_seq: u64,
        seq: u64,
    },
    /// The replica holds a manifest that is newer than, forks from or does not
    /// belong to this vault. Nothing was written to it.
    Diverged {
        replica_seq: Option<u64>,
        primary_seq: u64,
    },
    /// A confirmed diverged replica was overwritten.
    Adopted { seq: u64, rollback: bool },
    /// The attempt failed (network, provider, concurrent change): retry later.
    Failed { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ReplicaResult {
    pub id: String,
    pub outcome: ReplicaOutcomeFfi,
}

/// What an approval prompt shows. All but `sas` and `mailbox_taken` comes from
/// the browser or the pairing code: display it as untrusted.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ApprovalRequestFfi {
    pub app_id: String,
    pub origin: String,
    pub relay_host: String,
    /// Browser-supplied; empty when `mailbox_taken`.
    pub device_name: String,
    /// The 6-digit code; empty when `mailbox_taken`.
    pub sas: String,
    /// The relay mailbox was already taken: show the warning prominently and
    /// answer `Deny` without unlocking anything. Any answer counts as `Deny`.
    pub mailbox_taken: bool,
}

/// The user's answer.
#[derive(uniffi::Enum)]
pub enum ApprovalFfi {
    Deny,
    /// Codes match and the BiometricPrompt unwrapped the CRK: its 32 bytes.
    /// Rust copies them into zeroizing memory at once and wipes the copy after
    /// the unlock; Kotlin wipes its `ByteArray` right after the call.
    Approve {
        crk: Vec<u8>,
    },
}

impl fmt::Debug for ApprovalFfi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Deny => "Deny",
            Self::Approve { .. } => "Approve(<redacted>)",
        })
    }
}

/// Asks the user to approve a browser session (Kotlin: the approval screen and
/// the BiometricPrompt that unwraps the CRK with K_auth).
///
/// Called for one prompt at a time. Rust cancels the call (the coroutine is
/// cancelled) after the approval timeout or when the session ends; treat that
/// as a denial. A request with `mailbox_taken` is a warning only.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait ApproverCallback: Send + Sync {
    async fn approve(&self, request: ApprovalRequestFfi) -> ApprovalFfi;
}

/// Persists the vault's opaque state blob (high-water mark and per-replica
/// anti-rollback marks). Kotlin seals it with the background Keystore key
/// (K_bg) in `noBackupFilesDir`, outside the container directory.
///
/// `save` is called after every commit and compaction, after replication and
/// at create/restore/re-wrap; keep it quick. It is never called concurrently
/// for one vault.
#[uniffi::export(with_foreign)]
pub trait HighWaterStore: Send + Sync {
    /// The last saved blob, or `None` if there is none yet.
    fn load(&self) -> Result<Option<Vec<u8>>, FfiError>;
    fn save(&self, state: Vec<u8>) -> Result<(), FfiError>;
}

/// Session events (Kotlin: the foreground service and its notification).
/// Called from Rust worker threads; return quickly.
#[uniffi::export(with_foreign)]
pub trait SessionListener: Send + Sync {
    fn on_state(&self, session_id: u64, state: SessionStateFfi);
}

/// Result of [`crate::VaultFactory::create`].
#[derive(uniffi::Record)]
pub struct CreatedVault {
    /// The new vault, unlocked.
    pub handle: Arc<VaultHandle>,
    /// Secret, exported once: the recovery key (16 groups) to show once.
    pub recovery_key: String,
    /// Secret, exported once: the CRK, to wrap under K_auth at once.
    pub crk: Vec<u8>,
    /// Secret, exported once: the manifest key, to seal under K_bg at once.
    pub manifest_key: Vec<u8>,
}

impl fmt::Debug for CreatedVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreatedVault(<redacted>)")
    }
}

/// Result of [`crate::VaultFactory::restore`].
#[derive(Debug, uniffi::Record)]
pub struct RestoredVault {
    /// The restored vault, unlocked (opened with the recovery key).
    pub handle: Arc<VaultHandle>,
    /// The manifest seq restored.
    pub seq: u64,
}
