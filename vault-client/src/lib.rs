// SPDX-License-Identifier: MIT
//! App/browser side of the vault protocol (`docs/protocol/vault.md`): the app
//! keeps no user data and reads and writes all state in the vault's container
//! through a Noise channel carried by a relay mailbox.
//!
//! - [`MailboxTransport`] / [`MailboxFactory`]: the mailbox WebSocket (binary
//!   frames), implemented by `wasm::WsTransport` in the browser (feature `wasm`)
//!   and by `native::TokioMailbox` (feature `native`).
//! - [`VaultSession`]: the pairing lifecycle. Before the vault reports
//!   `Approved` **and** the user confirmed the code locally
//!   ([`VaultSession::confirm_sas`]), only `OpenSession`, `Ping` and `Close`
//!   are sent.
//! - [`RemoteBackend`]: a key-value store over that channel with a RAM
//!   write-back cache. Reads hit the cache, then the vault; writes are
//!   buffered; [`RemoteBackend::flush_commit`] sends them in order and then
//!   `Commit`, resolving once the vault answered `Committed`. Nothing is ever
//!   written to local storage, and cached values are zeroized when dropped.
//!
//! The platform supplies a [`Clock`] and an [`Rng`] ([`SystemRng`] uses the
//! OS or the browser's `crypto.getRandomValues`).

mod backend;
mod error;
mod rpc;
mod session;
mod transport;

#[cfg(feature = "native")]
pub mod native;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use backend::RemoteBackend;
pub use error::{ClientError, CloseReason, ErrorKind};
pub use session::{
    ActivateHook, DeactivateHook, PING_INTERVAL_MS, SessionCallbacks, SessionOptions, SessionState,
    VaultSession,
};
pub use transport::{Clock, MailboxFactory, MailboxTransport, Rng, SystemRng, mailbox_url};

#[cfg(test)]
mod tests;
