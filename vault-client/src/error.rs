// SPDX-License-Identifier: MIT
//! Errors of the client side, and why a session ended.

use vault_proto::ErrorCode;

/// Error message of a denied `OpenSession`.
pub(crate) const DENIED: &str = "pairing denied on the vault";

/// A client-side failure. The `Display` text never contains key material or
/// record contents.
///
/// The texts are grouped by [`ErrorKind`] (`network error: ...`,
/// `protocol error: ...`, `storage error: ...`); [`ClientError::kind`] and
/// [`ClientError::message`] give the group and the text after its prefix, so an
/// embedding app can map them onto its own error type without changing them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The mailbox socket or the relay failed (connect, send, timeouts).
    #[error("network error: {0}")]
    Transport(String),
    /// A request could not be sent or answered because the channel is closed.
    #[error("network error: vault session closed: {0}")]
    Closed(CloseReason),
    /// Malformed input, an unexpected answer, or a call that is not allowed in
    /// the current state (for example a store request before approval).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The vault (the user on the phone) denied the pairing.
    #[error("protocol error: {DENIED}")]
    Denied,
    /// The vault answered a request with an error.
    #[error("storage error: vault: {code:?}: {msg}")]
    Vault { code: ErrorCode, msg: String },
    /// The [`crate::RemoteBackend`] is dead: its session ended or a commit
    /// failed; its cache and buffered writes are gone.
    #[error("storage error: vault session closed")]
    BackendClosed,
    /// [`crate::RemoteBackend::flush_commit`] failed (the inner error says
    /// why). The writes are not durable and the session has been ended.
    #[error(transparent)]
    CommitFailed(Box<ClientError>),
}

/// The group of a [`ClientError`] (the prefix of its text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Network,
    Protocol,
    Storage,
}

impl ClientError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Transport(_) | Self::Closed(_) => ErrorKind::Network,
            Self::Protocol(_) | Self::Denied => ErrorKind::Protocol,
            Self::Vault { .. } | Self::BackendClosed => ErrorKind::Storage,
            Self::CommitFailed(e) => e.kind(),
        }
    }

    /// The text after the `network error: ` / `protocol error: ` /
    /// `storage error: ` prefix.
    pub fn message(&self) -> String {
        match self {
            Self::Transport(m) | Self::Protocol(m) => m.clone(),
            Self::Closed(r) => format!("vault session closed: {r}"),
            Self::Denied => DENIED.to_string(),
            Self::Vault { code, msg } => format!("vault: {code:?}: {msg}"),
            Self::BackendClosed => "vault session closed".to_string(),
            Self::CommitFailed(e) => e.message(),
        }
    }
}

pub(crate) fn perr(msg: impl Into<String>) -> ClientError {
    ClientError::Protocol(msg.into())
}

/// Why a vault session ended. `Display` gives the reason text reported to the
/// UI (`denied`, `locked`, `terminated: <vault's reason>`, `disconnected`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The vault denied the pairing, or the user cancelled the code comparison.
    Denied,
    /// The host locked the session ([`crate::VaultSession::lock`] or dropping
    /// the handle), or the vault reported `Locked`.
    Locked,
    /// The vault ended the session (`SessionTerminated`) with this reason.
    Terminated(String),
    /// The mailbox socket closed.
    Disconnected,
    /// Locked before the mailbox opened or the handshake finished.
    Cancelled,
    /// A commit failed; the vault discards what it staged.
    CommitFailed,
    /// Sending on the mailbox failed.
    SendFailed,
    /// The vault sent something that does not decrypt or decode.
    ProtocolError,
    /// The channel was closed locally (after the goodbye).
    Closed,
    /// The relay could not be reached (the error text).
    RelayUnreachable(String),
    /// The handshake or `OpenSession` failed (the error text).
    PairingFailed(String),
    /// A keepalive ping failed (the error text).
    KeepaliveFailed(String),
    /// A keepalive ping was not answered in time.
    KeepaliveTimedOut,
    /// The host's activation hook failed with this text (it is the whole reason).
    Failed(String),
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => f.write_str("denied"),
            Self::Locked => f.write_str("locked"),
            Self::Terminated(r) => write!(f, "terminated: {r}"),
            Self::Disconnected => f.write_str("disconnected"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::CommitFailed => f.write_str("commit failed"),
            Self::SendFailed => f.write_str("send failed"),
            Self::ProtocolError => f.write_str("protocol error"),
            Self::Closed => f.write_str("closed"),
            Self::RelayUnreachable(e) => write!(f, "relay unreachable: {e}"),
            Self::PairingFailed(e) => write!(f, "pairing failed: {e}"),
            Self::KeepaliveFailed(e) => write!(f, "keepalive failed: {e}"),
            Self::KeepaliveTimedOut => f.write_str("keepalive timed out"),
            Self::Failed(r) => f.write_str(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_stable() {
        let cases: [(ClientError, &str); 7] = [
            (
                ClientError::Transport("pairing timed out".into()),
                "network error: pairing timed out",
            ),
            (
                ClientError::Closed(CloseReason::Disconnected),
                "network error: vault session closed: disconnected",
            ),
            (
                ClientError::Protocol("request too large".into()),
                "protocol error: request too large",
            ),
            (
                ClientError::Denied,
                "protocol error: pairing denied on the vault",
            ),
            (
                ClientError::Vault {
                    code: ErrorCode::Internal,
                    msg: "x".into(),
                },
                "storage error: vault: Internal: x",
            ),
            (
                ClientError::BackendClosed,
                "storage error: vault session closed",
            ),
            (
                ClientError::CommitFailed(Box::new(ClientError::BackendClosed)),
                "storage error: vault session closed",
            ),
        ];
        for (e, text) in cases {
            assert_eq!(e.to_string(), text);
            let prefix = match e.kind() {
                ErrorKind::Network => "network error: ",
                ErrorKind::Protocol => "protocol error: ",
                ErrorKind::Storage => "storage error: ",
            };
            assert_eq!(format!("{prefix}{}", e.message()), text);
        }
        assert_eq!(
            CloseReason::Terminated("replaced".into()).to_string(),
            "terminated: replaced"
        );
        assert_eq!(CloseReason::Failed("x: y".into()).to_string(), "x: y");
    }
}
