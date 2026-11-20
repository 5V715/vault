// SPDX-License-Identifier: MIT

//! Container engine: an encrypted key/value store on a [`ContainerBackend`].
//!
//! State is the replay of the segments listed in the current sealed manifest, in
//! order (later records win, `Delete` removes). Writes collect in an in-memory
//! pending batch; [`Container::commit`] writes the batch as new immutable segments
//! (`put_if_absent`) and only then switches the manifest with compare-and-swap
//! (`put_if_match`). A crash at any point therefore leaves either the old or the
//! new state, never a mix; segments written before a failed switch are orphans that
//! [`Container::gc_orphans`] removes.
//!
//! Anti-rollback: the caller persists [`Container::high_water`] and passes it back
//! when opening; a manifest with a lower `seq`, or the same `seq` with a different
//! hash, is refused with [`Error::Rollback`].
//!
//! Objects: `header` = `"vault" || format_version(1) || postcard(HeaderBody)` holding the
//! recovery-wrapped CRK and the [`Binding`] (app id and origin, the only plaintext
//! identifiers); written once with `put_if_absent`. Every manifest carries a copy
//! of both under its authentication tag, and opening refuses a header that does not
//! match (so a tampered header fails to open). A header without the `vault` marker
//! whose first byte is 1 was written before the split: [`Error::PreV1Container`]. A header without a manifest is
//! an interrupted `create` ([`Error::IncompleteCreate`]) unless the caller holds a
//! high-water mark, in which case the manifest was removed ([`Error::Rollback`]).
//!
//! The engine is single-writer per instance (`&mut self`); writers on other devices
//! are detected by the manifest compare-and-swap and get [`Error::Conflict`].

use std::collections::{BTreeMap, HashMap, HashSet};

use hmac::{Hmac, Mac};
use rand::{CryptoRng, TryRngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use crate::backend::{ContainerBackend, Etag, PutOutcome};
use crate::manifest::{self, MAX_WRAPPED_CRK, Manifest};
use crate::segment::{Record, SegmentReader, SegmentWriter, random_segment_id};
use crate::{
    Binding, CONTAINER_FORMAT_VERSION, Crk, Error, RecoveryKey, Result, Subkeys, unwrap_crk,
    wrap_crk,
};

const HEADER: &str = "header";
const MANIFEST: &str = "manifest";
const SEG_PREFIX: &str = "seg/";
/// Fixed marker that starts every v1 header (containers from before the split have none).
const HEADER_MAGIC: &[u8] = b"vault";
/// First byte of a header from before the split (`version(1) || postcard(body)`).
const PRE_V1_HEADER_VERSION: u8 = 1;
const MAX_HEADER: usize = 4096;
/// Record stream budget per segment so that each segment object stays <= 32 MiB.
const SEGMENT_TARGET: usize = 32 * 1024 * 1024 - 64 * 1024;
/// Upper bound of a record's framing beyond key and value bytes: length prefix,
/// enum tag, key MAC and two varint lengths.
const RECORD_OVERHEAD: usize = 4 + 1 + 32 + 2 * 10;
/// How often `open`/`reload` restart when a listed segment vanished because the
/// manifest moved on (e.g. a concurrent compaction) while reading.
const LOAD_ATTEMPTS: usize = 3;

/// Anti-rollback high-water mark: the latest manifest `seq` accepted and its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighWater {
    pub seq: u64,
    pub hash: [u8; 32],
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HeaderBody {
    pub(crate) recovery_wrapped_crk: Vec<u8>,
    pub(crate) binding: Binding,
}

impl HeaderBody {
    /// The manifest carries the same wrapped CRK and binding (the header belongs to it).
    pub(crate) fn matches(&self, m: &Manifest) -> bool {
        self.recovery_wrapped_crk == m.recovery_wrapped_crk && self.binding == m.binding
    }
}

fn encode_header(h: &HeaderBody) -> Result<Vec<u8>> {
    let mut out = HEADER_MAGIC.to_vec();
    out.push(CONTAINER_FORMAT_VERSION);
    out.extend(postcard::to_allocvec(h).map_err(|e| Error::Format(e.to_string()))?);
    Ok(out)
}

pub(crate) fn decode_header(bytes: &[u8]) -> Result<HeaderBody> {
    if bytes.len() > MAX_HEADER {
        return Err(Error::Format("header too large".into()));
    }
    if bytes.is_empty() {
        return Err(Error::Format("empty header".into()));
    }
    let Some(rest) = bytes.strip_prefix(HEADER_MAGIC) else {
        if bytes[0] == PRE_V1_HEADER_VERSION {
            return Err(Error::PreV1Container);
        }
        return Err(Error::Format("not a vault container header".into()));
    };
    // The format version comes first and is checked before the body is decoded.
    let body = match rest.split_first() {
        None => return Err(Error::Format("header without format version".into())),
        Some((&CONTAINER_FORMAT_VERSION, body)) => body,
        Some((&v, _)) => return Err(Error::UnsupportedFormatVersion(v)),
    };
    let (h, rest) = postcard::take_from_bytes::<HeaderBody>(body)
        .map_err(|_| Error::Format("malformed header".into()))?;
    if !rest.is_empty() {
        return Err(Error::Format("trailing bytes in header".into()));
    }
    if h.recovery_wrapped_crk.len() > MAX_WRAPPED_CRK {
        return Err(Error::Format("wrapped CRK too large".into()));
    }
    h.binding.check()?;
    Ok(h)
}

pub(crate) fn seg_name(id: &[u8; 16]) -> String {
    format!("{SEG_PREFIX}{}", data_encoding::HEXLOWER.encode(id))
}

fn key_mac(mac_key: &[u8; 32], key: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(mac_key).expect("HMAC accepts any key size");
    m.update(key);
    m.finalize().into_bytes().into()
}

fn record_size(key: &[u8], value: &[u8]) -> usize {
    RECORD_OVERHEAD + key.len() + value.len()
}

fn check_size(key: &[u8], value: &[u8]) -> Result<()> {
    if record_size(key, value) > SEGMENT_TARGET {
        Err(Error::Unsupported("record exceeds maximum size".into()))
    } else {
        Ok(())
    }
}

/// Refuses a manifest older than the mark, a different manifest at the mark's
/// `seq`, and a direct successor that does not chain to the mark's hash. Manifests
/// further ahead cannot be link-checked without the intermediate ones.
fn check_high_water(m: &Manifest, hash: &[u8; 32], hw: Option<HighWater>) -> Result<()> {
    let Some(hw) = hw else { return Ok(()) };
    let fork = if m.seq < hw.seq {
        true
    } else if m.seq == hw.seq {
        !bool::from(hash.ct_eq(&hw.hash))
    } else if m.seq == hw.seq + 1 {
        !bool::from(m.prev.ct_eq(&hw.hash))
    } else {
        false
    };
    if fork { Err(Error::Rollback) } else { Ok(()) }
}

/// Committed state: logical key -> value, plus key MAC -> logical key for deletes.
#[derive(Default)]
struct Index {
    by_key: BTreeMap<Vec<u8>, Vec<u8>>,
    by_mac: HashMap<[u8; 32], Vec<u8>>,
}

impl Index {
    fn insert(&mut self, mac: [u8; 32], key: Vec<u8>, value: Vec<u8>) {
        self.by_mac.entry(mac).or_insert_with(|| key.clone());
        if let Some(mut old) = self.by_key.insert(key, value) {
            old.zeroize();
        }
    }

    fn remove_mac(&mut self, mac: &[u8; 32]) {
        if let Some(mut key) = self.by_mac.remove(mac) {
            if let Some((mut k, mut v)) = self.by_key.remove_entry(&key) {
                k.zeroize();
                v.zeroize();
            }
            key.zeroize();
        }
    }

    /// Applies a record read from a segment, checking its key MAC.
    fn replay(&mut self, mac_key: &[u8; 32], record: Record) -> Result<()> {
        match record {
            Record::Put {
                key_mac: mac,
                mut key,
                mut value,
            } => {
                if !bool::from(key_mac(mac_key, &key).ct_eq(&mac)) {
                    return Err(Error::Crypto);
                }
                // Moved (not copied) into the index, which wipes them itself.
                self.insert(mac, std::mem::take(&mut *key), std::mem::take(&mut *value));
            }
            Record::Delete { key_mac: mac } => self.remove_mac(&mac),
        }
        Ok(())
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        for (mut k, mut v) in std::mem::take(&mut self.by_key) {
            k.zeroize();
            v.zeroize();
        }
        for (_, mut k) in std::mem::take(&mut self.by_mac) {
            k.zeroize();
        }
    }
}

/// Result of reading the current manifest and replaying its segments.
struct Loaded {
    etag: Etag,
    hash: [u8; 32],
    seq: u64,
    segments: Vec<[u8; 16]>,
    index: Index,
    stored_bytes: u64,
}

/// Size accounting of the committed state, for a compaction policy.
///
/// Byte counts are record sizes (key + value + a fixed per-record overhead),
/// not object sizes: segments are additionally padded to power-of-two buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Segments listed in the current manifest.
    pub segments: usize,
    /// Bytes of all records in those segments (live, overwritten and deletes).
    pub stored_bytes: u64,
    /// Bytes the live committed entries would take as records.
    pub live_bytes: u64,
}

impl Stats {
    /// Share of `stored_bytes` that is overwritten or deleted (0.0 when empty).
    pub fn dead_ratio(&self) -> f64 {
        if self.stored_bytes == 0 {
            return 0.0;
        }
        let dead = self.stored_bytes.saturating_sub(self.live_bytes);
        dead as f64 / self.stored_bytes as f64
    }
}

fn record_bytes(r: &Record) -> usize {
    match r {
        Record::Put { key, value, .. } => record_size(key, value),
        Record::Delete { .. } => RECORD_OVERHEAD,
    }
}

/// An open container. Not shareable across tasks: callers serialize access.
pub struct Container<B> {
    backend: B,
    crk: Crk,
    subkeys: Subkeys,
    /// The header's contents (wrapped CRK and binding), copied into every manifest.
    header: HeaderBody,
    etag: Etag,
    hash: [u8; 32],
    seq: u64,
    segments: Vec<[u8; 16]>,
    index: Index,
    /// Uncommitted changes: `Some(value)` = put, `None` = delete.
    pending: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    segment_target: usize,
    /// Record bytes in the listed segments (see [`Stats`]).
    stored_bytes: u64,
}

impl<B> Drop for Container<B> {
    fn drop(&mut self) {
        wipe_pending(&mut self.pending);
    }
}

fn wipe_pending(p: &mut BTreeMap<Vec<u8>, Option<Vec<u8>>>) {
    for (mut k, v) in std::mem::take(p) {
        k.zeroize();
        if let Some(mut v) = v {
            v.zeroize();
        }
    }
}

impl<B: ContainerBackend> Container<B> {
    /// Creates a new container on an empty backend, bound to `app_id` and `origin`
    /// (see [`Binding::new`]; the origin is stored in canonical form): writes the
    /// header and the genesis manifest (seq 0). Returns the recovery key to show to
    /// the user once.
    ///
    /// Fails with [`Error::InvalidBinding`] for a bad app id or origin and with
    /// [`Error::Conflict`] if the backend already holds any object.
    ///
    /// Key material comes from the operating system RNG; callers cannot inject one:
    ///
    /// ```
    /// # async fn f() {
    /// use vault_container::{backend::MemoryBackend, engine::Container};
    /// let _ = Container::create(MemoryBackend::new(), "notes", "https://notes.example").await;
    /// # }
    /// ```
    ///
    /// ```compile_fail
    /// # async fn f() {
    /// use vault_container::{backend::MemoryBackend, engine::Container};
    /// let _ = Container::create(MemoryBackend::new(), "notes", "https://notes.example", &mut rand::rng()).await;
    /// # }
    /// ```
    pub async fn create(backend: B, app_id: &str, origin: &str) -> Result<(Self, RecoveryKey)> {
        let binding = Binding::new(app_id, origin)?;
        Self::create_with(backend, binding, &mut OsRng.unwrap_err()).await
    }

    /// As [`Container::create`] with a caller-supplied RNG; crate-private so production
    /// cannot inject a weak RNG.
    pub(crate) async fn create_with(
        backend: B,
        binding: Binding,
        rng: &mut (impl CryptoRng + Send),
    ) -> Result<(Self, RecoveryKey)> {
        if backend.get(HEADER).await?.is_some()
            || backend.get(MANIFEST).await?.is_some()
            || !backend.list(SEG_PREFIX).await?.is_empty()
        {
            return Err(Error::Conflict);
        }
        let crk = Crk::generate_with(rng);
        let rk = RecoveryKey::generate_with(rng);
        let wrapped = wrap_crk(&crk, &rk, rng);
        let body = HeaderBody {
            recovery_wrapped_crk: wrapped.clone(),
            binding: binding.clone(),
        };
        let header = encode_header(&body)?;
        // The header is the creation lock; a crash before the manifest below leaves an
        // incomplete container that `open_*` reports as `Error::IncompleteCreate`.
        if backend.put_if_absent(HEADER, &header).await? == PutOutcome::Conflict {
            return Err(Error::Conflict);
        }
        let subkeys = crk.subkeys();
        let mut genesis = Manifest::genesis(wrapped, binding);
        genesis.authenticate(&subkeys.manifest_auth)?;
        let sealed = manifest::seal(&genesis, &subkeys.manifest)?;
        let PutOutcome::Written(etag) = backend.put_if_absent(MANIFEST, &sealed).await? else {
            return Err(Error::Conflict);
        };
        let c = Self {
            backend,
            crk,
            subkeys,
            header: body,
            etag,
            hash: manifest::hash(&sealed),
            seq: 0,
            segments: Vec::new(),
            index: Index::default(),
            pending: BTreeMap::new(),
            segment_target: SEGMENT_TARGET,
            stored_bytes: 0,
        };
        Ok((c, rk))
    }

    /// Opens an existing container with its root key (e.g. unwrapped from the
    /// device keystore). `high_water` is the mark the caller persisted last time.
    pub async fn open_with_crk(
        backend: B,
        crk: Crk,
        high_water: Option<HighWater>,
    ) -> Result<Self> {
        let header = read_header(&backend).await?;
        Self::open_inner(backend, header, crk, high_water).await
    }

    /// Opens an existing container with the user's recovery key; also returns the
    /// unwrapped root key so the caller can re-protect it on this device.
    pub async fn open_with_recovery(
        backend: B,
        rk: &RecoveryKey,
        high_water: Option<HighWater>,
    ) -> Result<(Self, Crk)> {
        let header = read_header(&backend).await?;
        let crk = unwrap_crk(&header.recovery_wrapped_crk, rk)?;
        let crk_out = unwrap_crk(&header.recovery_wrapped_crk, rk)?;
        let c = Self::open_inner(backend, header, crk, high_water).await?;
        Ok((c, crk_out))
    }

    async fn open_inner(
        backend: B,
        header: HeaderBody,
        crk: Crk,
        high_water: Option<HighWater>,
    ) -> Result<Self> {
        let subkeys = crk.subkeys();
        let l = load(&backend, &subkeys, &header, high_water).await?;
        Ok(Self {
            backend,
            crk,
            subkeys,
            header,
            etag: l.etag,
            hash: l.hash,
            seq: l.seq,
            segments: l.segments,
            index: l.index,
            pending: BTreeMap::new(),
            segment_target: SEGMENT_TARGET,
            stored_bytes: l.stored_bytes,
        })
    }

    /// Returns the value at `key`, including uncommitted changes.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.lookup(key).map(<[u8]>::to_vec)
    }

    fn lookup(&self, key: &[u8]) -> Option<&[u8]> {
        match self.pending.get(key) {
            Some(v) => v.as_deref(),
            None => self.index.by_key.get(key).map(Vec::as_slice),
        }
    }

    /// Stores `value` at `key` (uncommitted until [`Container::commit`]).
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        check_size(key, value)?;
        if let Some(Some(mut old)) = self.pending.insert(key.to_vec(), Some(value.to_vec())) {
            old.zeroize();
        }
        Ok(())
    }

    /// Stores `value` only if `key` is absent (committed or pending); returns whether
    /// it was stored. Cross-device races are caught by the commit compare-and-swap.
    pub fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> Result<bool> {
        check_size(key, value)?;
        if self.lookup(key).is_some() {
            return Ok(false);
        }
        self.put(key, value)?;
        Ok(true)
    }

    /// Removes `key` (uncommitted until [`Container::commit`]).
    pub fn delete(&mut self, key: &[u8]) {
        if let Some(Some(mut old)) = self.pending.insert(key.to_vec(), None) {
            old.zeroize();
        }
    }

    /// All `(key, value)` pairs whose key starts with `prefix`, in key order,
    /// including uncommitted changes.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out: BTreeMap<&[u8], &[u8]> = self
            .index
            .by_key
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        for (k, v) in self
            .pending
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
        {
            match v {
                Some(v) => out.insert(k, v),
                None => out.remove(k.as_slice()),
            };
        }
        out.into_iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect()
    }

    /// Number of uncommitted keys.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Drops all uncommitted changes.
    pub fn discard_pending(&mut self) {
        wipe_pending(&mut self.pending);
    }

    /// Makes the pending batch durable and returns the new manifest `seq` (or the
    /// current one if there was nothing to write).
    ///
    /// On [`Error::Conflict`] another writer moved the manifest: nothing was
    /// committed, the batch is kept, and the caller may [`Container::reload`] and
    /// commit again, or discard it. On other errors the outcome may be unknown
    /// (e.g. the manifest write timed out after succeeding); the batch is kept and
    /// a later commit either succeeds or conflicts.
    pub async fn commit(&mut self) -> Result<u64> {
        if self.pending_records().next().is_none() {
            self.discard_pending();
            return Ok(self.seq);
        }
        let next = self.seq.checked_add(1).ok_or(Error::Conflict)?;
        let added: u64 = self
            .pending_records()
            .map(|r| record_bytes(&r) as u64)
            .sum();
        let new_ids = self.write_segments(self.pending_records()).await?;
        let mut segments = self.segments.clone();
        segments.extend_from_slice(&new_ids);
        self.publish(next, segments, &new_ids).await?;
        self.apply_pending();
        self.stored_bytes = self.stored_bytes.saturating_add(added);
        Ok(next)
    }

    /// Moves the pending batch into the committed index.
    fn apply_pending(&mut self) {
        for (key, value) in std::mem::take(&mut self.pending) {
            let mac = key_mac(&self.subkeys.mac, &key);
            match value {
                Some(value) => self.index.insert(mac, key, value),
                None => {
                    self.index.remove_mac(&mac);
                    let mut key = key;
                    key.zeroize();
                }
            }
        }
    }

    /// Rewrites the whole live state (including pending changes) into fresh segments,
    /// commits a manifest listing only them, then deletes the old segments
    /// (best effort; leftovers are removed by [`Container::gc_orphans`]).
    pub async fn compact(&mut self) -> Result<u64> {
        let next = self.seq.checked_add(1).ok_or(Error::Conflict)?;
        // Records are produced from the live view one segment at a time.
        let stored: u64 = self.live_records().map(|r| record_bytes(&r) as u64).sum();
        let new_ids = self.write_segments(self.live_records()).await?;
        let old = std::mem::take(&mut self.segments);
        if let Err(e) = self.publish(next, new_ids.clone(), &new_ids).await {
            self.segments = old;
            return Err(e);
        }
        self.apply_pending();
        self.stored_bytes = stored;
        for id in old.iter().filter(|id| !new_ids.contains(id)) {
            let _ = self.backend.delete(&seg_name(id)).await;
        }
        Ok(next)
    }

    /// Deletes `seg/` objects not referenced by the current manifest; returns how
    /// many were deleted.
    ///
    /// Refuses with [`Error::Conflict`] if the stored manifest is not the one this
    /// instance last committed or loaded, since another writer's segments would
    /// look like orphans. Run it only while no other writer is active on the
    /// backend: a segment written by another device whose manifest switch is still
    /// in flight is indistinguishable from an orphan.
    pub async fn gc_orphans(&mut self) -> Result<usize> {
        match self.backend.get(MANIFEST).await? {
            Some((_, etag)) if etag == self.etag => {}
            _ => return Err(Error::Conflict),
        }
        let live: HashSet<String> = self.segments.iter().map(seg_name).collect();
        let mut deleted = 0;
        for name in self.backend.list(SEG_PREFIX).await? {
            if !live.contains(&name) {
                self.backend.delete(&name).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Re-reads the stored manifest (refusing rollback against this instance's
    /// high-water mark) and replays it, keeping the pending batch on top.
    ///
    /// Pending changes then simply overwrite the reloaded state on commit; a
    /// `put_if_absent` decided before the reload is not re-evaluated.
    pub async fn reload(&mut self) -> Result<()> {
        let l = load(
            &self.backend,
            &self.subkeys,
            &self.header,
            Some(self.high_water()),
        )
        .await?;
        self.etag = l.etag;
        self.hash = l.hash;
        self.seq = l.seq;
        self.segments = l.segments;
        self.index = l.index;
        self.stored_bytes = l.stored_bytes;
        Ok(())
    }

    /// The mark to persist and pass to the next `open_*` call.
    pub fn high_water(&self) -> HighWater {
        HighWater {
            seq: self.seq,
            hash: self.hash,
        }
    }

    /// Size accounting of the committed state (pending changes excluded).
    pub fn stats(&self) -> Stats {
        Stats {
            segments: self.segments.len(),
            stored_bytes: self.stored_bytes,
            live_bytes: self
                .index
                .by_key
                .iter()
                .map(|(k, v)| record_size(k, v) as u64)
                .sum(),
        }
    }

    /// Sequence number of the current manifest.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Ids of the live segments, in replay order.
    pub fn segment_ids(&self) -> &[[u8; 16]] {
        &self.segments
    }

    /// The app id and (canonical) origin this container is bound to, as
    /// `(app_id, origin)`. Authenticated: opening checked them against the manifest.
    pub fn binding(&self) -> (&str, &str) {
        (&self.header.binding.app_id, &self.header.binding.origin)
    }

    /// The container root key.
    pub fn crk(&self) -> &Crk {
        &self.crk
    }

    /// The underlying backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Pending changes as records; deletes of keys absent from the committed state
    /// are dropped.
    fn pending_records(&self) -> impl Iterator<Item = Record> + Send + '_ {
        self.pending.iter().filter_map(|(k, v)| {
            let mac = key_mac(&self.subkeys.mac, k);
            match v {
                Some(v) => Some(Record::Put {
                    key_mac: mac,
                    key: Zeroizing::new(k.clone()),
                    value: Zeroizing::new(v.clone()),
                }),
                None => self
                    .index
                    .by_key
                    .contains_key(k)
                    .then_some(Record::Delete { key_mac: mac }),
            }
        })
    }

    /// The live view as `Put` records. A plain (non-async) fn so that the closure's
    /// lifetimes are inferred here, keeping `compact`'s future `Send`.
    fn live_records(&self) -> impl Iterator<Item = Record> + Send + '_ {
        let mac_key = &self.subkeys.mac;
        self.live_entries().map(move |(k, v)| Record::Put {
            key_mac: key_mac(mac_key, k),
            key: Zeroizing::new(k.to_vec()),
            value: Zeroizing::new(v.to_vec()),
        })
    }

    /// The committed index merged with pending changes, in key order, without copying.
    fn live_entries(&self) -> impl Iterator<Item = (&[u8], &[u8])> + Send + '_ {
        let mut committed = self.index.by_key.iter().peekable();
        let mut pending = self.pending.iter().peekable();
        std::iter::from_fn(move || {
            loop {
                let take_committed = match (committed.peek(), pending.peek()) {
                    (None, None) => return None,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (Some((c, _)), Some((p, _))) => {
                        if c == p {
                            committed.next(); // shadowed by the pending change
                            false
                        } else {
                            c < p
                        }
                    }
                };
                if take_committed {
                    let (k, v) = committed.next().expect("peeked");
                    return Some((k.as_slice(), v.as_slice()));
                }
                if let (k, Some(v)) = pending.next().expect("peeked") {
                    return Some((k.as_slice(), v.as_slice()));
                }
            }
        })
    }

    /// Writes `records` as one or more new segments. On error every segment this
    /// call attempted is deleted again (best effort): none is referenced yet.
    async fn write_segments(
        &self,
        records: impl Iterator<Item = Record> + Send,
    ) -> Result<Vec<[u8; 16]>> {
        let mut attempted = Vec::new();
        let result = self.write_segments_inner(records, &mut attempted).await;
        if result.is_err() {
            for id in &attempted {
                let _ = self.backend.delete(&seg_name(id)).await;
            }
        }
        result
    }

    /// Only one segment's worth of records is materialized at a time.
    async fn write_segments_inner(
        &self,
        records: impl Iterator<Item = Record> + Send,
        attempted: &mut Vec<[u8; 16]>,
    ) -> Result<Vec<[u8; 16]>> {
        let mut ids = Vec::new();
        let mut records = records.peekable();
        while records.peek().is_some() {
            let mut batch = Vec::new();
            let mut size = 0;
            while let Some(r) = records.peek() {
                let s = record_bytes(r);
                if !batch.is_empty() && size + s > self.segment_target {
                    break;
                }
                size += s;
                batch.push(records.next().expect("peeked"));
            }
            ids.push(self.write_segment(&batch, attempted).await?);
        }
        Ok(ids)
    }

    /// Writes one segment; on an id collision retries once with a fresh id.
    async fn write_segment(
        &self,
        records: &[Record],
        attempted: &mut Vec<[u8; 16]>,
    ) -> Result<[u8; 16]> {
        for _ in 0..2 {
            let id = random_segment_id();
            let mut w = SegmentWriter::new(&self.subkeys.enc, id);
            for r in records {
                w.push(r.clone())?;
            }
            let bytes = w.finish();
            match self.backend.put_if_absent(&seg_name(&id), &bytes).await {
                Ok(PutOutcome::Written(_)) => {
                    attempted.push(id);
                    return Ok(id);
                }
                // Someone else owns this name: never delete it.
                Ok(PutOutcome::Conflict) => {}
                Err(e) => {
                    attempted.push(id);
                    return Err(e);
                }
            }
        }
        Err(Error::SegmentIdCollision)
    }

    /// Seals and swaps in the manifest `seq`/`segments`. On a CAS conflict the
    /// just-written `new_ids` are deleted (best effort); on other errors they are
    /// kept, since the manifest write may have landed.
    async fn publish(
        &mut self,
        seq: u64,
        segments: Vec<[u8; 16]>,
        new_ids: &[[u8; 16]],
    ) -> Result<()> {
        let mut m = Manifest {
            seq,
            prev: self.hash,
            segments,
            recovery_wrapped_crk: self.header.recovery_wrapped_crk.clone(),
            binding: self.header.binding.clone(),
            auth_tag: [0u8; 32],
        };
        m.authenticate(&self.subkeys.manifest_auth)?;
        let sealed = manifest::seal(&m, &self.subkeys.manifest)?;
        match self
            .backend
            .put_if_match(MANIFEST, &sealed, &self.etag)
            .await?
        {
            PutOutcome::Written(etag) => {
                self.etag = etag;
                self.hash = manifest::hash(&sealed);
                self.seq = seq;
                self.segments = m.segments;
                Ok(())
            }
            PutOutcome::Conflict => {
                for id in new_ids {
                    let _ = self.backend.delete(&seg_name(id)).await;
                }
                Err(Error::Conflict)
            }
        }
    }
}

/// Reads and checks the `header` object only (marker, format version, layout,
/// binding), without any key. Tools call it before they touch stored keys, so a
/// container from before the split reports [`Error::PreV1Container`] (and an
/// unknown version [`Error::UnsupportedFormatVersion`]) rather than a key error.
pub async fn check_format<B: ContainerBackend + ?Sized>(backend: &B) -> Result<()> {
    read_header(backend).await.map(|_| ())
}

/// [`check_format`] on the bytes of a `header` object already read (for example
/// from a local container directory, without opening it as a backend).
pub fn check_header(bytes: &[u8]) -> Result<()> {
    decode_header(bytes).map(|_| ())
}

/// The app binding from the plaintext `header` object alone, without any key
/// (so a locked vault can refuse a wrong app before anything is unlocked).
/// Not authenticated by itself: opening the container checks that the
/// manifest, which the root key authenticates, carries the same binding.
pub async fn read_binding<B: ContainerBackend + ?Sized>(backend: &B) -> Result<Binding> {
    read_header(backend).await.map(|h| h.binding)
}

async fn read_header<B: ContainerBackend + ?Sized>(backend: &B) -> Result<HeaderBody> {
    match backend.get(HEADER).await? {
        Some((bytes, _)) => decode_header(&bytes),
        None => Err(Error::Format("no container header".into())),
    }
}

/// Reads and authenticates the manifest, enforces the high-water mark and replays
/// every listed segment. Restarts if a segment is missing and the manifest moved.
async fn load<B: ContainerBackend>(
    backend: &B,
    subkeys: &Subkeys,
    header: &HeaderBody,
    high_water: Option<HighWater>,
) -> Result<Loaded> {
    'attempt: for attempt in 0..LOAD_ATTEMPTS {
        let Some((bytes, etag)) = backend.get(MANIFEST).await? else {
            // Having held a mark proves a manifest existed: it was removed.
            return Err(if high_water.is_some() {
                Error::Rollback
            } else {
                Error::IncompleteCreate
            });
        };
        // AEAD and the inner auth tag: the manifest key alone cannot forge a manifest.
        let m = manifest::open_verified(&bytes, &subkeys.manifest, &subkeys.manifest_auth)?;
        let hash = manifest::hash(&bytes);
        check_high_water(&m, &hash, high_water)?;
        // The header is plaintext and unauthenticated on its own: its contents
        // count only because the authenticated manifest carries the same.
        if !header.matches(&m) {
            return Err(Error::Format("manifest does not match header".into()));
        }
        let mut index = Index::default();
        let mut stored_bytes = 0u64;
        for id in &m.segments {
            let Some((seg, _)) = backend.get(&seg_name(id)).await? else {
                let moved = match backend.get(MANIFEST).await? {
                    Some((_, now)) => now != etag,
                    None => false,
                };
                if moved && attempt + 1 < LOAD_ATTEMPTS {
                    continue 'attempt;
                }
                return Err(Error::Format(
                    "segment listed in manifest is missing".into(),
                ));
            };
            for record in SegmentReader::open(&subkeys.enc, *id, &seg)? {
                let record = record?;
                stored_bytes = stored_bytes.saturating_add(record_bytes(&record) as u64);
                index.replay(&subkeys.mac, record)?;
            }
        }
        return Ok(Loaded {
            etag,
            hash,
            seq: m.seq,
            segments: m.segments,
            index,
            stored_bytes,
        });
    }
    Err(Error::Conflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemoryBackend;
    use rand::SeedableRng;

    fn bind() -> Binding {
        Binding::new("test-app", "https://app.example").unwrap()
    }

    #[tokio::test]
    async fn large_batches_split_into_bounded_segments() {
        let backend = MemoryBackend::new();
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(1);
        let (mut c, rk) = Container::create_with(backend.clone(), bind(), &mut rng)
            .await
            .unwrap();
        c.segment_target = 4096;
        for i in 0..40u32 {
            c.put(format!("k{i:02}").as_bytes(), &[i as u8; 500])
                .unwrap();
        }
        c.commit().await.unwrap();
        let after_commit = c.segment_ids().len();
        assert!(after_commit >= 5, "got {after_commit} segments");
        let state = c.scan_prefix(b"");

        c.put(b"k00", b"new").unwrap();
        c.compact().await.unwrap();
        assert!(c.segment_ids().len() >= 5);
        assert_eq!(
            backend.list(SEG_PREFIX).await.unwrap().len(),
            c.segment_ids().len()
        );
        let hw = c.high_water();
        drop(c);
        let (c, _) = Container::open_with_recovery(backend, &rk, Some(hw))
            .await
            .unwrap();
        let mut expect = state;
        expect[0].1 = b"new".to_vec();
        assert_eq!(c.scan_prefix(b""), expect);
    }

    /// The binding is readable from the header alone: no key, no manifest read.
    #[tokio::test]
    async fn binding_readable_from_header_without_key() {
        let backend = MemoryBackend::new();
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(2);
        Container::create_with(backend.clone(), bind(), &mut rng)
            .await
            .unwrap();
        backend.delete(MANIFEST).await.unwrap();
        assert_eq!(read_binding(&backend).await.unwrap(), bind());
        assert!(matches!(
            read_binding(&MemoryBackend::new()).await,
            Err(Error::Format(_))
        ));
    }

    #[test]
    fn header_roundtrip_and_hostile_input() {
        let h = HeaderBody {
            recovery_wrapped_crk: vec![1, 2, 3],
            binding: Binding::new("ab", "https://c.d").unwrap(),
        };
        let bytes = encode_header(&h).unwrap();
        // marker, then the format version, then postcard(body): wrapped CRK, app id, origin
        assert_eq!(bytes, b"vault\x01\x03\x01\x02\x03\x02ab\x0bhttps://c.d");
        let back = decode_header(&bytes).unwrap();
        assert_eq!(back, h);
        // An invalid or non-canonical stored binding is refused.
        for (app, origin) in [
            ("AB", "https://c.d"),
            ("ab", "https://C.d"),
            ("ab", "http://c.d"),
        ] {
            let bad = encode_header(&HeaderBody {
                recovery_wrapped_crk: vec![1],
                binding: Binding {
                    app_id: app.into(),
                    origin: origin.into(),
                },
            })
            .unwrap();
            assert!(
                matches!(decode_header(&bad), Err(Error::Format(_))),
                "{app} {origin}"
            );
        }
        assert!(matches!(decode_header(&[]), Err(Error::Format(_))));
        assert!(matches!(decode_header(b"vault"), Err(Error::Format(_))));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(decode_header(&trailing), Err(Error::Format(_))));
        assert!(matches!(
            decode_header(&vec![1u8; MAX_HEADER + 1]),
            Err(Error::Format(_))
        ));
        let big = encode_header(&HeaderBody {
            recovery_wrapped_crk: vec![0; MAX_WRAPPED_CRK + 1],
            binding: bind(),
        })
        .unwrap();
        assert!(matches!(decode_header(&big), Err(Error::Format(_))));
    }

    #[test]
    fn header_rejects_unknown_format_version() {
        assert_eq!(crate::CONTAINER_FORMAT_VERSION, 1);
        for v in [0u8, 2, 0xff] {
            let mut bytes = b"vault".to_vec();
            bytes.push(v);
            bytes.extend_from_slice(&[3, 1, 2, 3]);
            assert!(
                matches!(
                    decode_header(&bytes),
                    Err(Error::UnsupportedFormatVersion(got)) if got == v
                ),
                "version {v}"
            );
        }
        // Checked before the body: an unknown version with any body is refused as such.
        assert!(matches!(
            decode_header(b"vault\x02"),
            Err(Error::UnsupportedFormatVersion(2))
        ));
        assert_eq!(
            Error::UnsupportedFormatVersion(2).to_string(),
            "unsupported container format version 2"
        );
        // No marker: a header from before the split (first byte 1), or not a header.
        assert!(matches!(
            decode_header(&[1, 3, 1, 2, 3]),
            Err(Error::PreV1Container)
        ));
        assert_eq!(
            Error::PreV1Container.to_string(),
            "unsupported container format (created before vault v1)"
        );
        assert!(matches!(
            decode_header(&[2, 3, 1, 2, 3]),
            Err(Error::Format(_))
        ));
    }

    #[test]
    fn high_water_checks() {
        let h = [1u8; 32];
        let m = |seq, prev| Manifest {
            seq,
            prev,
            segments: Vec::new(),
            recovery_wrapped_crk: Vec::new(),
            binding: bind(),
            auth_tag: [0; 32],
        };
        let hw = |seq, hash| Some(HighWater { seq, hash });
        assert!(check_high_water(&m(5, [2; 32]), &h, None).is_ok());
        // direct successor must chain to the mark
        assert!(check_high_water(&m(5, [2; 32]), &h, hw(4, [2; 32])).is_ok());
        assert!(matches!(
            check_high_water(&m(5, [2; 32]), &h, hw(4, [3; 32])),
            Err(Error::Rollback)
        ));
        // further ahead: accepted
        assert!(check_high_water(&m(5, [2; 32]), &h, hw(3, [3; 32])).is_ok());
        assert!(check_high_water(&m(5, [2; 32]), &h, hw(5, h)).is_ok());
        assert!(matches!(
            check_high_water(&m(5, [2; 32]), &h, hw(5, [0; 32])),
            Err(Error::Rollback)
        ));
        assert!(matches!(
            check_high_water(&m(4, [2; 32]), &h, hw(5, h)),
            Err(Error::Rollback)
        ));
        // u64::MAX mark: nothing can follow, no overflow
        assert!(check_high_water(&m(u64::MAX, [2; 32]), &h, hw(u64::MAX, h)).is_ok());
    }
}
