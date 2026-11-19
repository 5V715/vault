// SPDX-License-Identifier: MIT
//! Wire protocol between the browser client of an app and a vault.
//!
//! Pure data and framing: no I/O, no clock, no async runtime, so it compiles
//! for `wasm32-unknown-unknown`. The full protocol is in `docs/protocol/vault.md`.

pub mod binding;
pub mod frame;
pub mod msg;
pub mod noise;

pub use frame::{Chunk, Chunker, Reassembler};
pub use msg::{ErrorCode, Notice, PairOffer, Request, RequestBody, Response, ResponseBody};

/// Protocol version: the first wire field of [`PairOffer`], and the `v1` of the QR
/// prefix and the labels. An offer with any other version is refused.
pub const PROTOCOL_VERSION: u8 = 1;
/// Noise protocol name.
pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
/// Noise prologue label; the full prologue is label ‖ version ‖ u16be(len) app_id ‖
/// u16be(len) origin ‖ mailbox_id ‖ browser_eph_pub.
pub const PROLOGUE_LABEL: &[u8] = b"vault/v1";
/// HKDF info string for the short authentication string.
pub const SAS_INFO: &[u8] = b"vault/v1/sas";

/// Errors from encoding, decoding and framing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("QR payload has the wrong prefix")]
    BadQrPrefix,
    #[error("QR payload is not valid base64url")]
    BadQrEncoding,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    #[error("invalid relay url: {0}")]
    BadRelayUrl(&'static str),
    #[error("invalid app id (must match ^[a-z0-9][a-z0-9.-]{{0,63}}$)")]
    BadAppId,
    #[error("invalid origin: {0}")]
    BadOrigin(&'static str),
    #[error("malformed message")]
    Malformed,
    #[error("message or field too large")]
    TooLarge,
    #[error("invalid field value")]
    BadRequest,
    #[error("chunks interleaved: another message is in flight")]
    Interleaved,
    #[error("chunk out of sequence")]
    OutOfSequence,
    #[error("too many chunks")]
    TooManyChunks,
}
