// SPDX-License-Identifier: MIT
//! UniFFI facade over `vault-container` and `vault-server` for the Android
//! vault app (`android/`, Kotlin bindings generated in CI by this crate's
//! `uniffi-bindgen` binary).
//!
//! The surface is coarse and async: [`VaultFactory`] (create, open, restore,
//! re-wrap, offer parsing) and [`VaultHandle`] (stats, compact, lock,
//! serve_offer, replicate, manifest-key export). Kotlin supplies three
//! callback interfaces: [`ApproverCallback`] (approval screen plus
//! BiometricPrompt, returning the unwrapped CRK), [`HighWaterStore`] (the
//! opaque state blob, sealed under K_bg) and [`SessionListener`].
//!
//! Secrets cross the boundary only at these points:
//! - Rust → Kotlin, once each: [`VaultFactory::create`] returns the recovery
//!   key, the CRK and the manifest key; [`VaultFactory::rewrap_with_recovery_key`]
//!   returns a CRK copy; [`VaultHandle::export_manifest_key`] returns the
//!   manifest key while unlocked.
//! - Kotlin → Rust: the CRK in an approval ([`ApprovalFfi::Approve`]), the
//!   manifest key for [`VaultHandle::replicate`], the recovery key for restore
//!   and re-wrap, and replica credentials in [`ReplicaTarget`].
//!
//! Rust copies incoming secrets into `Zeroizing` buffers at once and wipes
//! them after use; outgoing ones are built from zeroizing copies. The byte
//! buffers UniFFI itself allocates to carry them across are freed without
//! wiping (a UniFFI limitation): Kotlin wipes its `ByteArray`s after use.
//! Nothing here logs secrets.
//!
//! Async calls run on a tokio runtime owned by the factory; cancelling the
//! Kotlin coroutine aborts the Rust task.

uniffi::setup_scaffolding!();

mod error;
mod factory;
mod handle;
mod replica;
mod runtime;
mod state;
mod types;

#[cfg(test)]
mod tests;

pub use error::FfiError;
pub use factory::VaultFactory;
pub use handle::VaultHandle;
pub use types::{
    ApprovalFfi, ApprovalRequestFfi, ApproverCallback, BindingInfo, CreatedVault, EndReasonFfi,
    FactoryConfig, HighWaterStore, NamedReplica, OfferInfo, ReplicaOutcomeFfi, ReplicaResult,
    ReplicaTarget, RestoredVault, SessionInfo, SessionListener, SessionStateFfi, VaultStats,
};
