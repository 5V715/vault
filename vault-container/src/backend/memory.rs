// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{ContainerBackend, Etag, PutOutcome, validate_name, validate_prefix};
use crate::Result;

type Store = HashMap<String, (Vec<u8>, Etag)>;

/// In-memory backend; clones share the same underlying store.
#[derive(Debug, Clone, Default)]
pub struct MemoryBackend {
    objects: Arc<Mutex<Store>>,
}

impl MemoryBackend {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

fn etag_of(data: &[u8]) -> Etag {
    Etag(data_encoding::HEXLOWER.encode(&Sha256::digest(data)))
}

#[async_trait::async_trait]
impl ContainerBackend for MemoryBackend {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        validate_name(name)?;
        Ok(self.objects.lock().await.get(name).cloned())
    }

    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        validate_name(name)?;
        let mut map = self.objects.lock().await;
        if map.contains_key(name) {
            return Ok(PutOutcome::Conflict);
        }
        let etag = etag_of(data);
        map.insert(name.to_owned(), (data.to_vec(), etag.clone()));
        Ok(PutOutcome::Written(etag))
    }

    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        validate_name(name)?;
        let mut map = self.objects.lock().await;
        match map.get(name) {
            Some((_, cur)) if cur == etag => {
                let new = etag_of(data);
                map.insert(name.to_owned(), (data.to_vec(), new.clone()));
                Ok(PutOutcome::Written(new))
            }
            _ => Ok(PutOutcome::Conflict),
        }
    }

    async fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.objects.lock().await.remove(name);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        validate_prefix(prefix)?;
        let mut names: Vec<String> = self
            .objects
            .lock()
            .await
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        names.sort();
        Ok(names)
    }
}
