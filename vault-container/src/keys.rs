// SPDX-License-Identifier: MIT

//! Key hierarchy: container root key, HKDF subkeys, recovery key and CRK wrapping.
//!
//! Labels and derivations are defined in `docs/vault/README.md` ("Format v1"); the
//! known answers in the tests come from `tools/kat/kat.py`.
//!
//! All secret material lives in [`Zeroizing`] buffers and no type here prints
//! key bytes through `Debug`.

use crate::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use data_encoding::BASE32_NOPAD;
use hkdf::Hkdf;
use rand::{CryptoRng, TryRngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use std::fmt;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

const INFO_ENC: &[u8] = b"vault/v1/enc";
const INFO_MAC: &[u8] = b"vault/v1/mac";
const INFO_MANIFEST: &[u8] = b"vault/v1/manifest";
const INFO_MANIFEST_AUTH: &[u8] = b"vault/v1/manifest-auth";
const INFO_REPLICA_ID: &[u8] = b"vault/v1/replica-id";
const INFO_RECOVERY: &[u8] = b"vault/v1/recovery";
const RECOVERY_CHECK_DOMAIN: &[u8] = b"vault/v1/recovery-check";
const CRK_WRAP_AAD: &[u8] = b"vault/v1/crk-wrap";

/// AAD = label || version byte.
fn wrap_aad() -> Vec<u8> {
    let mut aad = CRK_WRAP_AAD.to_vec();
    aad.push(WRAP_VERSION);
    aad
}
const QR_PREFIX: &str = "vault-recovery:v1:";
const WRAP_VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const CHECK_LEN: usize = 8;

fn hkdf_expand(ikm: &[u8], info: &[u8]) -> Zeroizing<[u8; KEY_LEN]> {
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    Hkdf::<Sha256>::new(None, ikm)
        .expand(info, &mut *out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

fn random_key(rng: &mut impl CryptoRng) -> Zeroizing<[u8; KEY_LEN]> {
    let mut k = Zeroizing::new([0u8; KEY_LEN]);
    rng.fill_bytes(&mut *k);
    k
}

fn os_rng() -> impl CryptoRng {
    OsRng.unwrap_err()
}

/// Container root key: 32 random bytes from which all subkeys are derived.
pub struct Crk(Zeroizing<[u8; KEY_LEN]>);

/// Keys derived from the [`Crk`].
pub struct Subkeys {
    pub enc: Zeroizing<[u8; KEY_LEN]>,
    pub mac: Zeroizing<[u8; KEY_LEN]>,
    /// Seals manifests (AEAD). The vault may keep it without user authentication so
    /// replication runs in the background: with it alone a holder can read `seq` and
    /// segment ids and copy sealed manifests, but not forge one (see `manifest_auth`).
    pub manifest: Zeroizing<[u8; KEY_LEN]>,
    /// HMAC key of the manifest's inner `auth_tag`; only held with the CRK, never
    /// stored alongside `manifest`.
    pub manifest_auth: Zeroizing<[u8; KEY_LEN]>,
    pub replica_id: Zeroizing<[u8; KEY_LEN]>,
}

impl Crk {
    /// Generates a CRK from the operating system RNG.
    pub fn generate() -> Self {
        Self::generate_with(&mut os_rng())
    }

    /// Generates a CRK from the given RNG (tests may pass a seeded one).
    pub fn generate_with(rng: &mut impl CryptoRng) -> Self {
        Self(random_key(rng))
    }

    /// Rebuilds a CRK from its 32 raw bytes, e.g. just unwrapped from a device
    /// keystore. The bytes are copied into zeroizing memory; the caller wipes
    /// its own buffer.
    #[cfg(feature = "raw-key-access")]
    pub fn from_bytes(bytes: &[u8; KEY_LEN]) -> Self {
        Self(Zeroizing::new(*bytes))
    }

    /// As [`Crk::from_bytes`] for a slice; anything but 32 bytes is
    /// [`Error::Crypto`].
    #[cfg(feature = "raw-key-access")]
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let mut k = Zeroizing::new([0u8; KEY_LEN]);
        if bytes.len() != KEY_LEN {
            return Err(Error::Crypto);
        }
        k.copy_from_slice(bytes);
        Ok(Self(k))
    }

    /// A zeroizing copy of the raw key, for wrapping it under a device key
    /// (the Android Keystore). Every copy is a secret: wrap it and drop it.
    #[cfg(feature = "raw-key-access")]
    pub fn to_bytes(&self) -> Zeroizing<[u8; KEY_LEN]> {
        Zeroizing::new(*self.0)
    }

    /// Derives the subkeys via HKDF-SHA256.
    pub fn subkeys(&self) -> Subkeys {
        Subkeys {
            enc: hkdf_expand(&*self.0, INFO_ENC),
            mac: hkdf_expand(&*self.0, INFO_MAC),
            manifest: hkdf_expand(&*self.0, INFO_MANIFEST),
            manifest_auth: hkdf_expand(&*self.0, INFO_MANIFEST_AUTH),
            replica_id: hkdf_expand(&*self.0, INFO_REPLICA_ID),
        }
    }
}

impl fmt::Debug for Crk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Crk(<redacted>)")
    }
}

impl fmt::Debug for Subkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Subkeys(<redacted>)")
    }
}

/// User-facing recovery key: 32 random bytes shown as 64 base32 characters
/// (32 key bytes plus an 8-byte checksum).
pub struct RecoveryKey(Zeroizing<[u8; KEY_LEN]>);

impl RecoveryKey {
    /// Generates a recovery key from the operating system RNG.
    pub fn generate() -> Self {
        Self::generate_with(&mut os_rng())
    }

    /// Generates a recovery key from the given RNG.
    pub fn generate_with(rng: &mut impl CryptoRng) -> Self {
        Self(random_key(rng))
    }

    fn checksum(key: &[u8]) -> [u8; CHECK_LEN] {
        let mut h = Sha256::new();
        h.update(RECOVERY_CHECK_DOMAIN);
        h.update(key);
        let digest = h.finalize();
        let mut out = [0u8; CHECK_LEN];
        out.copy_from_slice(&digest[..CHECK_LEN]);
        out
    }

    fn encoded(&self) -> Zeroizing<String> {
        let mut buf = Zeroizing::new(Vec::with_capacity(KEY_LEN + CHECK_LEN));
        buf.extend_from_slice(&*self.0);
        buf.extend_from_slice(&Self::checksum(&*self.0));
        Zeroizing::new(BASE32_NOPAD.encode(&buf))
    }

    /// 64 base32 characters in 16 groups of 4 separated by `-`.
    pub fn to_display(&self) -> Zeroizing<String> {
        let enc = self.encoded();
        let groups: Vec<&str> = enc
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).expect("base32 is ASCII"))
            .collect();
        Zeroizing::new(groups.join("-"))
    }

    /// Payload for a QR code: `vault-recovery:v1:<64 chars>`.
    pub fn qr_payload(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("{QR_PREFIX}{}", &**self.encoded()))
    }

    /// Parses a QR payload produced by [`RecoveryKey::qr_payload`].
    pub fn parse_qr(payload: &str) -> Result<Self> {
        let rest = payload
            .trim()
            .strip_prefix(QR_PREFIX)
            .ok_or_else(|| Error::Format("not a recovery key QR payload".into()))?;
        Self::parse(rest)
    }

    /// Parses user-typed text: case-insensitive, ignores spaces and dashes and
    /// maps `0`, `1`, `8` to `O`, `I`, `B`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut clean = Zeroizing::new(String::with_capacity(64));
        for c in text.chars() {
            match c {
                '-' => {}
                c if c.is_whitespace() => {}
                '0' => clean.push('O'),
                '1' => clean.push('I'),
                '8' => clean.push('B'),
                c if c.is_ascii_alphanumeric() => clean.push(c.to_ascii_uppercase()),
                _ => return Err(Error::Format("invalid character in recovery key".into())),
            }
        }
        if clean.len() != 64 {
            return Err(Error::Format("recovery key must be 64 characters".into()));
        }
        let bytes = Zeroizing::new(
            BASE32_NOPAD
                .decode(clean.as_bytes())
                .map_err(|_| Error::Format("invalid character in recovery key".into()))?,
        );
        if bytes.len() != KEY_LEN + CHECK_LEN {
            return Err(Error::Format("recovery key has wrong length".into()));
        }
        let (key, check) = bytes.split_at(KEY_LEN);
        if !bool::from(Self::checksum(key).ct_eq(check)) {
            return Err(Error::RecoveryKeyTypo);
        }
        let mut k = Zeroizing::new([0u8; KEY_LEN]);
        k.copy_from_slice(key);
        Ok(Self(k))
    }

    fn wrap_key(&self) -> Zeroizing<[u8; KEY_LEN]> {
        hkdf_expand(&*self.0, INFO_RECOVERY)
    }
}

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}

/// Wraps the CRK: `version(1) || nonce(12) || GCM(ct || tag)`, AAD `vault/v1/crk-wrap || version`.
pub fn wrap_crk(crk: &Crk, rk: &RecoveryKey, rng: &mut impl CryptoRng) -> Vec<u8> {
    let aad = wrap_aad();
    let key = rk.wrap_key();
    let cipher = Aes256Gcm::new_from_slice(&*key).expect("32-byte key");
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &*crk.0,
                aad: &aad,
            },
        )
        .expect("AES-GCM encryption of a short message cannot fail");
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(WRAP_VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// Unwraps a CRK produced by [`wrap_crk`].
pub fn unwrap_crk(blob: &[u8], rk: &RecoveryKey) -> Result<Crk> {
    if blob.len() < 1 + NONCE_LEN + 16 {
        return Err(Error::Format("wrapped CRK too short".into()));
    }
    if blob[0] != WRAP_VERSION {
        return Err(Error::Unsupported(format!(
            "wrapped CRK version {}",
            blob[0]
        )));
    }
    let aad = wrap_aad();
    let key = rk.wrap_key();
    let cipher = Aes256Gcm::new_from_slice(&*key).expect("32-byte key");
    let pt = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&blob[1..1 + NONCE_LEN]),
                Payload {
                    msg: &blob[1 + NONCE_LEN..],
                    aad: &aad,
                },
            )
            .map_err(|_| Error::Crypto)?,
    );
    if pt.len() != KEY_LEN {
        return Err(Error::Crypto);
    }
    let mut k = Zeroizing::new([0u8; KEY_LEN]);
    k.copy_from_slice(&pt);
    Ok(Crk(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    fn rng(seed: u64) -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(seed)
    }

    #[test]
    fn subkeys_are_distinct_and_deterministic() {
        let crk = Crk::generate_with(&mut rng(1));
        let a = crk.subkeys();
        let b = crk.subkeys();
        assert_eq!(a.enc[..], b.enc[..]);
        assert_eq!(a.mac[..], b.mac[..]);
        assert_eq!(a.manifest[..], b.manifest[..]);
        assert_eq!(a.manifest_auth[..], b.manifest_auth[..]);
        assert_eq!(a.replica_id[..], b.replica_id[..]);
        let all = [
            &a.enc[..],
            &a.mac[..],
            &a.manifest[..],
            &a.manifest_auth[..],
            &a.replica_id[..],
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j]);
            }
        }
        let other = Crk::generate_with(&mut rng(2)).subkeys();
        assert_ne!(a.enc[..], other.enc[..]);
    }

    #[test]
    fn recovery_roundtrip_display_and_qr() {
        let rk = RecoveryKey::generate_with(&mut rng(3));
        let shown = rk.to_display();
        assert_eq!(shown.len(), 16 * 4 + 15);
        let groups: Vec<&str> = shown.split('-').collect();
        assert_eq!(groups.len(), 16);
        assert!(groups.iter().all(|g| g.len() == 4));
        assert!(
            shown
                .chars()
                .all(|c| c == '-' || c.is_ascii_uppercase() || ('2'..='7').contains(&c))
        );
        assert_eq!(RecoveryKey::parse(&shown).unwrap().to_display(), shown);
        // case-insensitive, spaces instead of dashes
        let loose = shown.to_lowercase().replace('-', " ");
        assert_eq!(RecoveryKey::parse(&loose).unwrap().to_display(), shown);
        let qr = rk.qr_payload();
        let rest = qr.strip_prefix("vault-recovery:v1:").unwrap();
        assert_eq!(rest.len(), 64);
        assert_eq!(RecoveryKey::parse(rest).unwrap().to_display(), shown);
    }

    #[test]
    fn digit_aliases_map_to_letters() {
        for (digit, letter) in [('0', 'O'), ('1', 'I'), ('8', 'B')] {
            let mut found = false;
            for seed in 0..200 {
                let rk = RecoveryKey::generate_with(&mut rng(seed));
                let shown = rk.to_display();
                if !shown.contains(letter) {
                    continue;
                }
                let aliased = shown.replace(letter, &digit.to_string());
                assert_eq!(RecoveryKey::parse(&aliased).unwrap().to_display(), shown);
                found = true;
                break;
            }
            assert!(found, "no sample contained {letter}");
        }
    }

    #[test]
    fn parse_ignores_any_whitespace() {
        let rk = RecoveryKey::generate_with(&mut rng(20));
        let shown = rk.to_display();
        let messy = format!("\t{}\n", shown.replace('-', "\u{a0}"));
        assert_eq!(RecoveryKey::parse(&messy).unwrap().to_display(), shown);
    }

    #[cfg(feature = "raw-key-access")]
    #[test]
    fn crk_raw_bytes_roundtrip() {
        let crk = Crk::generate_with(&mut rng(7));
        let bytes = crk.to_bytes();
        let back = Crk::from_bytes(&bytes);
        assert_eq!(crk.subkeys().manifest[..], back.subkeys().manifest[..]);
        assert_eq!(back.to_bytes()[..], bytes[..]);
        assert_eq!(format!("{back:?}"), "Crk(<redacted>)");
        assert!(Crk::from_slice(&bytes[..31]).is_err());
        assert!(Crk::from_slice(&[0u8; 33]).is_err());
        let s = Crk::from_slice(&bytes[..]).unwrap();
        assert_eq!(s.to_bytes()[..], bytes[..]);
    }

    fn kat_crk() -> Crk {
        Crk(Zeroizing::new(std::array::from_fn(|i| i as u8 + 1)))
    }

    fn kat_rk() -> RecoveryKey {
        RecoveryKey(Zeroizing::new(std::array::from_fn(|i| 0xA0 + i as u8)))
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Known answers from `tools/kat/kat.py` (Python `cryptography`, independent of
    /// this code), for CRK and recovery key = `[0x11; 32]`.
    #[test]
    fn subkey_kats_v1() {
        let s = Crk(Zeroizing::new([0x11; 32])).subkeys();
        assert_eq!(
            hex(&*s.enc),
            "fadf21e22aeeb4991efd0af527ddfdfe15a9d07c797d49b5f93e239f98a085d5"
        );
        assert_eq!(
            hex(&*s.mac),
            "95386a492d73b845448b54e6f46de466c1a76a74d3b04103d7e9401a95f59fd4"
        );
        assert_eq!(
            hex(&*s.manifest),
            "8348e40177efe9bf9096df8d9f037acbe54dca8d279b5f068ac45ad14e71b16a"
        );
        assert_eq!(
            hex(&*s.manifest_auth),
            "f39f7075471082f238686355f684641795ed0e576e04a86e777fe5843c9d36e4"
        );
        assert_eq!(
            hex(&*s.replica_id),
            "88c1e18db40d7125c8cb58e8f44d8ff7a79989b9d5f577518054dd82379317c7"
        );
        assert_eq!(
            hex(&*RecoveryKey(Zeroizing::new([0x11; 32])).wrap_key()),
            "89b2113a8f028550ac5c7f8156bd26d761bb0b75af35083109ca3d303c87be37"
        );
    }

    /// Known answers from `tools/kat/kat.py` for recovery key = `[0x11; 32]`.
    #[test]
    fn recovery_checksum_kat_v1() {
        let key = [0x11u8; 32];
        assert_eq!(hex(&RecoveryKey::checksum(&key)), "52c5e2d43fcec705");
        let rk = RecoveryKey(Zeroizing::new(key));
        assert_eq!(
            &*rk.to_display(),
            "CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIR-CEIV-FRPC-2Q74-5RYF"
        );
        assert_eq!(
            &*rk.qr_payload(),
            "vault-recovery:v1:CEIRCEIRCEIRCEIRCEIRCEIRCEIRCEIRCEIRCEIRCEIRCEIRCEIVFRPC2Q745RYF"
        );
    }

    /// Known answers from `tools/kat/kat.py` for the inputs `1..=32` (CRK) and
    /// `0xa0..=0xbf` (recovery key).
    #[test]
    fn known_answers() {
        let s = kat_crk().subkeys();
        assert_eq!(
            hex(&*s.enc),
            "44c845ff40730b73eeb9808f4c4b1aeab99cb101a897b84cbad68185e0988ce9"
        );
        assert_eq!(
            hex(&*s.mac),
            "fb134726d0eb7249a8d2ba0d5260da7ce2a055017fa20c32d6bf2cbdcfb1533e"
        );
        assert_eq!(
            hex(&*s.manifest),
            "d115b3f28b0f6a768a75885ffdb90e4173ef3a1dd6354752092bdaa239908a95"
        );
        assert_eq!(
            hex(&*s.manifest_auth),
            "7634e8564c41a2bc5a955b682d9c804bfeb2fae2fbdacc0c4f83741328969c8e"
        );
        assert_eq!(
            hex(&*s.replica_id),
            "70c468f9f142c85652197b3d0de2834f701336b477a03e5516894b41ac07413f"
        );
        let rk = kat_rk();
        let display =
            "UCQ2-FI5E-UWTK-PKFJ-VKV2-ZLNO-V6YL-DMVT-WS23-NN5Y-XG5L-XPF5-X277-ES6E-QNBE-6W4K";
        assert_eq!(&*rk.to_display(), display);
        assert_eq!(
            &*rk.qr_payload(),
            &format!("vault-recovery:v1:{}", display.replace('-', ""))
        );
        assert_eq!(
            hex(&*rk.wrap_key()),
            "f26b29d2181715e8fd2a03ac9895fba6c98315ddbd1bf3f2c8aa534a6272d174"
        );
    }

    #[test]
    fn known_answer_wrap_blob_and_aad() {
        let blob = (0..61)
            .map(|i| {
                let h = "01a0ce4a8df256558162747d72bad79a9d4dae96ab3a10a2b2e45124fc95b5eea037ba52b2f26c7008f5c73ad0078dd4a43130442b0bcd870f8c89bbb8";
                u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap()
            })
            .collect::<Vec<u8>>();
        let crk = unwrap_crk(&blob, &kat_rk()).unwrap();
        assert_eq!(crk.0[..], kat_crk().0[..]);
        // Pinned wrap output for a seeded RNG (printed by this code; the CRK decrypts
        // from it in Python with the wrap key and AAD of `docs/vault/README.md`).
        assert_eq!(wrap_crk(&kat_crk(), &kat_rk(), &mut rng(99)), blob);
        // AAD is exactly label || version byte.
        let key = kat_rk().wrap_key();
        let cipher = Aes256Gcm::new_from_slice(&*key).unwrap();
        let nonce = Nonce::from_slice(&blob[1..13]);
        let ok = |aad: &[u8]| {
            cipher
                .decrypt(
                    nonce,
                    Payload {
                        msg: &blob[13..],
                        aad,
                    },
                )
                .is_ok()
        };
        assert!(ok(b"vault/v1/crk-wrap\x01"));
        assert!(!ok(b"vault/v1/crk-wrap"));
        assert!(!ok(b"vault/v1/crk-wrap\x02"));
    }

    #[test]
    fn recovery_key_typo_detected() {
        let rk = RecoveryKey::generate_with(&mut rng(4));
        let shown = rk.to_display();
        let mut chars: Vec<char> = shown.chars().collect();
        let i = chars.iter().position(|c| *c != '-').unwrap();
        chars[i] = if chars[i] == 'A' { 'C' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        assert!(matches!(
            RecoveryKey::parse(&typo),
            Err(Error::RecoveryKeyTypo)
        ));
        assert!(matches!(
            RecoveryKey::parse("ABCD-EFGH"),
            Err(Error::Format(_))
        ));
        assert!(matches!(
            RecoveryKey::parse(&format!("{}!", *shown)),
            Err(Error::Format(_))
        ));
    }

    #[test]
    fn wrap_unwrap_roundtrip_and_format() {
        let crk = Crk::generate_with(&mut rng(5));
        let rk = RecoveryKey::generate_with(&mut rng(6));
        let blob = wrap_crk(&crk, &rk, &mut rng(7));
        assert_eq!(blob[0], 1);
        assert_eq!(blob.len(), 1 + 12 + 32 + 16);
        let back = unwrap_crk(&blob, &rk).unwrap();
        assert_eq!(back.subkeys().enc[..], crk.subkeys().enc[..]);
        // fresh nonce each call
        let blob2 = wrap_crk(&crk, &rk, &mut rng(8));
        assert_ne!(blob[1..13], blob2[1..13]);
    }

    #[test]
    fn wrong_recovery_key_fails_unwrap() {
        let crk = Crk::generate_with(&mut rng(9));
        let rk = RecoveryKey::generate_with(&mut rng(10));
        let wrong = RecoveryKey::generate_with(&mut rng(11));
        let blob = wrap_crk(&crk, &rk, &mut rng(12));
        assert!(matches!(unwrap_crk(&blob, &wrong), Err(Error::Crypto)));
        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(matches!(unwrap_crk(&tampered, &rk), Err(Error::Crypto)));
        let mut badver = blob.clone();
        badver[0] = 2;
        assert!(matches!(
            unwrap_crk(&badver, &rk),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            unwrap_crk(&blob[..10], &rk),
            Err(Error::Format(_))
        ));
    }

    #[test]
    fn debug_redacts_secrets() {
        let crk = Crk::generate_with(&mut rng(13));
        let rk = RecoveryKey::generate_with(&mut rng(14));
        let sub = crk.subkeys();
        let shown = rk.to_display();
        let first_group = &shown[..4];
        for dbg in [format!("{crk:?}"), format!("{rk:?}"), format!("{sub:?}")] {
            assert!(dbg.contains("redacted"), "{dbg}");
            assert!(!dbg.contains(first_group), "{dbg}");
        }
        let hex: String = crk
            .subkeys()
            .enc
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!format!("{sub:?}").contains(&hex));
    }
}
