// SPDX-License-Identifier: MIT

//! Segment codec: immutable, encrypted, padded objects holding record changes.
//!
//! Layout: `version(1) || chunk*`, each chunk `nonce(12) || AES-256-GCM(pt <= 64 KiB)`
//! with AAD `"seg" || version || segment_id(16) || chunk_index(u32 BE) || is_last(u8)`.
//! The plaintext stream is `(u32 BE length || postcard record)*`, a zero length
//! as end marker, then zero padding up to the bucket size. Padding sits inside
//! the authenticated plaintext. The total object size is a power of two
//! (64 KiB ..= 64 MiB). Every byte read back is treated as hostile; all limits
//! are checked before allocating.

use crate::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::{CryptoRng, TryRngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::Zeroizing;

const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const OVERHEAD: usize = NONCE_LEN + TAG_LEN;
/// Maximum plaintext bytes per chunk.
const CHUNK_PT: usize = 64 * 1024;
/// Full chunk size on the wire.
const CHUNK_WIRE: usize = CHUNK_PT + OVERHEAD;
const MIN_BUCKET: usize = 64 * 1024;
/// Maximum segment size (reject larger).
pub const MAX_SEGMENT_SIZE: usize = 64 * 1024 * 1024;
const LEN_PREFIX: usize = 4;

/// A change to the container's key/value state.
///
/// The logical key and value are wiped when a record (or any clone of it) is
/// dropped; `key_mac` is an HMAC and not secret. The wire encoding is that of plain
/// byte vectors.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Record {
    Put {
        key_mac: [u8; 32],
        key: Zeroizing<Vec<u8>>,
        value: Zeroizing<Vec<u8>>,
    },
    Delete {
        key_mac: [u8; 32],
    },
}

impl zeroize::ZeroizeOnDrop for Record {}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Record::Put { .. } => f.write_str("Record::Put(<redacted>)"),
            Record::Delete { .. } => f.write_str("Record::Delete(<redacted>)"),
        }
    }
}

/// Generates a random 128-bit segment id.
pub fn random_segment_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    OsRng
        .unwrap_err()
        .try_fill_bytes(&mut id)
        .expect("infallible");
    id
}

/// For a total object size, returns (chunk count, plaintext capacity) if the
/// layout is valid (last chunk at least holds nonce + tag).
fn layout(total: usize) -> Option<(usize, usize)> {
    if total < 1 + OVERHEAD {
        return None;
    }
    let body = total - 1;
    let n = body.div_ceil(CHUNK_WIRE);
    let last = body - (n - 1) * CHUNK_WIRE;
    if last < OVERHEAD {
        return None;
    }
    Some((n, body - n * OVERHEAD))
}

fn is_bucket(total: usize) -> bool {
    (MIN_BUCKET..=MAX_SEGMENT_SIZE).contains(&total) && total.is_power_of_two()
}

fn aad(segment_id: &[u8; 16], index: u32, is_last: bool) -> [u8; 3 + 1 + 16 + 4 + 1] {
    let mut a = [0u8; 25];
    a[..3].copy_from_slice(b"seg");
    a[3] = VERSION;
    a[4..20].copy_from_slice(segment_id);
    a[20..24].copy_from_slice(&index.to_be_bytes());
    a[24] = is_last as u8;
    a
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).expect("32-byte key")
}

fn seal_chunk(
    c: &Aes256Gcm,
    rng: &mut impl CryptoRng,
    segment_id: &[u8; 16],
    index: u32,
    is_last: bool,
    pt: &[u8],
    out: &mut Vec<u8>,
) {
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let a = aad(segment_id, index, is_last);
    let ct = c
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: pt, aad: &a })
        .expect("AES-GCM encryption of a bounded message cannot fail");
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
}

/// Builds a segment from records.
pub struct SegmentWriter {
    key: Zeroizing<[u8; 32]>,
    segment_id: [u8; 16],
    stream: Zeroizing<Vec<u8>>,
}

impl SegmentWriter {
    pub fn new(enc_key: &[u8; 32], segment_id: [u8; 16]) -> Self {
        Self {
            key: Zeroizing::new(*enc_key),
            segment_id,
            stream: Zeroizing::new(Vec::new()),
        }
    }

    fn max_stream() -> usize {
        // Reserve the 4-byte end marker.
        layout(MAX_SEGMENT_SIZE).expect("valid layout").1 - LEN_PREFIX
    }

    /// Appends a record; fails if the segment would exceed the maximum size.
    pub fn push(&mut self, record: Record) -> Result<()> {
        let enc = Zeroizing::new(
            postcard::to_allocvec(&record).map_err(|e| Error::Format(e.to_string()))?,
        );
        if self.stream.len() + LEN_PREFIX + enc.len() > Self::max_stream() {
            return Err(Error::Unsupported("segment exceeds maximum size".into()));
        }
        self.stream
            .extend_from_slice(&(enc.len() as u32).to_be_bytes());
        self.stream.extend_from_slice(&enc);
        Ok(())
    }

    /// Encrypts and pads to the next bucket size.
    pub fn finish(self) -> Vec<u8> {
        self.finish_with(&mut OsRng.unwrap_err())
    }

    /// As `finish` with a caller-supplied RNG; crate-private so production cannot inject a weak RNG.
    pub(crate) fn finish_with(self, rng: &mut impl CryptoRng) -> Vec<u8> {
        let need = self.stream.len() + LEN_PREFIX;
        let mut total = MIN_BUCKET;
        let (n, cap) = loop {
            if let Some((n, cap)) = layout(total)
                && cap >= need
            {
                break (n, cap);
            }
            total *= 2;
        };
        let mut pt = Zeroizing::new(Vec::with_capacity(cap));
        pt.extend_from_slice(&self.stream);
        pt.extend_from_slice(&0u32.to_be_bytes());
        pt.resize(cap, 0);

        let c = cipher(&self.key);
        let mut out = Vec::with_capacity(total);
        out.push(VERSION);
        let mut off = 0;
        for i in 0..n {
            let take = if i + 1 == n { cap - off } else { CHUNK_PT };
            seal_chunk(
                &c,
                rng,
                &self.segment_id,
                i as u32,
                i + 1 == n,
                &pt[off..off + take],
                &mut out,
            );
            off += take;
        }
        debug_assert_eq!(out.len(), total);
        out
    }
}

/// Reads a segment. The whole object is authenticated in `open`.
pub struct SegmentReader {
    stream: Zeroizing<Vec<u8>>,
    pos: usize,
    done: bool,
}

impl SegmentReader {
    pub fn open(
        enc_key: &[u8; 32],
        segment_id: [u8; 16],
        bytes: &[u8],
    ) -> Result<impl Iterator<Item = Result<Record>>> {
        Self::open_inner(enc_key, segment_id, bytes)
    }

    fn open_inner(enc_key: &[u8; 32], segment_id: [u8; 16], bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(Error::Format("empty segment".into()));
        }
        if bytes[0] != VERSION {
            return Err(Error::Unsupported(format!("segment version {}", bytes[0])));
        }
        if !is_bucket(bytes.len()) {
            return Err(Error::Format("invalid segment size".into()));
        }
        let (n, cap) = layout(bytes.len()).ok_or_else(|| Error::Format("bad layout".into()))?;
        let c = cipher(enc_key);
        let mut stream = Zeroizing::new(Vec::with_capacity(cap));
        for (i, chunk) in bytes[1..].chunks(CHUNK_WIRE).enumerate() {
            let a = aad(&segment_id, i as u32, i + 1 == n);
            let pt = Zeroizing::new(
                c.decrypt(
                    Nonce::from_slice(&chunk[..NONCE_LEN]),
                    Payload {
                        msg: &chunk[NONCE_LEN..],
                        aad: &a,
                    },
                )
                .map_err(|_| Error::Crypto)?,
            );
            stream.extend_from_slice(&pt);
        }
        // Validate framing: records, end marker, zero padding.
        let mut pos = 0;
        loop {
            let len = read_len(&stream, pos)?;
            pos += LEN_PREFIX;
            if len == 0 {
                break;
            }
            if len > stream.len() - pos {
                return Err(Error::Format("record length beyond segment".into()));
            }
            pos += len;
        }
        if stream[pos..].iter().any(|&b| b != 0) {
            return Err(Error::Format("non-zero padding".into()));
        }
        Ok(Self {
            stream,
            pos: 0,
            done: false,
        })
    }
}

fn read_len(s: &[u8], pos: usize) -> Result<usize> {
    let b = s
        .get(pos..pos + LEN_PREFIX)
        .ok_or_else(|| Error::Format("missing end marker".into()))?;
    Ok(u32::from_be_bytes(b.try_into().expect("4 bytes")) as usize)
}

impl Iterator for SegmentReader {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let len = match read_len(&self.stream, self.pos) {
            Ok(l) => l,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        if len == 0 {
            self.done = true;
            return None;
        }
        let start = self.pos + LEN_PREFIX;
        self.pos = start + len; // bounds validated in open
        match postcard::take_from_bytes::<Record>(&self.stream[start..self.pos]) {
            Ok((r, [])) => Some(Ok(r)),
            Ok(_) => {
                self.done = true;
                Some(Err(Error::Format("trailing bytes in record".into())))
            }
            Err(e) => {
                self.done = true;
                Some(Err(Error::Format(format!("bad record: {e}"))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    const KEY: [u8; 32] = [7u8; 32];
    const ID: [u8; 16] = [9u8; 16];

    fn rng() -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(1)
    }

    fn put(i: u32, vlen: usize) -> Record {
        Record::Put {
            key_mac: [i as u8; 32],
            key: Zeroizing::new(format!("key{i}").into_bytes()),
            value: Zeroizing::new(vec![i as u8; vlen]),
        }
    }

    fn build(records: &[Record]) -> Vec<u8> {
        let mut w = SegmentWriter::new(&KEY, ID);
        for r in records {
            w.push(r.clone()).unwrap();
        }
        w.finish_with(&mut rng())
    }

    fn read(bytes: &[u8]) -> Result<Vec<Record>> {
        SegmentReader::open(&KEY, ID, bytes)?.collect()
    }

    #[test]
    fn record_contents_zeroize_on_drop() {
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<Record>();
        // The wire encoding is unchanged by the zeroizing wrappers.
        let r = put(5, 3);
        let enc = postcard::to_allocvec(&r).unwrap();
        let plain =
            postcard::to_allocvec(&(0u32, [5u8; 32], b"key5".to_vec(), vec![5u8; 3])).unwrap();
        assert_eq!(&enc[1..], &plain[1..]);
        assert_eq!(enc[0], 0);
    }

    #[test]
    fn segment_roundtrip() {
        let recs = vec![
            put(1, 10),
            Record::Delete { key_mac: [2; 32] },
            put(3, 0),
            put(4, 200_000),
        ];
        let bytes = build(&recs);
        assert_eq!(read(&bytes).unwrap(), recs);
        // empty segment
        assert_eq!(read(&build(&[])).unwrap(), vec![]);
        // via OS rng path
        let mut w = SegmentWriter::new(&KEY, ID);
        w.push(put(1, 5)).unwrap();
        assert_eq!(read(&w.finish()).unwrap(), vec![put(1, 5)]);
    }

    #[test]
    fn tampered_objects_rejected() {
        let bytes = build(&[put(1, 100_000), put(2, 100_000)]);
        assert_eq!(bytes.len(), 256 * 1024);
        // bit flips across the object (version byte, nonce, ct, tag)
        for pos in [0, 1, 20, 70_000, bytes.len() - 1] {
            let mut b = bytes.clone();
            b[pos] ^= 1;
            assert!(read(&b).is_err(), "flip at {pos}");
        }
        // truncation / extension
        assert!(read(&bytes[..bytes.len() - 1]).is_err());
        assert!(read(&bytes[..1 + CHUNK_WIRE]).is_err());
        let mut ext = bytes.clone();
        ext.push(0);
        assert!(read(&ext).is_err());
        assert!(read(&[]).is_err());
        // chunk reorder
        let mut b = bytes.clone();
        let (c0, c1) = (1..1 + CHUNK_WIRE, 1 + CHUNK_WIRE..1 + 2 * CHUNK_WIRE);
        let first = b[c0.clone()].to_vec();
        let second = b[c1.clone()].to_vec();
        b[c0].copy_from_slice(&second);
        b[c1].copy_from_slice(&first);
        assert!(read(&b).is_err());
        // chunk from another segment (same key, same index)
        let other = {
            let mut w = SegmentWriter::new(&KEY, [1u8; 16]);
            w.push(put(1, 100_000)).unwrap();
            w.push(put(2, 100_000)).unwrap();
            w.finish_with(&mut rng())
        };
        let mut b = bytes.clone();
        b[1..1 + CHUNK_WIRE].copy_from_slice(&other[1..1 + CHUNK_WIRE]);
        assert!(read(&b).is_err());
        // wrong id / wrong key / unknown version
        assert!(SegmentReader::open(&KEY, [0; 16], &bytes).is_err());
        assert!(SegmentReader::open(&[0; 32], ID, &bytes).is_err());
        let mut b = bytes.clone();
        b[0] = 2;
        assert!(matches!(
            SegmentReader::open(&KEY, ID, &b),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn padding_hides_exact_size() {
        let a = build(&[put(1, 10)]);
        let b = build(&[put(1, 20_000)]);
        assert_eq!(a.len(), 64 * 1024);
        assert_eq!(a.len(), b.len());
        let big = build(&[put(1, 70_000)]);
        assert_eq!(big.len(), 128 * 1024);
        assert!(big.len().is_power_of_two());
        // all sizes are valid buckets with a valid layout
        for t in (16..=26).map(|p| 1usize << p) {
            assert!(layout(t).is_some(), "layout {t}");
        }
    }

    #[test]
    fn last_chunk_flag_prevents_truncation() {
        // Craft a valid-size object whose final chunk was sealed with is_last=0
        // (what an attacker splicing a prefix of a longer stream would produce).
        let c = cipher(&KEY);
        let (n, cap) = layout(128 * 1024).unwrap();
        let mut pt = vec![0u8; cap];
        pt[..4].copy_from_slice(&0u32.to_be_bytes());
        let mut out = vec![VERSION];
        let mut off = 0;
        for i in 0..n {
            let take = if i + 1 == n { cap - off } else { CHUNK_PT };
            seal_chunk(
                &c,
                &mut rng(),
                &ID,
                i as u32,
                false,
                &pt[off..off + take],
                &mut out,
            );
            off += take;
        }
        assert!(read(&out).is_err());
        // Control: same with correct flag is accepted.
        let mut ok = vec![VERSION];
        off = 0;
        for i in 0..n {
            let take = if i + 1 == n { cap - off } else { CHUNK_PT };
            seal_chunk(
                &c,
                &mut rng(),
                &ID,
                i as u32,
                i + 1 == n,
                &pt[off..off + take],
                &mut ok,
            );
            off += take;
        }
        assert_eq!(read(&ok).unwrap(), vec![]);
    }

    fn forged(stream: &[u8]) -> Vec<u8> {
        let c = cipher(&KEY);
        let (_, cap) = layout(MIN_BUCKET).unwrap();
        let mut pt = stream.to_vec();
        pt.resize(cap, 0);
        let mut out = vec![VERSION];
        seal_chunk(&c, &mut rng(), &ID, 0, true, &pt, &mut out);
        out
    }

    #[test]
    fn authenticated_but_malformed_streams_rejected() {
        // record length beyond remaining bytes
        assert!(read(&forged(&u32::MAX.to_be_bytes())).is_err());
        // non-zero padding after the end marker
        let mut s = vec![0u8; 4];
        s.push(1);
        assert!(read(&forged(&s)).is_err());
        // garbage record body
        let mut s = 3u32.to_be_bytes().to_vec();
        s.extend_from_slice(&[0xff, 0xff, 0xff]);
        s.extend_from_slice(&0u32.to_be_bytes());
        assert!(read(&forged(&s)).is_err());
        // no end marker: stream filled with a record up to the very end
        let (_, cap) = layout(MIN_BUCKET).unwrap();
        let mut s = ((cap - 4) as u32).to_be_bytes().to_vec();
        s.resize(cap, 1);
        assert!(read(&forged(&s)).is_err());
    }

    fn framed(body: &[u8]) -> Vec<u8> {
        let mut s = (body.len() as u32).to_be_bytes().to_vec();
        s.extend_from_slice(body);
        s.extend_from_slice(&0u32.to_be_bytes());
        forged(&s)
    }

    #[test]
    fn hostile_record_bodies_rejected() {
        // Put with a huge Vec length prefix (varint ~ 2^35) and no data behind it.
        let mut body = vec![0u8]; // Put variant
        body.extend_from_slice(&[0u8; 32]);
        body.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert!(read(&framed(&body)).is_err());
        // Valid record followed by trailing bytes inside the same frame.
        let mut body = postcard::to_allocvec(&Record::Delete { key_mac: [5; 32] }).unwrap();
        assert_eq!(read(&framed(&body)).unwrap().len(), 1);
        body.push(0);
        assert!(read(&framed(&body)).is_err());
    }

    #[test]
    fn size_limits() {
        // oversize input is rejected before any allocation
        let big = vec![VERSION; MAX_SEGMENT_SIZE * 2];
        assert!(read(&big).is_err());
        // non-power-of-two size
        assert!(read(&vec![VERSION; 100_000]).is_err());
        // writer rejects records exceeding the maximum
        let mut w = SegmentWriter::new(&KEY, ID);
        let r = (0..70).try_for_each(|_| w.push(put(1, 1_000_000)));
        assert!(r.is_err());
    }
}
