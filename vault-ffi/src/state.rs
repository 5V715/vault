// SPDX-License-Identifier: MIT
//! The opaque state blob Kotlin persists through [`HighWaterStore`]: the
//! container's anti-rollback high-water mark and the replication state
//! (per-replica marks). Not secret by itself (sequence numbers and manifest
//! hashes), but Kotlin seals it under K_bg so that it cannot be rolled back
//! or edited unnoticed.

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use vault_container::engine::HighWater;
use vault_container::replicate::ReplicaState;

use crate::error::FfiError;
use crate::types::HighWaterStore;

/// Version byte in front of the postcard encoding.
const STATE_VERSION: u8 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Persisted {
    pub high_water: Option<HighWater>,
    pub replicas: ReplicaState,
}

impl Persisted {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, FfiError> {
        let mut out = vec![STATE_VERSION];
        let body = postcard::to_allocvec(self)
            .map_err(|_| FfiError::internal("cannot encode the vault state"))?;
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, FfiError> {
        match bytes.split_first() {
            Some((&STATE_VERSION, body)) => {
                postcard::from_bytes(body).map_err(|_| FfiError::Storage {
                    detail: "the saved vault state is malformed".into(),
                })
            }
            _ => Err(FfiError::Unsupported {
                detail: "unknown vault state version".into(),
            }),
        }
    }

    /// Raise the mark to `hw` unless that would move it backwards.
    pub(crate) fn raise(&mut self, hw: HighWater) {
        match self.high_water {
            Some(cur) if cur.seq > hw.seq => {}
            _ => self.high_water = Some(hw),
        }
    }
}

/// The persisted state of one vault and the Kotlin store behind it. Updates
/// are serialized (one lock around modify-and-save).
pub(crate) struct StateCell {
    store: Arc<dyn HighWaterStore>,
    state: Mutex<Persisted>,
}

impl StateCell {
    /// Loads the state from the store (an empty store is a fresh state).
    pub(crate) fn load(store: Arc<dyn HighWaterStore>) -> Result<Arc<Self>, FfiError> {
        let state = match store.load()? {
            Some(b) => Persisted::decode(&b)?,
            None => Persisted::default(),
        };
        Ok(Self::with(store, state))
    }

    pub(crate) fn with(store: Arc<dyn HighWaterStore>, state: Persisted) -> Arc<Self> {
        Arc::new(Self {
            store,
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Persisted> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self) -> Persisted {
        self.lock().clone()
    }

    /// Apply `f` and save the result; on a failed save the change is kept in
    /// memory (it is retried with the next save) and the error returned.
    pub(crate) fn update(&self, f: impl FnOnce(&mut Persisted)) -> Result<(), FfiError> {
        let mut st = self.lock();
        f(&mut st);
        let bytes = st.encode()?;
        self.store.save(bytes)
    }

    /// Save the current state as is.
    pub(crate) fn save(&self) -> Result<(), FfiError> {
        self.update(|_| {})
    }
}
