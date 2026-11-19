// SPDX-License-Identifier: MIT

//! A key-value store over an approved vault session, with a RAM write-back cache.

use crate::error::{ClientError, CloseReason};
use crate::rpc::Rpc;
use futures::lock::Mutex;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use vault_proto::{RequestBody, ResponseBody};
use zeroize::Zeroizing;

/// Record bytes held in RAM: wiped when dropped (`discard`, `clear`, session end).
type Secret = Zeroizing<Vec<u8>>;

fn dead() -> ClientError {
    ClientError::BackendClosed
}

fn unexpected(what: &str) -> ClientError {
    ClientError::Protocol(format!("unexpected vault response to {what}"))
}

#[derive(Default)]
struct State {
    /// Everything known about the vault's state plus our unsent writes:
    /// `Some(v)` = value, `None` = known absent. Overlay entries win over
    /// anything the vault would say.
    cache: BTreeMap<Vec<u8>, Option<Secret>>,
    /// Keys whose cache entry has not been sent to the vault yet.
    dirty: BTreeSet<Vec<u8>>,
    /// A `Clear` is buffered: every key not in the cache is absent.
    clear_pending: bool,
    /// Prefixes whose full contents are in the cache.
    scanned: Vec<Vec<u8>>,
    /// The vault holds staged writes (from `put_if_absent`) not yet committed.
    staged_remote: bool,
    dead: bool,
}

impl State {
    fn complete_for(&self, prefix: &[u8]) -> bool {
        self.clear_pending || self.scanned.iter().any(|s| prefix.starts_with(s))
    }
}

struct Inner {
    rpc: Rc<Rpc>,
    state: RefCell<State>,
    /// One flush at a time.
    flush: Mutex<()>,
}

/// Key-value store over an approved vault session, with a RAM write-back cache.
/// Reads hit the cache, then the vault; writes are buffered until
/// [`Self::flush_commit`]. Nothing is ever written to local storage.
/// Cloning yields another handle to the same session and cache.
///
/// An embedding app implements its own storage trait for a thin wrapper
/// around this type (the method semantics are those of a plain key-value
/// store whose `flush` makes every earlier write durable).
#[derive(Clone)]
pub struct RemoteBackend {
    inner: Rc<Inner>,
}

impl RemoteBackend {
    pub(crate) fn new(rpc: Rc<Rpc>) -> Self {
        Self {
            inner: Rc::new(Inner {
                rpc,
                state: RefCell::default(),
                flush: Mutex::new(()),
            }),
        }
    }

    fn live(&self) -> Result<(), ClientError> {
        if self.inner.state.borrow().dead {
            Err(dead())
        } else {
            Ok(())
        }
    }

    /// Drop the cache and every buffered write; all later calls fail. Called
    /// when the session ends (the vault discards its staged writes too).
    pub(crate) fn discard(&self) {
        let mut s = self.inner.state.borrow_mut();
        *s = State {
            dead: true,
            ..State::default()
        };
    }

    /// (cached entries, buffered writes): for tests and diagnostics (counts
    /// only, no contents).
    pub fn sizes(&self) -> (usize, usize) {
        let s = self.inner.state.borrow();
        (s.cache.len(), s.dirty.len())
    }

    /// Whether the backend still works (its session has not ended).
    pub fn is_live(&self) -> bool {
        !self.inner.state.borrow().dead
    }

    /// The value at `k`, or `None` if absent.
    pub async fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>, ClientError> {
        self.live()?;
        {
            let s = self.inner.state.borrow();
            if let Some(v) = s.cache.get(k) {
                return Ok(v.as_ref().map(|v| v.to_vec()));
            }
            if s.complete_for(k) {
                return Ok(None);
            }
        }
        let r = self
            .inner
            .rpc
            .call(RequestBody::Get { key: k.to_vec() })
            .await?;
        let ResponseBody::Value(v) = r else {
            return Err(unexpected("Get"));
        };
        let mut s = self.inner.state.borrow_mut();
        if s.dead {
            return Err(dead());
        }
        // A concurrent write wins over what the vault knew.
        Ok(s.cache
            .entry(k.to_vec())
            .or_insert(v.map(Zeroizing::new))
            .as_ref()
            .map(|v| v.to_vec()))
    }

    /// Buffer a write of `v` at `k` (sent at [`Self::flush_commit`]).
    pub async fn put(&self, k: &[u8], v: &[u8]) -> Result<(), ClientError> {
        self.live()?;
        let mut s = self.inner.state.borrow_mut();
        s.cache.insert(k.to_vec(), Some(Zeroizing::new(v.to_vec())));
        s.dirty.insert(k.to_vec());
        Ok(())
    }

    /// Store `v` at `k` only if `k` is absent; returns whether it was stored.
    /// Answered locally when the cache knows the key, otherwise by the vault
    /// (which stages the write until the next commit).
    pub async fn put_if_absent(&self, k: &[u8], v: &[u8]) -> Result<bool, ClientError> {
        self.live()?;
        {
            let mut s = self.inner.state.borrow_mut();
            match s.cache.get(k) {
                Some(Some(_)) => return Ok(false),
                Some(None) => {
                    s.cache.insert(k.to_vec(), Some(Zeroizing::new(v.to_vec())));
                    s.dirty.insert(k.to_vec());
                    return Ok(true);
                }
                None if s.complete_for(k) => {
                    s.cache.insert(k.to_vec(), Some(Zeroizing::new(v.to_vec())));
                    s.dirty.insert(k.to_vec());
                    return Ok(true);
                }
                None => {}
            }
        }
        // Unknown here: the vault decides (and stages the write when it wins).
        let r = self
            .inner
            .rpc
            .call(RequestBody::PutIfAbsent {
                key: k.to_vec(),
                value: v.to_vec(),
            })
            .await?;
        let ResponseBody::Bool(won) = r else {
            return Err(unexpected("PutIfAbsent"));
        };
        let mut s = self.inner.state.borrow_mut();
        if s.dead {
            return Err(dead());
        }
        if won {
            s.staged_remote = true;
            // Already staged at the vault: cache it clean, unless a concurrent
            // write got in first (that one is sent at flush and wins).
            s.cache
                .entry(k.to_vec())
                .or_insert_with(|| Some(Zeroizing::new(v.to_vec())));
        }
        Ok(won)
    }

    /// Buffer a delete of `k`.
    pub async fn delete(&self, k: &[u8]) -> Result<(), ClientError> {
        self.live()?;
        let mut s = self.inner.state.borrow_mut();
        s.cache.insert(k.to_vec(), None);
        s.dirty.insert(k.to_vec());
        Ok(())
    }

    /// Every entry whose key starts with `p`, in key order, including the
    /// buffered writes.
    pub async fn scan_prefix(&self, p: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ClientError> {
        self.live()?;
        let covered = self.inner.state.borrow().complete_for(p);
        if !covered {
            let r = self
                .inner
                .rpc
                .call(RequestBody::ScanPrefix { prefix: p.to_vec() })
                .await?;
            let ResponseBody::Entries(entries) = r else {
                return Err(unexpected("ScanPrefix"));
            };
            let mut s = self.inner.state.borrow_mut();
            if s.dead {
                return Err(dead());
            }
            for (k, v) in entries {
                s.cache.entry(k).or_insert(Some(Zeroizing::new(v)));
            }
            if !s.complete_for(p) {
                s.scanned.retain(|q| !q.starts_with(p));
                s.scanned.push(p.to_vec());
            }
        }
        let s = self.inner.state.borrow();
        if s.dead {
            return Err(dead());
        }
        Ok(s.cache
            .range(p.to_vec()..)
            .take_while(|(k, _)| k.starts_with(p))
            .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.to_vec())))
            .collect())
    }

    /// Buffer a `Clear` of the whole store.
    pub async fn clear(&self) -> Result<(), ClientError> {
        self.live()?;
        let mut s = self.inner.state.borrow_mut();
        s.cache.clear();
        s.dirty.clear();
        s.scanned.clear();
        s.clear_pending = true;
        Ok(())
    }

    /// Send the buffered writes in order (`Clear`, then each key's latest
    /// `Put`/`Delete`), then `Commit`; resolves after `Committed`. Any failure
    /// kills the backend: the vault discards what it staged, and the caller
    /// must not treat the writes as durable ([`ClientError::CommitFailed`]).
    pub async fn flush_commit(&self) -> Result<(), ClientError> {
        let _one = self.inner.flush.lock().await;
        self.live()?;
        let (batch, clear, staged) = {
            let mut s = self.inner.state.borrow_mut();
            let keys = std::mem::take(&mut s.dirty);
            let batch: Vec<RequestBody> = keys
                .into_iter()
                .map(
                    |k| match s.cache.get(&k).and_then(|v| v.as_ref().map(|v| v.to_vec())) {
                        Some(value) => RequestBody::Put { key: k, value },
                        None => RequestBody::Delete { key: k },
                    },
                )
                .collect();
            let clear = std::mem::take(&mut s.clear_pending);
            // `scanned` stays valid: after a Clear the cache holds the whole state,
            // which `complete_for` can no longer assume once the Clear is sent.
            if clear {
                s.scanned.push(Vec::new());
            }
            (batch, clear, std::mem::take(&mut s.staged_remote))
        };
        if batch.is_empty() && !clear && !staged {
            return Ok(());
        }
        let result = async {
            let mut bodies = Vec::with_capacity(batch.len() + 1);
            if clear {
                bodies.push(RequestBody::Clear);
            }
            bodies.extend(batch);
            for r in self.inner.rpc.call_many(bodies).await? {
                if !matches!(r, ResponseBody::Ok) {
                    return Err(unexpected("write"));
                }
            }
            // Only after every write was accepted: a rejected write must not
            // leave a partial commit behind.
            match self.inner.rpc.call(RequestBody::Commit).await? {
                ResponseBody::Committed { .. } => Ok(()),
                _ => Err(unexpected("Commit")),
            }
        }
        .await;
        if let Err(e) = result {
            // The writes are not durable: end the session (the vault drops what
            // it staged) so nothing can be acked or reported as saved.
            self.inner.rpc.fail(CloseReason::CommitFailed);
            self.discard();
            return Err(ClientError::CommitFailed(Box::new(e)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod zeroize_tests {
    use super::*;

    /// Compile-time guard: cached record bytes are `Zeroizing`, so dropping the
    /// state (`discard`, `clear`, session end) wipes them.
    #[test]
    fn cache_values_are_zeroizing() {
        fn _wants(_: &BTreeMap<Vec<u8>, Option<Zeroizing<Vec<u8>>>>) {}
        let s = State::default();
        _wants(&s.cache);
    }
}
