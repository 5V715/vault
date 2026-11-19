// SPDX-License-Identifier: MIT

//! Sealed manifest: the authenticated list of live segments.
//!
//! Layout of a sealed manifest: `version(1) || nonce(12) || GCM(postcard(Manifest) || zero padding)`
//! with AAD `b"manifest" || version`. The total object size is always a padding bucket
//! (4 KiB, then powers of two up to 16 MiB). Everything read back is attacker-controlled,
//! so [`open`] checks size, authenticity, structure and padding before returning.
//!
//! Two authenticators: the AEAD under the `manifest` subkey (which the vault keeps
//! without user authentication for background replication), and inside the sealed body
//! [`Manifest::auth_tag`] = HMAC-SHA256(`manifest_auth` subkey, canonical bytes of
//! `{seq, prev, segments, recovery_wrapped_crk}`). The `manifest_auth` subkey is derived
//! from the CRK and never stored with the manifest key, so a holder of the manifest key
//! alone can read `seq`/segment ids and copy sealed manifests but cannot forge one.
//! [`open`] checks only the AEAD (enough for replication); readers that hold the CRK
//! (engine open, restore) also call [`Manifest::verify_auth`] (or use [`open_verified`]).

use std::collections::HashSet;

use crate::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use rand::{CryptoRng, TryRngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

const VERSION: u8 = 1;
const AUTH_DOMAIN: &[u8] = b"vault/v1/manifest-auth-tag";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const OVERHEAD: usize = 1 + NONCE_LEN + TAG_LEN;
const MIN_BUCKET: usize = 4 * 1024;
const MAX_BUCKET: usize = 16 * 1024 * 1024;
/// Maximum number of live segment ids in one manifest.
pub const MAX_SEGMENTS: usize = 1_000_000;
/// Maximum length of `recovery_wrapped_crk` (a real wrapped CRK is 61 bytes).
pub const MAX_WRAPPED_CRK: usize = 1024;

/// The manifest: which segments are live, chained to its predecessor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Strictly increasing sequence number (genesis is 0).
    pub seq: u64,
    /// SHA-256 of the previous sealed manifest (`[0; 32]` for genesis).
    pub prev: [u8; 32],
    /// Ids of the live segments.
    pub segments: Vec<[u8; 16]>,
    /// The CRK wrapped under the recovery key (see `keys::wrap_crk`).
    pub recovery_wrapped_crk: Vec<u8>,
    /// HMAC-SHA256 under the `manifest_auth` subkey over the fields above (see
    /// [`Manifest::compute_auth_tag`]); all zero until [`Manifest::authenticate`].
    pub auth_tag: [u8; 32],
}

impl Manifest {
    /// The first manifest of a container: seq 0, no predecessor, no segments. Not
    /// yet authenticated: call [`Manifest::authenticate`] before sealing.
    pub fn genesis(recovery_wrapped_crk: Vec<u8>) -> Self {
        Self {
            seq: 0,
            prev: [0u8; 32],
            segments: Vec::new(),
            recovery_wrapped_crk,
            auth_tag: [0u8; 32],
        }
    }

    /// Canonical bytes covered by the tag: domain label || version ||
    /// postcard((seq, prev, segments, recovery_wrapped_crk)).
    fn auth_input(&self) -> Result<Vec<u8>> {
        let body = postcard::to_allocvec(&(
            self.seq,
            &self.prev,
            &self.segments,
            &self.recovery_wrapped_crk,
        ))
        .map_err(|_| Error::Format("manifest encoding failed".into()))?;
        let mut out = Vec::with_capacity(AUTH_DOMAIN.len() + 1 + body.len());
        out.extend_from_slice(AUTH_DOMAIN);
        out.push(VERSION);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// HMAC-SHA256(`auth_key`, canonical bytes of `{seq, prev, segments,
    /// recovery_wrapped_crk}`).
    pub fn compute_auth_tag(&self, auth_key: &[u8; 32]) -> Result<[u8; 32]> {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(auth_key).expect("HMAC accepts any key size");
        mac.update(&self.auth_input()?);
        Ok(mac.finalize().into_bytes().into())
    }

    /// Sets [`Manifest::auth_tag`] for the current contents.
    pub fn authenticate(&mut self, auth_key: &[u8; 32]) -> Result<()> {
        self.auth_tag = self.compute_auth_tag(auth_key)?;
        Ok(())
    }

    /// Checks [`Manifest::auth_tag`] in constant time; [`Error::Crypto`] if it does
    /// not match (e.g. a manifest sealed by a holder of only the manifest key).
    pub fn verify_auth(&self, auth_key: &[u8; 32]) -> Result<()> {
        let expect = self.compute_auth_tag(auth_key)?;
        if bool::from(expect.ct_eq(&self.auth_tag)) {
            Ok(())
        } else {
            Err(Error::Crypto)
        }
    }
}

fn check_unique(segments: &[[u8; 16]]) -> Result<()> {
    let mut seen = HashSet::with_capacity(segments.len());
    if segments.iter().all(|id| seen.insert(id)) {
        Ok(())
    } else {
        Err(Error::Format("duplicate segment id in manifest".into()))
    }
}

fn aad() -> [u8; 9] {
    let mut a = [0u8; 9];
    a[..8].copy_from_slice(b"manifest");
    a[8] = VERSION;
    a
}

fn is_bucket(total: usize) -> bool {
    (MIN_BUCKET..=MAX_BUCKET).contains(&total) && total.is_power_of_two()
}

/// Seals a manifest with OsRng nonces, padded to the next bucket.
pub fn seal(m: &Manifest, manifest_key: &[u8; 32]) -> Result<Vec<u8>> {
    seal_with(m, manifest_key, &mut OsRng.unwrap_err())
}

/// As [`seal`] with a caller-supplied RNG; crate-private so production cannot inject a weak RNG.
pub(crate) fn seal_with(
    m: &Manifest,
    manifest_key: &[u8; 32],
    rng: &mut impl CryptoRng,
) -> Result<Vec<u8>> {
    if m.segments.len() > MAX_SEGMENTS {
        return Err(Error::Format("too many segments in manifest".into()));
    }
    if m.recovery_wrapped_crk.len() > MAX_WRAPPED_CRK {
        return Err(Error::Format("wrapped CRK too large".into()));
    }
    check_unique(&m.segments)?;
    let body = Zeroizing::new(
        postcard::to_allocvec(m).map_err(|_| Error::Format("manifest encoding failed".into()))?,
    );
    seal_body(&body, manifest_key, rng)
}

fn seal_body(body: &[u8], key: &[u8; 32], rng: &mut impl CryptoRng) -> Result<Vec<u8>> {
    let mut total = MIN_BUCKET;
    while total - OVERHEAD < body.len() {
        total *= 2;
        if total > MAX_BUCKET {
            return Err(Error::Format("manifest exceeds maximum size".into()));
        }
    }
    let mut pt = Zeroizing::new(Vec::with_capacity(total - OVERHEAD));
    pt.extend_from_slice(body);
    pt.resize(total - OVERHEAD, 0);

    let mut nonce = [0u8; NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let a = aad();
    let ct = Aes256Gcm::new_from_slice(key)
        .expect("32-byte key")
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &pt, aad: &a })
        .expect("AES-GCM encryption of a bounded message cannot fail");
    let mut out = Vec::with_capacity(total);
    out.push(VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

/// Authenticates (AEAD only) and decodes a sealed manifest. This proves it was sealed
/// with the manifest key, not that its [`Manifest::auth_tag`] is valid; readers that
/// hold the `manifest_auth` subkey use [`open_verified`].
///
/// Authenticity only proves the manifest was sealed with the key; it does not
/// prove freshness. Anti-rollback is the caller's job: keep a high-water mark
/// of the highest accepted `seq` and its hash, reject anything older or
/// forked, and publish a new manifest with compare-and-swap.
pub fn open(bytes: &[u8], manifest_key: &[u8; 32]) -> Result<Manifest> {
    if !is_bucket(bytes.len()) {
        return Err(Error::Format("manifest size is not a valid bucket".into()));
    }
    if bytes[0] != VERSION {
        return Err(Error::Unsupported(format!("manifest version {}", bytes[0])));
    }
    let a = aad();
    let pt = Zeroizing::new(
        Aes256Gcm::new_from_slice(manifest_key)
            .expect("32-byte key")
            .decrypt(
                Nonce::from_slice(&bytes[1..1 + NONCE_LEN]),
                Payload {
                    msg: &bytes[1 + NONCE_LEN..],
                    aad: &a,
                },
            )
            .map_err(|_| Error::Crypto)?,
    );
    let (m, rest) = postcard::take_from_bytes::<Manifest>(&pt)
        .map_err(|_| Error::Format("malformed manifest".into()))?;
    if m.segments.len() > MAX_SEGMENTS {
        return Err(Error::Format("too many segments in manifest".into()));
    }
    if m.recovery_wrapped_crk.len() > MAX_WRAPPED_CRK {
        return Err(Error::Format("wrapped CRK too large".into()));
    }
    // Genesis (seq 0) has no predecessor; every later manifest must chain to one.
    if (m.seq == 0) != (m.prev == [0u8; 32]) {
        return Err(Error::Format("inconsistent seq/prev chain link".into()));
    }
    check_unique(&m.segments)?;
    if rest.iter().any(|&b| b != 0) {
        return Err(Error::Format("non-zero manifest padding".into()));
    }
    Ok(m)
}

/// [`open`] plus [`Manifest::verify_auth`] under the `manifest_auth` subkey.
pub fn open_verified(
    bytes: &[u8],
    manifest_key: &[u8; 32],
    auth_key: &[u8; 32],
) -> Result<Manifest> {
    let m = open(bytes, manifest_key)?;
    m.verify_auth(auth_key)?;
    Ok(m)
}

/// SHA-256 over the whole sealed manifest bytes.
pub fn hash(sealed: &[u8]) -> [u8; 32] {
    Sha256::digest(sealed).into()
}

/// Checks that `next` validly follows the sealed manifest `prev_sealed`:
/// `next.seq == prev.seq + 1` and `next.prev == hash(prev_sealed)`.
///
/// The previous manifest must be opened to learn its `seq`, hence the key.
///
/// This checks one link only. The caller must also enforce anti-rollback:
/// persist a high-water mark of the latest accepted `seq` and hash, refuse
/// older or forked manifests, and write the successor with compare-and-swap.
pub fn verify_successor(
    prev_sealed: &[u8],
    manifest_key: &[u8; 32],
    next: &Manifest,
) -> Result<()> {
    let prev = open(prev_sealed, manifest_key)?;
    // A manifest at u64::MAX has no successor; treat it as a conflict, not a wrap.
    let expected_seq = prev.seq.checked_add(1).ok_or(Error::Conflict)?;
    let seq_ok = next.seq == expected_seq;
    let prev_ok = bool::from(next.prev.ct_eq(&hash(prev_sealed)));
    if seq_ok && prev_ok {
        Ok(())
    } else {
        Err(Error::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    const KEY: [u8; 32] = [7u8; 32];

    fn rng(seed: u64) -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(seed)
    }

    fn sample(n: usize, seq: u64) -> Manifest {
        Manifest {
            seq,
            prev: [3u8; 32],
            segments: (0..n)
                .map(|i| {
                    let mut id = [0u8; 16];
                    id[..8].copy_from_slice(&(i as u64).to_le_bytes());
                    id
                })
                .collect(),
            recovery_wrapped_crk: vec![9u8; 61],
            auth_tag: [5u8; 32],
        }
    }

    #[test]
    fn manifest_roundtrip_padded() {
        for (n, expect) in [
            (0, 4096),
            (10, 4096),
            (200, 4096),
            (300, 8192),
            (1000, 16384),
        ] {
            let m = sample(n, 5);
            let sealed = seal_with(&m, &KEY, &mut rng(1)).unwrap();
            assert_eq!(sealed.len(), expect, "n={n}");
            assert_eq!(sealed[0], 1);
            assert_eq!(open(&sealed, &KEY).unwrap(), m);
        }
        // public seal works and uses fresh nonces
        let m = sample(3, 1);
        let a = seal(&m, &KEY).unwrap();
        let b = seal(&m, &KEY).unwrap();
        assert_ne!(a[1..13], b[1..13]);
        assert_eq!(open(&a, &KEY).unwrap(), m);
        // genesis
        let g = Manifest::genesis(vec![1, 2, 3]);
        assert_eq!((g.seq, g.prev, g.segments.len()), (0, [0u8; 32], 0));
        assert_eq!(open(&seal(&g, &KEY).unwrap(), &KEY).unwrap(), g);
    }

    #[test]
    fn size_limits() {
        // Largest allowed: 1_000_000 ids fits in 16 MiB.
        let big = sample(MAX_SEGMENTS, 1);
        let sealed = seal_with(&big, &KEY, &mut rng(2)).unwrap();
        assert_eq!(sealed.len(), MAX_BUCKET);
        assert_eq!(open(&sealed, &KEY).unwrap().segments.len(), MAX_SEGMENTS);
        // One more is rejected on seal.
        let too_many = sample(MAX_SEGMENTS + 1, 1);
        assert!(matches!(
            seal_with(&too_many, &KEY, &mut rng(3)),
            Err(Error::Format(_))
        ));
        // Oversized body is rejected.
        assert!(seal_body(&vec![1u8; MAX_BUCKET], &KEY, &mut rng(4)).is_err());
        // Oversized object is rejected on open without decrypting.
        assert!(matches!(
            open(&vec![1u8; MAX_BUCKET * 2], &KEY),
            Err(Error::Format(_))
        ));
        // Authentic manifest with too many ids is rejected on open.
        let body = postcard::to_allocvec(&sample(MAX_SEGMENTS + 1, 1)).unwrap();
        let sealed = seal_body(&body, &KEY, &mut rng(5)).unwrap();
        assert!(matches!(open(&sealed, &KEY), Err(Error::Format(_))));
    }

    #[test]
    fn tampered_objects_rejected() {
        let m = sample(5, 2);
        let sealed = seal_with(&m, &KEY, &mut rng(6)).unwrap();
        // every single-byte flip is rejected
        for i in 0..sealed.len() {
            let mut t = sealed.clone();
            t[i] ^= 0x01;
            assert!(open(&t, &KEY).is_err(), "flip at {i} accepted");
        }
        // wrong version is Unsupported
        let mut v = sealed.clone();
        v[0] = 2;
        assert!(matches!(open(&v, &KEY), Err(Error::Unsupported(_))));
        // wrong key
        assert!(matches!(open(&sealed, &[8u8; 32]), Err(Error::Crypto)));
        // truncation / extension / empty
        assert!(open(&sealed[..sealed.len() - 1], &KEY).is_err());
        let mut ext = sealed.clone();
        ext.push(0);
        assert!(open(&ext, &KEY).is_err());
        assert!(open(&[], &KEY).is_err());
        // non-bucket size, even with valid-looking content
        assert!(open(&sealed[..4095], &KEY).is_err());
        // another object kind's AAD (segment-style label) does not authenticate
        let other = Aes256Gcm::new_from_slice(&KEY)
            .unwrap()
            .encrypt(
                Nonce::from_slice(&sealed[1..13]),
                Payload {
                    msg: &vec![0u8; 4096 - OVERHEAD],
                    aad: b"seg\x01",
                },
            )
            .unwrap();
        let mut forged = vec![1u8];
        forged.extend_from_slice(&sealed[1..13]);
        forged.extend_from_slice(&other);
        assert_eq!(forged.len(), 4096);
        assert!(matches!(open(&forged, &KEY), Err(Error::Crypto)));
    }

    #[test]
    fn nonzero_padding_and_garbage_rejected() {
        let m = sample(2, 1);
        let mut body = postcard::to_allocvec(&m).unwrap();
        body.push(0);
        body.push(1); // non-zero byte in padding
        let sealed = seal_body(&body, &KEY, &mut rng(7)).unwrap();
        assert!(matches!(open(&sealed, &KEY), Err(Error::Format(_))));
        // authentic but undecodable plaintext
        let sealed = seal_body(&[0xFF; 40], &KEY, &mut rng(8)).unwrap();
        assert!(matches!(open(&sealed, &KEY), Err(Error::Format(_))));
        // an all-zero plaintext is a valid (empty) genesis encoding, not an error
        let sealed = seal_body(&[], &KEY, &mut rng(9)).unwrap();
        assert_eq!(open(&sealed, &KEY).unwrap(), Manifest::genesis(Vec::new()));
    }

    #[test]
    fn wrapped_crk_capped() {
        let ok = Manifest::genesis(vec![1; MAX_WRAPPED_CRK]);
        let sealed = seal_with(&ok, &KEY, &mut rng(13)).unwrap();
        assert_eq!(open(&sealed, &KEY).unwrap(), ok);
        let big = Manifest::genesis(vec![1; MAX_WRAPPED_CRK + 1]);
        assert!(matches!(
            seal_with(&big, &KEY, &mut rng(14)),
            Err(Error::Format(_))
        ));
        let body = postcard::to_allocvec(&big).unwrap();
        let sealed = seal_body(&body, &KEY, &mut rng(15)).unwrap();
        assert!(matches!(open(&sealed, &KEY), Err(Error::Format(_))));
    }

    #[test]
    fn auth_tag_binds_every_field() {
        const AUTH: [u8; 32] = [11u8; 32];
        let mut m = sample(3, 4);
        m.authenticate(&AUTH).unwrap();
        m.verify_auth(&AUTH).unwrap();
        let sealed = seal_with(&m, &KEY, &mut rng(18)).unwrap();
        assert_eq!(open_verified(&sealed, &KEY, &AUTH).unwrap(), m);
        assert!(matches!(m.verify_auth(&[12u8; 32]), Err(Error::Crypto)));
        let variants = [
            Manifest {
                seq: 5,
                ..m.clone()
            },
            Manifest {
                prev: [4u8; 32],
                ..m.clone()
            },
            Manifest {
                segments: m.segments[..2].to_vec(),
                ..m.clone()
            },
            Manifest {
                segments: vec![m.segments[1], m.segments[0], m.segments[2]],
                ..m.clone()
            },
            Manifest {
                recovery_wrapped_crk: vec![8u8; 61],
                ..m.clone()
            },
            Manifest {
                auth_tag: [0u8; 32],
                ..m.clone()
            },
        ];
        for v in variants {
            assert!(matches!(v.verify_auth(&AUTH), Err(Error::Crypto)));
            // AEAD-valid under the manifest key alone, but not authentic.
            let sealed = seal_with(&v, &KEY, &mut rng(19)).unwrap();
            assert!(open(&sealed, &KEY).is_ok());
            assert!(matches!(
                open_verified(&sealed, &KEY, &AUTH),
                Err(Error::Crypto)
            ));
        }
    }

    #[test]
    fn auth_input_uses_v1_label() {
        let m = sample(1, 1);
        let input = m.auth_input().unwrap();
        assert!(input.starts_with(b"vault/v1/manifest-auth-tag\x01"));
    }

    #[test]
    fn duplicate_segment_ids_rejected() {
        let mut m = sample(3, 4);
        m.segments.push(m.segments[1]);
        let body = postcard::to_allocvec(&m).unwrap();
        let sealed = seal_body(&body, &KEY, &mut rng(17)).unwrap();
        assert!(matches!(open(&sealed, &KEY), Err(Error::Format(_))));
        assert!(matches!(seal(&m, &KEY), Err(Error::Format(_))));
    }

    #[test]
    fn open_rejects_inconsistent_chain_link() {
        let genesis_with_prev = Manifest {
            prev: [1; 32],
            ..Manifest::genesis(vec![1])
        };
        let later_without_prev = Manifest {
            seq: 3,
            ..Manifest::genesis(vec![1])
        };
        for m in [genesis_with_prev, later_without_prev] {
            let body = postcard::to_allocvec(&m).unwrap();
            let sealed = seal_body(&body, &KEY, &mut rng(16)).unwrap();
            assert!(matches!(open(&sealed, &KEY), Err(Error::Format(_))));
        }
    }

    #[test]
    fn successor_requires_seq_plus_and_prev_hash() {
        let g = Manifest::genesis(vec![1; 61]);
        let g_sealed = seal_with(&g, &KEY, &mut rng(10)).unwrap();
        let h = hash(&g_sealed);
        assert_eq!(h, <[u8; 32]>::from(Sha256::digest(&g_sealed)));

        let good = Manifest {
            seq: 1,
            prev: h,
            ..g.clone()
        };
        verify_successor(&g_sealed, &KEY, &good).unwrap();

        let bad_seq = |seq| Manifest {
            seq,
            ..good.clone()
        };
        for s in [0, 2, u64::MAX] {
            assert!(matches!(
                verify_successor(&g_sealed, &KEY, &bad_seq(s)),
                Err(Error::Conflict)
            ));
        }
        // wrong prev hash (e.g. rollback to a different predecessor)
        let mut wrong = good.clone();
        wrong.prev[0] ^= 1;
        assert!(verify_successor(&g_sealed, &KEY, &wrong).is_err());
        // hash covers sealed bytes: re-sealing the same manifest changes the hash
        let g_sealed2 = seal_with(&g, &KEY, &mut rng(11)).unwrap();
        assert_ne!(hash(&g_sealed2), h);
        assert!(verify_successor(&g_sealed2, &KEY, &good).is_err());
        // seq overflow on prev does not panic
        let max = Manifest {
            seq: u64::MAX,
            prev: [4u8; 32],
            ..g.clone()
        };
        let max_sealed = seal_with(&max, &KEY, &mut rng(12)).unwrap();
        let next = Manifest {
            seq: 0,
            prev: hash(&max_sealed),
            ..g.clone()
        };
        assert!(verify_successor(&max_sealed, &KEY, &next).is_err());
        // unauthentic prev is rejected
        assert!(verify_successor(&g_sealed, &[1u8; 32], &good).is_err());
    }
}
