// SPDX-License-Identifier: MIT
//! Noise transport (`Noise_XX_25519_ChaChaPoly_SHA256`) between the browser
//! (initiator, per-session static key shown in the QR) and the vault (responder,
//! long-term static key), plus the short authentication string.
//!
//! Handshake: `Initiator::new` -> msg1 -> `Responder::handle_msg1` -> msg2 ->
//! `Initiator::handle_msg2` -> msg3 -> `ResponderAwaitingMsg3::handle_msg3`.
//! Every step consumes its state, so a failed handshake cannot be resumed.

use crate::{NOISE_PATTERN, PROLOGUE_LABEL, PairOffer, SAS_INFO, frame::MAX_PLAINTEXT};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{AeadInPlace, generic_array::GenericArray},
};
use curve25519_dalek::montgomery::MontgomeryPoint;
use hkdf::Hkdf;
use sha2::Sha256;
use snow::{
    Builder, Error as SnowError, HandshakeState, TransportState,
    params::{CipherChoice, DHChoice, HashChoice},
    resolvers::{BoxedCryptoResolver, CryptoResolver, DefaultResolver},
    types::{Cipher, Dh, Hash, Random},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// AEAD tag appended to every Noise transport message.
const TAG_LEN: usize = 16;
/// Largest ciphertext frame accepted by `Session::decrypt`.
const MAX_FRAME: usize = MAX_PLAINTEXT + TAG_LEN;

/// Noise and session errors. Deliberately coarse: no detail for an attacker.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NoiseError {
    #[error("static key does not match the pair offer")]
    KeyMismatch,
    #[error("handshake failed")]
    Handshake,
    #[error("plaintext exceeds the frame limit")]
    PlaintextTooLarge,
    #[error("frame too large")]
    FrameTooLarge,
    #[error("decryption failed")]
    Decrypt,
    #[error("encryption failed")]
    Encrypt,
    #[error("session is poisoned")]
    Poisoned,
}

type Result<T> = core::result::Result<T, NoiseError>;

/// Derive the X25519 public key. `mul_base_clamped` takes the secret by value,
/// so one copy of it lives on the stack for the call and is not wiped; this is
/// a short-lived, unavoidable copy (the caller's `Zeroizing` owns the real one).
fn public_of(secret: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint::mul_base_clamped(*secret).to_bytes()
}

/// Resolver that keeps snow's default primitives but wipes DH private keys and
/// ChaChaPoly keys on drop (snow 0.9 does not).
struct ZeroizingResolver;

impl CryptoResolver for ZeroizingResolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        DefaultResolver.resolve_rng()
    }
    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        match choice {
            DHChoice::Curve25519 => Some(Box::new(ZDh25519 {
                privkey: Zeroizing::new([0; 32]),
                pubkey: [0; 32],
            })),
            _ => None,
        }
    }
    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        DefaultResolver.resolve_hash(choice)
    }
    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        match choice {
            CipherChoice::ChaChaPoly => Some(Box::new(ZChaChaPoly {
                key: Zeroizing::new([0; 32]),
            })),
            _ => None,
        }
    }
}

struct ZDh25519 {
    privkey: Zeroizing<[u8; 32]>,
    pubkey: [u8; 32],
}

impl ZDh25519 {
    fn set_priv(&mut self, bytes: [u8; 32]) {
        *self.privkey = bytes;
        self.pubkey = public_of(&self.privkey);
    }
}

impl Dh for ZDh25519 {
    fn name(&self) -> &'static str {
        "25519"
    }
    fn pub_len(&self) -> usize {
        32
    }
    fn priv_len(&self) -> usize {
        32
    }
    fn set(&mut self, privkey: &[u8]) {
        let mut b = Zeroizing::new([0u8; 32]);
        let n = privkey.len().min(32);
        b[..n].copy_from_slice(&privkey[..n]);
        self.set_priv(*b);
    }
    fn generate(&mut self, rng: &mut dyn Random) {
        let mut b = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(&mut *b);
        self.set_priv(*b);
    }
    fn pubkey(&self) -> &[u8] {
        &self.pubkey
    }
    fn privkey(&self) -> &[u8] {
        &*self.privkey
    }
    fn dh(&self, pubkey: &[u8], out: &mut [u8]) -> core::result::Result<(), SnowError> {
        let pk: [u8; 32] = pubkey
            .get(..32)
            .and_then(|p| p.try_into().ok())
            .ok_or(SnowError::Dh)?;
        let shared = Zeroizing::new(MontgomeryPoint(pk).mul_clamped(*self.privkey).to_bytes());
        out.get_mut(..32)
            .ok_or(SnowError::Dh)?
            .copy_from_slice(&*shared);
        Ok(())
    }
}

struct ZChaChaPoly {
    key: Zeroizing<[u8; 32]>,
}

impl ZChaChaPoly {
    /// `ChaCha20Poly1305` zeroizes its key copy on drop (ZeroizeOnDrop), so this temporary is wiped.
    fn aead(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(GenericArray::from_slice(&*self.key))
    }
    fn nonce(n: u64) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[4..].copy_from_slice(&n.to_le_bytes());
        b
    }
}

impl Cipher for ZChaChaPoly {
    fn name(&self) -> &'static str {
        "ChaChaPoly"
    }
    fn set(&mut self, key: &[u8]) {
        let n = key.len().min(32);
        self.key[..n].copy_from_slice(&key[..n]);
    }
    fn encrypt(&self, nonce: u64, authtext: &[u8], plaintext: &[u8], out: &mut [u8]) -> usize {
        let n = plaintext.len();
        out[..n].copy_from_slice(plaintext);
        let tag = self
            .aead()
            .encrypt_in_place_detached(&Self::nonce(nonce).into(), authtext, &mut out[..n])
            .expect("ChaCha20-Poly1305 encryption cannot fail for in-bounds buffers");
        out[n..n + TAG_LEN].copy_from_slice(&tag);
        n + TAG_LEN
    }
    fn decrypt(
        &self,
        nonce: u64,
        authtext: &[u8],
        ciphertext: &[u8],
        out: &mut [u8],
    ) -> core::result::Result<usize, SnowError> {
        let n = ciphertext
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(SnowError::Decrypt)?;
        out.get_mut(..n)
            .ok_or(SnowError::Decrypt)?
            .copy_from_slice(&ciphertext[..n]);
        self.aead()
            .decrypt_in_place_detached(
                &Self::nonce(nonce).into(),
                authtext,
                &mut out[..n],
                ciphertext[n..].into(),
            )
            .map_err(|_| SnowError::Decrypt)?;
        Ok(n)
    }
}

fn prologue(offer: &PairOffer) -> Vec<u8> {
    let mut p = Vec::with_capacity(PROLOGUE_LABEL.len() + 16 + 32);
    p.extend_from_slice(PROLOGUE_LABEL);
    p.extend_from_slice(&offer.mailbox_id);
    p.extend_from_slice(&offer.browser_eph_pub);
    p
}

fn state(secret: &[u8; 32], offer: &PairOffer, initiator: bool) -> Result<HandshakeState> {
    state_with(Box::new(ZeroizingResolver), secret, offer, initiator)
}

fn state_with(
    resolver: BoxedCryptoResolver,
    secret: &[u8; 32],
    offer: &PairOffer,
    initiator: bool,
) -> Result<HandshakeState> {
    let params = NOISE_PATTERN.parse().map_err(|_| NoiseError::Handshake)?;
    let prologue = prologue(offer);
    let b = Builder::with_resolver(params, resolver)
        .prologue(&prologue)
        .local_private_key(secret);
    if initiator {
        b.build_initiator()
    } else {
        b.build_responder()
    }
    .map_err(|_| NoiseError::Handshake)
}

fn write(hs: &mut HandshakeState) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 1024];
    let n = hs
        .write_message(&[], &mut buf)
        .map_err(|_| NoiseError::Handshake)?;
    buf.truncate(n);
    Ok(buf)
}

fn read(hs: &mut HandshakeState, msg: &[u8]) -> Result<()> {
    // Handshake payloads are always empty.
    let mut buf = vec![0u8; 1024];
    if msg.len() > buf.len() {
        return Err(NoiseError::Handshake);
    }
    match hs.read_message(msg, &mut buf) {
        Ok(0) => Ok(()),
        _ => Err(NoiseError::Handshake),
    }
}

/// `(u32_be(HKDF-SHA256(hash, info = SAS_INFO)[..4]) mod 1_000_000)`, 6 digits.
fn sas_from_hash(hash: &[u8]) -> String {
    let hk = Hkdf::<Sha256>::new(None, hash);
    let mut okm = [0u8; 4];
    hk.expand(SAS_INFO, &mut okm)
        .expect("4 bytes is a valid HKDF length");
    format!("{:06}", u32::from_be_bytes(okm) % 1_000_000)
}

fn into_session(hs: HandshakeState) -> Result<Session> {
    let sas = sas_from_hash(hs.get_handshake_hash());
    let remote: [u8; 32] = hs
        .get_remote_static()
        .and_then(|r| r.try_into().ok())
        .ok_or(NoiseError::Handshake)?;
    let transport = hs
        .into_transport_mode()
        .map_err(|_| NoiseError::Handshake)?;
    Ok(Session {
        transport: Some(transport),
        sas,
        remote_static: remote,
    })
}

/// A fresh browser keypair together with the offer that advertises it.
///
/// The browser key MUST be generated fresh for every offer (every QR) and never
/// persisted: it is the browser's Noise static and its public half is what the
/// vault pins. Reuse would let an old QR or leaked key impersonate the browser.
pub struct PairKeys {
    offer: PairOffer,
    secret: Zeroizing<[u8; 32]>,
}

impl core::fmt::Debug for PairKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PairKeys")
            .field("offer", &self.offer)
            .finish_non_exhaustive()
    }
}

impl PairKeys {
    /// Generate a fresh X25519 keypair from the OS RNG and build the offer for it.
    pub fn generate(relay_url: String, mailbox_id: [u8; 16]) -> Result<Self> {
        let mut rng = DefaultResolver.resolve_rng().ok_or(NoiseError::Handshake)?;
        let mut secret = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(&mut *secret);
        let offer = PairOffer {
            version: crate::PROTOCOL_VERSION,
            relay_url,
            mailbox_id,
            browser_eph_pub: public_of(&secret),
        };
        Ok(Self { offer, secret })
    }

    /// The offer to encode as the QR.
    pub fn offer(&self) -> &PairOffer {
        &self.offer
    }

    /// Begin the handshake with these keys; returns the initiator and Noise message 1.
    pub fn into_initiator(self) -> Result<(Initiator, Vec<u8>)> {
        Initiator::new(&self.offer, *self.secret)
    }
}

/// Browser side. The static key is the per-session key whose public half is in the QR.
pub struct Initiator {
    hs: HandshakeState,
}

impl core::fmt::Debug for Initiator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Initiator(<redacted>)")
    }
}

impl Initiator {
    /// Start the handshake; returns the state and Noise message 1 for the relay.
    ///
    /// Fails with `KeyMismatch` unless `static_secret` belongs to `offer.browser_eph_pub`.
    pub fn new(offer: &PairOffer, static_secret: [u8; 32]) -> Result<(Self, Vec<u8>)> {
        let secret = Zeroizing::new(static_secret);
        if !bool::from(public_of(&secret).ct_eq(&offer.browser_eph_pub)) {
            return Err(NoiseError::KeyMismatch);
        }
        let mut hs = state(&secret, offer, true)?;
        let msg1 = write(&mut hs)?;
        Ok((Self { hs }, msg1))
    }

    /// Consume message 2; returns message 3 and the transport session.
    pub fn handle_msg2(mut self, msg2: &[u8]) -> Result<(Vec<u8>, Session)> {
        read(&mut self.hs, msg2)?;
        let msg3 = write(&mut self.hs)?;
        Ok((msg3, into_session(self.hs)?))
    }
}

/// Vault side, before message 1.
pub struct Responder {
    hs: HandshakeState,
    expected_remote: [u8; 32],
    static_public: [u8; 32],
}

impl core::fmt::Debug for Responder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Responder(<redacted>)")
    }
}

impl Responder {
    /// `offer` is the QR the vault scanned.
    pub fn new(vault_static_secret: [u8; 32], offer: &PairOffer) -> Result<Self> {
        let secret = Zeroizing::new(vault_static_secret);
        Ok(Self {
            hs: state(&secret, offer, false)?,
            expected_remote: offer.browser_eph_pub,
            static_public: public_of(&secret),
        })
    }

    /// The vault's static public key (for a fingerprint display).
    pub fn static_public(&self) -> [u8; 32] {
        self.static_public
    }

    /// Consume message 1; returns message 2 and the state awaiting message 3.
    pub fn handle_msg1(mut self, msg1: &[u8]) -> Result<(Vec<u8>, ResponderAwaitingMsg3)> {
        read(&mut self.hs, msg1)?;
        let msg2 = write(&mut self.hs)?;
        Ok((
            msg2,
            ResponderAwaitingMsg3 {
                hs: self.hs,
                expected_remote: self.expected_remote,
            },
        ))
    }
}

/// Vault side, after message 2.
pub struct ResponderAwaitingMsg3 {
    hs: HandshakeState,
    expected_remote: [u8; 32],
}

impl core::fmt::Debug for ResponderAwaitingMsg3 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ResponderAwaitingMsg3(<redacted>)")
    }
}

impl ResponderAwaitingMsg3 {
    /// Consume message 3. Fails unless the initiator's authenticated static key
    /// equals `browser_eph_pub` from the scanned QR (protocol MUST).
    pub fn handle_msg3(mut self, msg3: &[u8]) -> Result<Session> {
        read(&mut self.hs, msg3)?;
        let remote = self.hs.get_remote_static().ok_or(NoiseError::Handshake)?;
        if !bool::from(remote.ct_eq(&self.expected_remote[..])) {
            return Err(NoiseError::KeyMismatch);
        }
        into_session(self.hs)
    }
}

/// Established transport. Any error poisons it permanently.
///
/// Browser side: until the vault reports `Approved`, the only request that may
/// be sent is `OpenSession`; the layer above this one enforces that. A `Session`
/// proves only that the peer holds the pinned key, not that the user approved.
pub struct Session {
    transport: Option<TransportState>,
    sas: String,
    remote_static: [u8; 32],
}

impl core::fmt::Debug for Session {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("poisoned", &self.transport.is_none())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// 6-digit short authentication string, identical on both sides of an
    /// unmodified handshake.
    pub fn sas(&self) -> &str {
        &self.sas
    }

    /// The peer's authenticated static public key.
    pub fn remote_static(&self) -> [u8; 32] {
        self.remote_static
    }

    pub fn is_poisoned(&self) -> bool {
        self.transport.is_none()
    }

    /// Encrypt one frame (plaintext at most 60 KiB).
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let t = self.transport.as_mut().ok_or(NoiseError::Poisoned)?;
        if plaintext.len() > MAX_PLAINTEXT {
            self.transport = None;
            return Err(NoiseError::PlaintextTooLarge);
        }
        let mut out = vec![0u8; plaintext.len() + TAG_LEN];
        match t.write_message(plaintext, &mut out) {
            Ok(n) => {
                out.truncate(n);
                Ok(out)
            }
            Err(_) => {
                self.transport = None;
                Err(NoiseError::Encrypt)
            }
        }
    }

    /// Decrypt one frame; replayed, reordered, truncated or forged frames poison the session.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let t = self.transport.as_mut().ok_or(NoiseError::Poisoned)?;
        if frame.len() > MAX_FRAME {
            self.transport = None;
            return Err(NoiseError::FrameTooLarge);
        }
        let mut out = vec![0u8; frame.len()];
        match t.read_message(frame, &mut out) {
            Ok(n) => {
                out.truncate(n);
                Ok(out)
            }
            Err(_) => {
                self.transport = None;
                Err(NoiseError::Decrypt)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn offer_for(secret: &[u8; 32], mailbox: u8) -> PairOffer {
        PairOffer {
            version: crate::PROTOCOL_VERSION,
            relay_url: "wss://relay.example/".into(),
            mailbox_id: [mailbox; 16],
            browser_eph_pub: public_of(secret),
        }
    }

    fn handshake(
        browser: [u8; 32],
        vault: [u8; 32],
        offer: &PairOffer,
    ) -> Result<(Session, Session)> {
        let (i, m1) = Initiator::new(offer, browser)?;
        let r = Responder::new(vault, offer)?;
        let (m2, r) = r.handle_msg1(&m1)?;
        let (m3, bs) = i.handle_msg2(&m2)?;
        let vs = r.handle_msg3(&m3)?;
        Ok((bs, vs))
    }

    fn pair() -> (Session, Session) {
        let b = secret(1);
        handshake(b, secret(2), &offer_for(&b, 7)).unwrap()
    }

    #[test]
    fn handshake_and_roundtrip() {
        let (mut b, mut v) = pair();
        assert_eq!(b.remote_static(), public_of(&secret(2)));
        assert_eq!(v.remote_static(), public_of(&secret(1)));
        let f = b.encrypt(b"hello vault").unwrap();
        assert_eq!(v.decrypt(&f).unwrap(), b"hello vault");
        let f = v.encrypt(b"hello browser").unwrap();
        assert_eq!(b.decrypt(&f).unwrap(), b"hello browser");
        let max = vec![9u8; MAX_PLAINTEXT];
        let f = b.encrypt(&max).unwrap();
        assert_eq!(v.decrypt(&f).unwrap(), max);
    }

    #[test]
    fn sas_matches_both_sides() {
        let (b, v) = pair();
        assert_eq!(b.sas(), v.sas());
        assert_eq!(b.sas().len(), 6);
        assert!(b.sas().bytes().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn sas_format_is_zero_padded_and_pinned() {
        let hash = [0u8; 32];
        let hk = Hkdf::<Sha256>::new(None, &hash);
        let mut okm = [0u8; 4];
        hk.expand(SAS_INFO, &mut okm).unwrap();
        let want = format!("{:06}", u32::from_be_bytes(okm) % 1_000_000);
        assert_eq!(sas_from_hash(&hash), want);
        assert_eq!(sas_from_hash(&hash).len(), 6);
    }

    /// Known answer from `tools/kat/kat.py` (Python `cryptography` HKDF, independent
    /// of this code): okm = 83f0d811 under info `vault/v1/sas`.
    #[test]
    fn sas_kat_v1() {
        assert_eq!(SAS_INFO, b"vault/v1/sas");
        assert_eq!(PROLOGUE_LABEL, b"vault/v1");
        assert_eq!(sas_from_hash(&[0u8; 32]), "599249");
    }

    /// Run the handshake with the initiator and responder each on a chosen resolver.
    fn handshake_with(init_zero: bool, resp_zero: bool) -> Result<(Session, Session)> {
        let pick = |z: bool| -> BoxedCryptoResolver {
            if z {
                Box::new(ZeroizingResolver)
            } else {
                Box::new(DefaultResolver)
            }
        };
        let (b, v) = (secret(1), secret(2));
        let offer = offer_for(&b, 7);
        let mut i = state_with(pick(init_zero), &b, &offer, true)?;
        let mut r = state_with(pick(resp_zero), &v, &offer, false)?;
        let m1 = write(&mut i)?;
        read(&mut r, &m1)?;
        let m2 = write(&mut r)?;
        read(&mut i, &m2)?;
        let m3 = write(&mut i)?;
        read(&mut r, &m3)?;
        Ok((into_session(i)?, into_session(r)?))
    }

    #[test]
    fn zeroizing_resolver_interoperates_with_default() {
        for (a, b) in [(true, false), (false, true), (true, true)] {
            let (mut bs, mut vs) = handshake_with(a, b).unwrap();
            assert_eq!(bs.sas(), vs.sas());
            let f = bs.encrypt(b"ping").unwrap();
            assert_eq!(vs.decrypt(&f).unwrap(), b"ping");
            let f = vs.encrypt(b"pong").unwrap();
            assert_eq!(bs.decrypt(&f).unwrap(), b"pong");
        }
    }

    #[test]
    fn pair_keys_are_fresh_and_consistent() {
        let a = PairKeys::generate("wss://relay.example/".into(), [3; 16]).unwrap();
        let b = PairKeys::generate("wss://relay.example/".into(), [3; 16]).unwrap();
        assert_ne!(a.offer().browser_eph_pub, b.offer().browser_eph_pub);
        let offer = a.offer().clone();
        let (i, m1) = a.into_initiator().unwrap();
        let (m2, r) = Responder::new(secret(2), &offer)
            .unwrap()
            .handle_msg1(&m1)
            .unwrap();
        let (m3, bs) = i.handle_msg2(&m2).unwrap();
        assert_eq!(r.handle_msg3(&m3).unwrap().sas(), bs.sas());
        assert!(!format!("{b:?}").contains("secret"));
    }

    #[test]
    fn initiator_rejects_foreign_secret() {
        let offer = offer_for(&secret(1), 7);
        assert_eq!(
            Initiator::new(&offer, secret(3)).unwrap_err(),
            NoiseError::KeyMismatch
        );
    }

    #[test]
    fn mitm_relay_detected() {
        let (a, b, vault) = (secret(1), secret(5), secret(2));
        let offer_a = offer_for(&a, 7);

        // (a) relay runs the handshake with its own browser key B while the vault
        // holds the QR for key A: the prologues differ (B's offer) or, if the relay
        // copies the QR values, the remote static check rejects it.
        let offer_b = offer_for(&b, 7);
        assert!(handshake(b, vault, &offer_a).is_err()); // initiator refuses key != QR
        // Relay copies QR values into the prologue but authenticates with key B.
        let mut hs = state(&b, &offer_a, true).unwrap();
        let m1 = write(&mut hs).unwrap();
        let (m2, r) = Responder::new(vault, &offer_a)
            .unwrap()
            .handle_msg1(&m1)
            .unwrap();
        read(&mut hs, &m2).unwrap();
        let m3 = write(&mut hs).unwrap();
        assert_eq!(r.handle_msg3(&m3).unwrap_err(), NoiseError::KeyMismatch);
        // Relay using offer B end to end: prologue mismatch with the vault's QR (A).
        let (i, m1) = Initiator::new(&offer_b, b).unwrap();
        let (m2, _r) = Responder::new(vault, &offer_a)
            .unwrap()
            .handle_msg1(&m1)
            .unwrap();
        // msg1 only carries `e`; the prologue mismatch surfaces when msg2 is decrypted.
        assert!(i.handle_msg2(&m2).is_err());

        // (b) relay alters the mailbox id or eph key in the QR the vault sees.
        let mut tampered = offer_a.clone();
        tampered.mailbox_id[0] ^= 1;
        assert!(handshake_two_offers(a, vault, &offer_a, &tampered).is_err());
        let mut tampered = offer_a.clone();
        tampered.browser_eph_pub = public_of(&b);
        assert!(handshake_two_offers(a, vault, &offer_a, &tampered).is_err());

        // (c) full MITM: two independent handshakes give different SAS values.
        let relay_static = secret(9);
        let offer_r = offer_for(&relay_static, 7);
        let (browser_side, _) = handshake(a, relay_static, &offer_a).unwrap();
        let (_, vault_side) = handshake(relay_static, vault, &offer_r).unwrap();
        assert_ne!(browser_side.sas(), vault_side.sas());
    }

    /// Browser uses `browser_offer`, vault uses `vault_offer`.
    fn handshake_two_offers(
        browser: [u8; 32],
        vault: [u8; 32],
        browser_offer: &PairOffer,
        vault_offer: &PairOffer,
    ) -> Result<(Session, Session)> {
        let (i, m1) = Initiator::new(browser_offer, browser)?;
        let r = Responder::new(vault, vault_offer)?;
        let (m2, r) = r.handle_msg1(&m1)?;
        let (m3, bs) = i.handle_msg2(&m2)?;
        Ok((bs, r.handle_msg3(&m3)?))
    }

    #[test]
    fn replayed_frame_aborts() {
        let (mut b, mut v) = pair();
        let f = b.encrypt(b"one").unwrap();
        assert!(v.decrypt(&f).is_ok());
        assert_eq!(v.decrypt(&f).unwrap_err(), NoiseError::Decrypt);
        assert_eq!(v.decrypt(&f).unwrap_err(), NoiseError::Poisoned);
    }

    #[test]
    fn reordered_frame_aborts() {
        let (mut b, mut v) = pair();
        let f1 = b.encrypt(b"one").unwrap();
        let f2 = b.encrypt(b"two").unwrap();
        assert!(v.decrypt(&f2).is_err());
        assert_eq!(v.decrypt(&f1).unwrap_err(), NoiseError::Poisoned);
    }

    #[test]
    fn oversize_plaintext_rejected_and_poisons() {
        let (mut b, _) = pair();
        let big = vec![0u8; MAX_PLAINTEXT + 1];
        assert_eq!(b.encrypt(&big).unwrap_err(), NoiseError::PlaintextTooLarge);
        assert_eq!(b.encrypt(b"x").unwrap_err(), NoiseError::Poisoned);
    }

    #[test]
    fn forged_frame_poisons_both_directions() {
        let (mut b, mut v) = pair();
        let mut f = b.encrypt(b"data").unwrap();
        f[0] ^= 1;
        assert_eq!(v.decrypt(&f).unwrap_err(), NoiseError::Decrypt);
        assert!(v.is_poisoned());
        assert_eq!(v.encrypt(b"x").unwrap_err(), NoiseError::Poisoned);
        assert_eq!(v.decrypt(&[0u8; 20]).unwrap_err(), NoiseError::Poisoned);
    }

    #[test]
    fn oversize_frame_rejected() {
        let (_, mut v) = pair();
        let f = vec![0u8; MAX_FRAME + 1];
        assert_eq!(v.decrypt(&f).unwrap_err(), NoiseError::FrameTooLarge);
    }

    #[test]
    fn debug_is_redacted() {
        let (b, _) = pair();
        let s = format!("{b:?}");
        assert!(!s.contains(&b.sas().to_string()) && !s.contains("remote_static"));
        let (i, _) = Initiator::new(&offer_for(&secret(1), 7), secret(1)).unwrap();
        assert_eq!(format!("{i:?}"), "Initiator(<redacted>)");
    }
}
