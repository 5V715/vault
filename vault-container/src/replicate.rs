// SPDX-License-Identifier: MIT

//! Replication of the sealed container to several replicas, and restore from them.
//!
//! Only objects the backends already hold are copied: the `header` (recovery-wrapped
//! CRK, no plaintext identifiers), encrypted `seg/` objects and the sealed `manifest`.
//!
//! [`ReplicaSet::replicate_once`] brings every replica to the primary's current
//! manifest, each replica independently and concurrently, in two phases: (1)
//! classification of the replica's header and manifest (a diverged replica is left
//! untouched), then our header if it has none and upload of the segments it lacks;
//! then the caller's persist hook stores the marks about to be written
//! ([`ReplicaRecord::in_flight`]); (2) the manifest with compare-and-swap, a re-read
//! of the replica's manifest, and only then deletion of unreferenced segments. It is
//! idempotent; retry and backoff are the caller's job ([`ReplicaRecord::last_error`]).
//!
//! Anti-rollback per replica: [`ReplicaState`] keeps the last manifest mark written
//! to each replica (and one possibly written, `in_flight`). A replica serving an older
//! `seq` (or no manifest) is reported as [`ReplicaOutcome::Rollback`] and repaired; a
//! replica whose manifest is newer than the primary, forks from it, or is neither mark
//! is [`ReplicaOutcome::Diverged`] and is overwritten only after the user confirms
//! ([`ReplicaSet::adopt_diverged`]). After a restore, replicas the vault has no mark
//! for are overwritten only up to [`ReplicaState::restored_floor`].
//!
//! [`restore`] reads every reachable replica, unwraps the CRK with the recovery key
//! and copies the highest manifest whose every segment verifies onto an empty target.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::backend::{ContainerBackend, Etag, PutOutcome};
use crate::engine::{HeaderBody, HighWater, decode_header, seg_name};
use crate::manifest::{self, Manifest};
use crate::segment::SegmentReader;
use crate::{Crk, Error, RecoveryKey, Result, Subkeys, unwrap_crk};

const HEADER: &str = "header";
const MANIFEST: &str = "manifest";
const SEG_PREFIX: &str = "seg/";

/// Caller-chosen, stable name of a replica (e.g. a config entry id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReplicaId(pub String);

/// What the vault remembers about one replica.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaRecord {
    /// Manifest mark last written to (or found in sync on) the replica.
    pub last_seen: Option<HighWater>,
    /// Mark persisted just before a manifest write whose outcome is not yet recorded;
    /// a replica holding it is ours (e.g. crash after the write).
    pub in_flight: Option<HighWater>,
    /// The user confirmed overwriting this diverged replica with the primary, as it
    /// was observed then; cleared once used or when the replica no longer matches.
    pub adopt: Option<ObservedMark>,
    /// Wall-clock time (ms since the Unix epoch) of the last successful sync.
    pub last_ok_at_ms: Option<u64>,
    /// Error of the last attempt, `None` after a successful one.
    pub last_error: Option<String>,
}

/// What a diverged replica held when it was reported; the user's confirmation in
/// [`ReplicaSet::adopt_diverged`] is bound to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservedMark {
    /// The replica's manifest: `seq` if it could be opened, and SHA-256 of its bytes.
    Manifest { seq: Option<u64>, hash: [u8; 32] },
    /// The replica's header belongs to another container: SHA-256 of its bytes.
    Header { hash: [u8; 32] },
}

/// Replication state; the caller persists it (encrypted) and passes it back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaState {
    pub replicas: BTreeMap<ReplicaId, ReplicaRecord>,
    /// Set by [`restore`]: the mark restored from. A replica without a mark of its
    /// own is overwritten only if it is not ahead of this (it may hold newer history
    /// that was unreachable during the restore).
    pub restored_floor: Option<HighWater>,
}

/// Result of one replica in [`ReplicaSet::replicate_once`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaOutcome {
    /// The replica now holds the primary's manifest `seq`.
    Synced {
        seq: u64,
        /// Segments uploaded in this call.
        uploaded: usize,
        /// Unreferenced replica segments deleted in this call.
        deleted: usize,
    },
    /// The replica served an older manifest (`found_seq`, `None` = no manifest) than
    /// the mark last written there; it was repaired to the primary's `seq`.
    Rollback {
        found_seq: Option<u64>,
        last_seen_seq: u64,
        seq: u64,
    },
    /// The replica holds a manifest that is newer than, forks from, or does not
    /// belong to the primary's container (`replica_seq` is `None` if it cannot be
    /// opened). Nothing was written to it (header, manifest, segments); see
    /// [`ReplicaSet::adopt_diverged`].
    Diverged {
        replica_seq: Option<u64>,
        primary_seq: u64,
        /// Pass this to [`ReplicaSet::adopt_diverged`] after the user confirms.
        observed: ObservedMark,
    },
    /// After the user's confirmation, the diverged replica (`replaced`) was
    /// overwritten with the primary's `seq`. `rollback` is set if the replaced
    /// manifest was older than the mark last written there.
    Adopted {
        seq: u64,
        replaced: ObservedMark,
        rollback: bool,
    },
    /// The attempt failed (network, provider, concurrent change); retry later.
    Failed { error: String },
}

/// Status of one replica after [`ReplicaSet::replicate_once`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaStatus {
    pub id: ReplicaId,
    pub outcome: ReplicaOutcome,
}

/// A primary backend and the replicas it is copied to.
pub struct ReplicaSet {
    primary: Arc<dyn ContainerBackend>,
    replicas: Vec<(ReplicaId, Arc<dyn ContainerBackend>)>,
    state: ReplicaState,
}

/// The primary's current objects, read once per [`ReplicaSet::replicate_once`].
struct Source {
    header: Vec<u8>,
    sealed: Vec<u8>,
    mark: HighWater,
    segments: Vec<String>,
    /// The decoded header (wrapped CRK and binding), checked against the manifest.
    body: HeaderBody,
}

fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn same_mark(a: &HighWater, b: &HighWater) -> bool {
    a.seq == b.seq && bool::from(a.hash.ct_eq(&b.hash))
}

fn failed(error: impl Into<String>) -> ReplicaOutcome {
    ReplicaOutcome::Failed {
        error: error.into(),
    }
}

/// Phase-one result for one replica.
enum Prepared {
    Done(ReplicaOutcome),
    Ready(Ready),
}

struct Ready {
    /// Adoption of a foreign header: replace the header at this etag first.
    header_etag: Option<Etag>,
    /// Set when this write overwrites a diverged replica the user confirmed.
    adopted: Option<(ObservedMark, bool)>,
    /// Manifest to write: `None` = up to date; `Some(None)` = create; else CAS etag.
    write: Option<Option<Etag>>,
    rollback: Option<(Option<u64>, u64)>,
    /// The replica's manifest was recognised via `in_flight`: it is ours.
    promote: Option<HighWater>,
    present: Vec<String>,
    uploaded: usize,
}

impl ReplicaSet {
    /// State entries of replicas not in `replicas` are kept unchanged (a replica may
    /// be left out of one run); drop them explicitly with [`ReplicaSet::forget_replica`].
    pub fn new(
        primary: Arc<dyn ContainerBackend>,
        replicas: Vec<(ReplicaId, Arc<dyn ContainerBackend>)>,
        state: ReplicaState,
    ) -> Self {
        Self {
            primary,
            replicas,
            state,
        }
    }

    /// Drops everything remembered about replica `id` (its anti-rollback marks
    /// included), e.g. after the user removed it from the configuration. Returns
    /// whether there was any state. Persist the state afterwards.
    pub fn forget_replica(&mut self, id: &ReplicaId) -> bool {
        self.state.replicas.remove(id).is_some()
    }

    /// The state to persist after each call.
    pub fn state(&self) -> &ReplicaState {
        &self.state
    }

    /// Records the user's confirmation that the diverged replica `id`, as reported in
    /// `confirmed` (the `observed` mark of the [`ReplicaOutcome::Diverged`] shown to
    /// the user), may be overwritten (header, manifest, unreferenced segments) by the
    /// next [`ReplicaSet::replicate_once`]. If the replica no longer matches
    /// `confirmed` by then, it is reported diverged again and the confirmation is
    /// dropped. Persist the state afterwards.
    pub fn adopt_diverged(&mut self, id: &ReplicaId, confirmed: ObservedMark) -> Result<()> {
        if !self.replicas.iter().any(|(r, _)| r == id) {
            return Err(Error::Format(format!("unknown replica {:?}", id.0)));
        }
        self.state.replicas.entry(id.clone()).or_default().adopt = Some(confirmed);
        Ok(())
    }

    /// Syncs every replica to the primary's current manifest; one status per replica,
    /// in configuration order. `manifest_key` (`Subkeys::manifest`) is needed to read
    /// the primary's segment list and the replicas' `seq`.
    ///
    /// `persist` is called with the state right before any manifest is written (with
    /// [`ReplicaRecord::in_flight`] set); if it fails, no manifest is written. Persist
    /// [`ReplicaSet::state`] again after the call returns.
    pub async fn replicate_once(
        &mut self,
        manifest_key: &[u8; 32],
        mut persist: impl FnMut(&ReplicaState) -> Result<()>,
    ) -> Vec<ReplicaStatus> {
        let source = match read_source(self.primary.as_ref(), manifest_key).await {
            Ok(s) => s,
            Err(e) => {
                let outcomes = vec![failed(format!("primary: {e}")); self.replicas.len()];
                return self.finish(outcomes, None);
            }
        };
        // Phase 1: header, classification, segments.
        let jobs = self.replicas.iter().map(|(id, replica)| {
            let rec = self.state.replicas.get(id).cloned().unwrap_or_default();
            let floor = self.state.restored_floor;
            let primary = self.primary.as_ref();
            let source = &source;
            async move {
                prepare(primary, replica.as_ref(), source, &rec, floor, manifest_key)
                    .await
                    .unwrap_or_else(|e| Prepared::Done(failed(e.to_string())))
            }
        });
        let prepared = join_all(jobs).await;

        // Persist the marks about to be written before any manifest write.
        let mut writes = false;
        for ((id, _), p) in self.replicas.iter().zip(&prepared) {
            if let Prepared::Ready(r) = p {
                let rec = self.state.replicas.entry(id.clone()).or_default();
                if let Some(m) = r.promote {
                    rec.last_seen = Some(m);
                }
                if r.write.is_some() {
                    rec.in_flight = Some(source.mark);
                    writes = true;
                }
            }
        }
        let persist_error = if writes {
            persist(&self.state)
                .err()
                .map(|e| format!("persist state: {e}"))
        } else {
            None
        };

        // Phase 2: manifest, re-check, deletes.
        let jobs = self.replicas.iter().zip(prepared).map(|((_, replica), p)| {
            let source = &source;
            let persist_error = persist_error.clone();
            async move {
                match p {
                    Prepared::Done(o) => o,
                    Prepared::Ready(r) => match persist_error {
                        Some(e) if r.write.is_some() => failed(e),
                        _ => commit(replica.as_ref(), source, r)
                            .await
                            .unwrap_or_else(|e| failed(e.to_string())),
                    },
                }
            }
        });
        let outcomes = join_all(jobs).await;
        self.finish(outcomes, Some(source.mark))
    }

    fn finish(
        &mut self,
        outcomes: Vec<ReplicaOutcome>,
        mark: Option<HighWater>,
    ) -> Vec<ReplicaStatus> {
        let mut out = Vec::with_capacity(outcomes.len());
        for ((id, _), outcome) in self.replicas.iter().zip(outcomes) {
            let rec = self.state.replicas.entry(id.clone()).or_default();
            match &outcome {
                ReplicaOutcome::Synced { .. }
                | ReplicaOutcome::Rollback { .. }
                | ReplicaOutcome::Adopted { .. } => {
                    rec.last_seen = mark;
                    rec.in_flight = None;
                    rec.adopt = None;
                    rec.last_ok_at_ms = now_ms();
                    rec.last_error = None;
                }
                ReplicaOutcome::Diverged {
                    replica_seq,
                    primary_seq,
                    ..
                } => {
                    rec.adopt = None;
                    rec.last_error = Some(match replica_seq {
                        Some(s) => format!("diverged: replica seq {s}, primary seq {primary_seq}"),
                        None => "diverged: replica holds a foreign container".into(),
                    });
                }
                ReplicaOutcome::Failed { error } => rec.last_error = Some(error.clone()),
            }
            out.push(ReplicaStatus {
                id: id.clone(),
                outcome,
            });
        }
        out
    }
}

async fn read_source(primary: &dyn ContainerBackend, key: &[u8; 32]) -> Result<Source> {
    let (header, _) = primary
        .get(HEADER)
        .await?
        .ok_or_else(|| Error::Format("no container header".into()))?;
    let (sealed, _) = primary
        .get(MANIFEST)
        .await?
        .ok_or_else(|| Error::Format("no manifest".into()))?;
    let m = manifest::open(&sealed, key)?;
    // Never spread a header that does not belong to the manifest it travels with.
    let body = decode_header(&header)?;
    if !body.matches(&m) {
        return Err(Error::Format(
            "primary header does not match its manifest".into(),
        ));
    }
    Ok(Source {
        header,
        mark: HighWater {
            seq: m.seq,
            hash: manifest::hash(&sealed),
        },
        segments: m.segments.iter().map(seg_name).collect(),
        body,
        sealed,
    })
}

/// How the replica's current manifest relates to the primary's.
enum Plan {
    /// Already the primary's manifest.
    InSync,
    /// Write the primary's manifest: `None` = create, `Some` = replace this etag.
    Write {
        etag: Option<Etag>,
        rollback: Option<(Option<u64>, u64)>,
        promote: Option<HighWater>,
        adopted: Option<(ObservedMark, bool)>,
    },
    Diverged(Option<u64>, ObservedMark),
}

fn classify(
    replica: Option<(Vec<u8>, Etag)>,
    source: &Source,
    rec: &ReplicaRecord,
    floor: Option<HighWater>,
    key: &[u8; 32],
) -> Result<Plan> {
    let p = source.mark;
    let Some((bytes, etag)) = replica else {
        return Ok(Plan::Write {
            etag: None,
            rollback: rec.last_seen.map(|ls| (None, ls.seq)),
            promote: None,
            adopted: None,
        });
    };
    let hash = manifest::hash(&bytes);
    let seq = match manifest::open(&bytes, key) {
        Ok(m) => Some(m.seq),
        Err(Error::Crypto | Error::Format(_) | Error::Unsupported(_)) => None,
        Err(e) => return Err(e),
    };
    let observed = ObservedMark::Manifest { seq, hash };
    let write = |rollback, promote, adopted| Plan::Write {
        etag: Some(etag.clone()),
        rollback,
        promote,
        adopted,
    };
    // Only exactly what the user confirmed is overwritten.
    if rec.adopt == Some(observed) {
        let rollback = matches!((seq, rec.last_seen), (Some(s), Some(ls)) if s < ls.seq);
        return Ok(write(None, None, Some((observed, rollback))));
    }
    let Some(seq) = seq else {
        return Ok(Plan::Diverged(None, observed));
    };
    let r = HighWater { seq, hash };
    if same_mark(&r, &p) {
        return Ok(Plan::InSync);
    }
    if r.seq >= p.seq {
        return Ok(Plan::Diverged(Some(r.seq), observed));
    }
    let is = |m: Option<HighWater>| m.is_some_and(|m| same_mark(&r, &m));
    Ok(match rec.last_seen {
        Some(ls) if r.seq < ls.seq => write(Some((Some(r.seq), ls.seq)), None, None),
        _ if is(rec.last_seen) => write(None, None, None),
        _ if is(rec.in_flight) => write(None, Some(r), None),
        Some(_) => Plan::Diverged(Some(r.seq), observed),
        None if rec.in_flight.is_some() => Plan::Diverged(Some(r.seq), observed),
        None => match floor {
            Some(f) if r.seq > f.seq || (r.seq == f.seq && !same_mark(&r, &f)) => {
                Plan::Diverged(Some(r.seq), observed)
            }
            _ => write(None, None, None),
        },
    })
}

async fn prepare(
    primary: &dyn ContainerBackend,
    replica: &dyn ContainerBackend,
    source: &Source,
    rec: &ReplicaRecord,
    floor: Option<HighWater>,
    key: &[u8; 32],
) -> Result<Prepared> {
    let diverged = |replica_seq, observed| {
        Prepared::Done(ReplicaOutcome::Diverged {
            replica_seq,
            primary_seq: source.mark.seq,
            observed,
        })
    };
    // 1. Classify before writing anything: the header (compared by the wrapped CRK
    // and the binding it carries), then the manifest. A diverged replica gets nothing from us, not
    // even our header.
    // TODO(recovery-key rotation): rotation changes the wrapped CRK; replicas' headers
    // will then need an explicit update instead of being reported diverged.
    let mut header_etag = None;
    let mut header_adopted = None;
    let replica_header = replica.get(HEADER).await?;
    if let Some((h, etag)) = &replica_header {
        let ours = decode_header(h).is_ok_and(|b| b == source.body);
        if !ours {
            let observed = ObservedMark::Header {
                hash: manifest::hash(h),
            };
            if rec.adopt != Some(observed) {
                return Ok(diverged(None, observed));
            }
            // Replaced in phase two, after the persist hook ran.
            header_etag = Some(etag.clone());
            header_adopted = Some((observed, false));
        }
    }
    let current = replica.get(MANIFEST).await?;
    let (write, rollback, promote, adopted) = if header_adopted.is_some() {
        // Another container: its manifest goes with the confirmed header.
        let etag = current.map(|(_, e)| e);
        (Some(etag), None, None, header_adopted)
    } else {
        match classify(current, source, rec, floor, key)? {
            Plan::Diverged(s, o) => return Ok(diverged(s, o)),
            Plan::InSync => (None, None, None, None),
            Plan::Write {
                etag,
                rollback,
                promote,
                adopted,
            } => (Some(etag), rollback, promote, adopted),
        }
    };
    // 2. The header (written once), if the replica has none yet.
    if replica_header.is_none()
        && replica.put_if_absent(HEADER, &source.header).await? == PutOutcome::Conflict
    {
        match replica.get(HEADER).await? {
            Some((h, _)) if h == source.header => {}
            _ => return Err(Error::Io("replica header changed concurrently".into())),
        }
    }
    // 3. Segments listed in the primary's manifest that the replica lacks.
    let present = replica.list(SEG_PREFIX).await?;
    let have: HashSet<&String> = present.iter().collect();
    let mut uploaded = 0;
    for name in source.segments.iter().filter(|n| !have.contains(n)) {
        let (data, _) = primary
            .get(name)
            .await?
            .ok_or_else(|| Error::Format(format!("primary lacks listed {name}")))?;
        if replica.put_if_absent(name, &data).await? == PutOutcome::Conflict {
            match replica.get(name).await? {
                Some((d, _)) if d == data => {}
                _ => return Err(Error::Format(format!("replica holds a different {name}"))),
            }
        } else {
            uploaded += 1;
        }
    }
    Ok(Prepared::Ready(Ready {
        header_etag,
        adopted,
        write,
        rollback,
        promote,
        present,
        uploaded,
    }))
}

async fn commit(
    replica: &dyn ContainerBackend,
    source: &Source,
    r: Ready,
) -> Result<ReplicaOutcome> {
    // 4a. Confirmed adoption of a foreign container: its header first (CAS).
    if let Some(etag) = &r.header_etag
        && replica.put_if_match(HEADER, &source.header, etag).await? == PutOutcome::Conflict
    {
        return Ok(failed("replica header changed concurrently"));
    }
    // 4b. Manifest with compare-and-swap against what was classified.
    if let Some(etag) = &r.write {
        let outcome = match etag {
            None => replica.put_if_absent(MANIFEST, &source.sealed).await?,
            Some(e) => replica.put_if_match(MANIFEST, &source.sealed, e).await?,
        };
        if outcome == PutOutcome::Conflict {
            return Ok(failed("replica manifest changed concurrently"));
        }
    }
    // 5. Unreferenced segments, only if the replica still serves our manifest.
    match replica.get(MANIFEST).await? {
        Some((m, _)) if bool::from(manifest::hash(&m).ct_eq(&source.mark.hash)) => {}
        _ => return Ok(failed("replica manifest changed concurrently")),
    }
    let live: HashSet<&String> = source.segments.iter().collect();
    let mut deleted = 0;
    for name in r.present.iter().filter(|n| !live.contains(n)) {
        replica.delete(name).await?;
        deleted += 1;
    }
    let seq = source.mark.seq;
    Ok(match (r.adopted, r.rollback) {
        (Some((replaced, rollback)), _) => ReplicaOutcome::Adopted {
            seq,
            replaced,
            rollback,
        },
        (None, Some((found_seq, last_seen_seq))) => ReplicaOutcome::Rollback {
            found_seq,
            last_seen_seq,
            seq,
        },
        (None, None) => ReplicaOutcome::Synced {
            seq,
            uploaded: r.uploaded,
            deleted,
        },
    })
}

/// Outcome of [`restore`].
#[derive(Debug)]
pub struct RestoreReport {
    /// The replica that was copied.
    pub chosen: ReplicaId,
    /// Its manifest `seq`.
    pub seq: u64,
    /// Valid replicas behind the chosen one (stale or rolled back).
    pub rollbacks: Vec<ReplicaId>,
    /// Replicas forked from the choice (same `seq`, other manifest) or from the
    /// caller's high-water mark.
    pub diverged: Vec<ReplicaId>,
    /// Replicas that could not be read.
    pub unreachable: Vec<ReplicaId>,
    /// Replicas whose header, manifest or segments failed to verify.
    pub broken: Vec<ReplicaId>,
    /// The unwrapped root key, to re-protect on this device (wiped on drop).
    pub crk: Crk,
    /// Initial replication state for [`ReplicaSet::new`] on the restored primary: the
    /// chosen replica and those holding the same manifest are marked as seen, and
    /// [`ReplicaState::restored_floor`] keeps replicas ahead of the choice (newer
    /// history that was unreachable) from being overwritten.
    pub state: ReplicaState,
}

struct Candidate {
    id: ReplicaId,
    backend: Arc<dyn ContainerBackend>,
    header: Vec<u8>,
    sealed: Vec<u8>,
    manifest: Manifest,
    hash: [u8; 32],
    crk: Crk,
    subkeys: Subkeys,
}

enum ReadFail {
    Unreachable,
    Broken,
    WrongKey,
}

enum ChainFail {
    Unreachable,
    Broken,
    Target(Error),
}

async fn read_candidate(
    id: ReplicaId,
    backend: Arc<dyn ContainerBackend>,
    rk: &RecoveryKey,
) -> std::result::Result<Candidate, (ReplicaId, ReadFail)> {
    let fail = |e: Error| match e {
        Error::Io(_) => ReadFail::Unreachable,
        _ => ReadFail::Broken,
    };
    let header = match backend.get(HEADER).await {
        Ok(Some((h, _))) => h,
        Ok(None) => return Err((id, ReadFail::Broken)),
        Err(e) => return Err((id, fail(e))),
    };
    let body = match decode_header(&header) {
        Ok(b) => b,
        Err(_) => return Err((id, ReadFail::Broken)),
    };
    let crk = match unwrap_crk(&body.recovery_wrapped_crk, rk) {
        Ok(c) => c,
        Err(Error::Crypto) => return Err((id, ReadFail::WrongKey)),
        Err(_) => return Err((id, ReadFail::Broken)),
    };
    let sealed = match backend.get(MANIFEST).await {
        Ok(Some((m, _))) => m,
        Ok(None) => return Err((id, ReadFail::Broken)),
        Err(e) => return Err((id, fail(e))),
    };
    let subkeys = crk.subkeys();
    // AEAD and inner auth tag: a manifest resealed with only the manifest key is broken.
    let manifest = match manifest::open_verified(&sealed, &subkeys.manifest, &subkeys.manifest_auth)
    {
        Ok(m) if body.matches(&m) => m,
        _ => return Err((id, ReadFail::Broken)),
    };
    Ok(Candidate {
        id,
        backend,
        hash: manifest::hash(&sealed),
        header,
        sealed,
        manifest,
        crk,
        subkeys,
    })
}

/// Refuses manifests older than the mark or forked from it (as the engine does).
fn passes_mark(c: &Candidate, hw: Option<HighWater>) -> std::result::Result<(), bool> {
    let Some(hw) = hw else { return Ok(()) };
    let m = &c.manifest;
    if m.seq < hw.seq {
        Err(true) // rollback
    } else if (m.seq == hw.seq && !bool::from(c.hash.ct_eq(&hw.hash)))
        || (hw.seq.checked_add(1) == Some(m.seq) && !bool::from(m.prev.ct_eq(&hw.hash)))
    {
        Err(false) // fork
    } else {
        Ok(())
    }
}

/// Fetches and verifies every listed segment; with a target, also writes the whole
/// chain there (segments, header, manifest) and removes it again on failure.
async fn check_chain(
    c: &Candidate,
    target: Option<&dyn ContainerBackend>,
) -> std::result::Result<(), ChainFail> {
    let mut written = Vec::new();
    let r = check_chain_inner(c, target, &mut written).await;
    if r.is_err()
        && let Some(t) = target
    {
        for name in written.iter().rev() {
            let _ = t.delete(name).await;
        }
    }
    r
}

async fn put_new(
    t: &dyn ContainerBackend,
    name: String,
    data: &[u8],
) -> std::result::Result<String, ChainFail> {
    match t.put_if_absent(&name, data).await {
        Ok(PutOutcome::Written(_)) => Ok(name),
        Ok(PutOutcome::Conflict) => Err(ChainFail::Target(Error::Conflict)),
        Err(e) => Err(ChainFail::Target(e)),
    }
}

async fn check_chain_inner(
    c: &Candidate,
    target: Option<&dyn ContainerBackend>,
    written: &mut Vec<String>,
) -> std::result::Result<(), ChainFail> {
    for id in &c.manifest.segments {
        let name = seg_name(id);
        let data = match c.backend.get(&name).await {
            Ok(Some((d, _))) => d,
            Ok(None) => return Err(ChainFail::Broken),
            Err(Error::Io(_)) => return Err(ChainFail::Unreachable),
            Err(_) => return Err(ChainFail::Broken),
        };
        let reader =
            SegmentReader::open(&c.subkeys.enc, *id, &data).map_err(|_| ChainFail::Broken)?;
        for record in reader {
            record.map_err(|_| ChainFail::Broken)?;
        }
        if let Some(t) = target {
            written.push(put_new(t, name, &data).await?);
        }
    }
    if let Some(t) = target {
        written.push(put_new(t, HEADER.into(), &c.header).await?);
        written.push(put_new(t, MANIFEST.into(), &c.sealed).await?);
    }
    Ok(())
}

/// Restores the container onto the empty backend `target` from whichever replicas
/// are reachable, using only the recovery key.
///
/// Picks the highest `seq` whose every listed segment can be fetched and verified.
/// If the highest `seq` holding a verifiable manifest also holds a different
/// manifest (verifiable or not), the user must choose
/// ([`Error::DivergedReplicas`]; call again with only the chosen replica). With
/// `known_high_water`, anything older or forked from it is refused
/// ([`Error::Rollback`] if nothing else is left). A target holding only `seg/`
/// objects and at most the chosen replica's header, but no manifest (an interrupted
/// restore), has them deleted, but only once a chain has verified and right before
/// it is copied; any other non-empty target fails with [`Error::Conflict`]. `target_id` is the caller's configuration identity of the
/// target; if a replica in `replicas` has the same id the target would alias a
/// replica, and restore fails with [`Error::Conflict`] before touching anything. Fails with [`Error::Crypto`] if the recovery key opens
/// no replica, [`Error::Format`] if no replica holds a complete chain.
pub async fn restore(
    replicas: &[(ReplicaId, Arc<dyn ContainerBackend>)],
    rk: &RecoveryKey,
    target_id: &ReplicaId,
    target: &dyn ContainerBackend,
    known_high_water: Option<HighWater>,
) -> Result<RestoreReport> {
    if replicas.iter().any(|(id, _)| id == target_id) {
        return Err(Error::Conflict);
    }
    if target.get(MANIFEST).await?.is_some() {
        return Err(Error::Conflict);
    }
    // Segments, and a header without a manifest, are leftovers of an interrupted
    // restore. They are deleted only right before the copy (and a header only if it
    // is the chosen replica's), so a failed restore never loses them.
    let leftover_header = target.get(HEADER).await?.map(|(h, _)| h);
    let leftovers = target.list(SEG_PREFIX).await?;
    let clean_target = leftovers.is_empty() && leftover_header.is_none();
    let reads = join_all(
        replicas
            .iter()
            .map(|(id, b)| read_candidate(id.clone(), Arc::clone(b), rk)),
    )
    .await;
    let (mut unreachable, mut broken, mut diverged, mut rollbacks) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut wrong_key = Vec::new();
    let mut candidates = Vec::new();
    for r in reads {
        match r {
            Ok(c) => candidates.push(c),
            Err((id, ReadFail::Unreachable)) => unreachable.push(id),
            Err((id, ReadFail::Broken)) => broken.push(id),
            Err((id, ReadFail::WrongKey)) => wrong_key.push(id),
        }
    }
    if candidates.is_empty() {
        return Err(if !wrong_key.is_empty() {
            Error::Crypto
        } else if !broken.is_empty() {
            Error::Format("no replica holds a valid container".into())
        } else {
            Error::Io("no replica reachable".into())
        });
    }
    // Replicas the recovery key does not open hold another container.
    broken.extend(wrong_key);

    let mut refused_by_mark = false;
    candidates.retain(|c| match passes_mark(c, known_high_water) {
        Ok(()) => true,
        Err(is_rollback) => {
            refused_by_mark = true;
            if is_rollback {
                rollbacks.push(c.id.clone());
            } else {
                diverged.push(c.id.clone());
            }
            false
        }
    });
    candidates.sort_by_key(|c| std::cmp::Reverse(c.manifest.seq));

    let mut chosen: Option<usize> = None;
    let mut start = 0;
    while start < candidates.len() && chosen.is_none() {
        let seq = candidates[start].manifest.seq;
        let end = start
            + candidates[start..]
                .iter()
                .take_while(|c| c.manifest.seq == seq)
                .count();
        let group = start..end;
        start = end;
        let mut hashes: Vec<[u8; 32]> = group.clone().map(|i| candidates[i].hash).collect();
        hashes.sort();
        hashes.dedup();
        let single = hashes.len() == 1;

        // With one manifest, verify and copy in one pass (unless leftovers must go
        // first); with several, verify all.
        let mut ok: Vec<usize> = Vec::new();
        let mut failed: Vec<usize> = Vec::new();
        for i in group.clone() {
            if single && !ok.is_empty() {
                break;
            }
            let tgt = (single && clean_target).then_some(target);
            match check_chain(&candidates[i], tgt).await {
                Ok(()) => ok.push(i),
                Err(ChainFail::Target(e)) => return Err(e),
                Err(ChainFail::Unreachable) => {
                    unreachable.push(candidates[i].id.clone());
                    failed.push(i);
                }
                Err(ChainFail::Broken) => {
                    broken.push(candidates[i].id.clone());
                    failed.push(i);
                }
            }
        }
        let Some(&first) = ok.first() else {
            continue; // nothing at this seq verifies: try the next lower one
        };
        if !single {
            // A fork at the top: even if only one side verifies, the other may be
            // the newer history (e.g. a temporarily broken replica). The user decides.
            let mut ids: Vec<String> = group.clone().map(|i| candidates[i].id.0.clone()).collect();
            ids.sort();
            ids.dedup();
            return Err(Error::DivergedReplicas(ids));
        }
        chosen = Some(first);
    }

    let Some(ci) = chosen else {
        return Err(if refused_by_mark {
            Error::Rollback
        } else {
            Error::Format("no replica holds a complete, verifiable manifest chain".into())
        });
    };
    let c = &candidates[ci];
    if !clean_target {
        if leftover_header.as_ref().is_some_and(|h| *h != c.header) {
            // A header of another container (or another wrap): not ours to wipe.
            return Err(Error::Conflict);
        }
        for name in &leftovers {
            target.delete(name).await?;
        }
        if leftover_header.is_some() {
            target.delete(HEADER).await?;
        }
        match check_chain(c, Some(target)).await {
            Ok(()) => {}
            Err(ChainFail::Target(e)) => return Err(e),
            Err(_) => return Err(Error::Io("chosen replica changed during restore".into())),
        }
    }
    let mark = HighWater {
        seq: c.manifest.seq,
        hash: c.hash,
    };
    rollbacks.extend(
        candidates
            .iter()
            .filter(|o| o.manifest.seq < mark.seq)
            .map(|o| o.id.clone()),
    );
    let mut state = ReplicaState {
        restored_floor: Some(mark),
        ..ReplicaState::default()
    };
    for o in candidates.iter().filter(|o| o.hash == mark.hash) {
        state.replicas.insert(
            o.id.clone(),
            ReplicaRecord {
                last_seen: Some(mark),
                ..ReplicaRecord::default()
            },
        );
    }
    let c = candidates.swap_remove(ci);
    Ok(RestoreReport {
        chosen: c.id,
        seq: mark.seq,
        rollbacks,
        diverged,
        unreachable,
        broken,
        crk: c.crk,
        state,
    })
}
