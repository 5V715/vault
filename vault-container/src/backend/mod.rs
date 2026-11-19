// SPDX-License-Identifier: MIT

//! Storage abstraction under the encrypted container.
//!
//! Objects are named `header`, `manifest` or `seg/<32 lowercase hex>`; `probe/<hex>`
//! is additionally accepted for conditional-write capability probes. The manifest is
//! updated with compare-and-swap ([`ContainerBackend::put_if_match`]) so that two
//! writers cannot clobber each other.

mod http;
mod local;
mod memory;
mod s3;
mod webdav;

pub use local::LocalBackend;
pub use memory::MemoryBackend;
pub use s3::{S3Backend, S3Config};
pub use webdav::{WebDavBackend, WebDavConfig};

use crate::{Error, Result};

/// Opaque version tag of a stored object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Etag(pub String);

/// Result of a conditional write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    /// The write happened; carries the new etag.
    Written(Etag),
    /// The precondition did not hold; nothing was written.
    Conflict,
}

/// A key-value object store with conditional writes.
#[async_trait::async_trait]
pub trait ContainerBackend: Send + Sync {
    /// Returns the object and its etag, or `None` if absent.
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>>;
    /// Creates the object only if it does not exist.
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome>;
    /// Replaces the object only if it exists with the given etag.
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome>;
    /// Deletes the object; deleting a missing object succeeds.
    async fn delete(&self, name: &str) -> Result<()>;
    /// Lists object names under `prefix` (only `seg/` is supported).
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

/// A shared backend is a backend (e.g. `Container<Arc<dyn ContainerBackend>>` next to
/// a `ReplicaSet` using the same primary).
#[async_trait::async_trait]
impl<T: ContainerBackend + ?Sized> ContainerBackend for std::sync::Arc<T> {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        (**self).get(name).await
    }
    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        (**self).put_if_absent(name, data).await
    }
    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        (**self).put_if_match(name, data, etag).await
    }
    async fn delete(&self, name: &str) -> Result<()> {
        (**self).delete(name).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        (**self).list(prefix).await
    }
}

const SEG_PREFIX: &str = "seg/";

fn is_lower_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Checks that `name` is a legal object name.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    let ok = match name {
        "header" | "manifest" => true,
        _ => {
            if let Some(id) = name.strip_prefix(SEG_PREFIX) {
                id.len() == 32 && is_lower_hex(id)
            } else if let Some(id) = name.strip_prefix("probe/") {
                id.len() <= 64 && is_lower_hex(id)
            } else {
                false
            }
        }
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Format(format!("invalid object name {name:?}")))
    }
}

/// Checks that `prefix` is a legal list prefix (only `seg/`).
pub(crate) fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix == SEG_PREFIX {
        Ok(())
    } else {
        Err(Error::Format(format!("unsupported list prefix {prefix:?}")))
    }
}
