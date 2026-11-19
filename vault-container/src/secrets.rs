// SPDX-License-Identifier: MIT

//! Passphrase-sealed secrets files shared by the command-line tools (`vaultctl`,
//! `vault-dev`): Argon2id (64 MiB, t=3, p=1) -> AES-256-GCM, with the file's magic
//! string as associated data. This is a testing aid; the Android app protects its
//! keys with the Keystore instead.

use std::io::Write;
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::engine::HighWater;
use crate::replicate::{ObservedMark, ReplicaState};
use crate::{Error, Result};

/// File name of `vaultctl`'s secrets file inside the container directory.
pub const SECRETS_FILE: &str = ".vaultctl";
const MAGIC: &[u8] = b"vaultctl-secrets-v1";
const ARGON_M_KIB: u32 = 65536;
const ARGON_T: u32 = 3;
const ARGON_P: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

#[derive(Serialize, Deserialize)]
pub enum ReplicaCfg {
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        access_key: String,
        secret_key: String,
        path_style: bool,
        allow_insecure_http: bool,
        ca_pem: Option<String>,
    },
    WebDav {
        url: String,
        username: String,
        password: String,
        allow_insecure_http: bool,
        ca_pem: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
pub struct ReplicaEntry {
    pub id: String,
    pub cfg: ReplicaCfg,
}

#[derive(Serialize, Deserialize)]
pub struct Secrets {
    /// The recovery key text; it unwraps the CRK stored in the container header.
    pub recovery_key: String,
    pub high_water: Option<HighWater>,
    pub state: ReplicaState,
    pub replicas: Vec<ReplicaEntry>,
    /// Mark each diverged replica held when `replicate` last reported it; `replica
    /// adopt` confirms exactly this.
    pub last_observed: std::collections::BTreeMap<String, ObservedMark>,
}

impl Drop for Secrets {
    fn drop(&mut self) {
        self.recovery_key.zeroize();
        for r in &mut self.replicas {
            match &mut r.cfg {
                ReplicaCfg::S3 { secret_key, .. } => secret_key.zeroize(),
                ReplicaCfg::WebDav { password, .. } => password.zeroize(),
            }
        }
    }
}

fn derive_key(pass: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    let params = argon2::Params::new(ARGON_M_KIB, ARGON_T, ARGON_P, Some(32))
        .map_err(|e| Error::Format(e.to_string()))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    a.hash_password_into(pass.as_bytes(), salt, &mut *key)
        .map_err(|e| Error::Format(e.to_string()))?;
    Ok(key)
}

/// Encrypt `plain` under a key derived from `pass`; `magic` prefixes the blob and is
/// bound as associated data, so blobs of different files cannot be swapped.
pub fn seal(magic: &[u8], plain: &[u8], pass: &str) -> Result<Vec<u8>> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut nonce);
    let key = derive_key(pass, &salt)?;
    let ct = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&*key))
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: magic,
            },
        )
        .map_err(|_| Error::Crypto)?;
    let mut out = magic.to_vec();
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Inverse of [`seal`]; every failure (wrong passphrase, tampering, wrong file) is
/// [`Error::BadSecrets`].
pub fn unseal(magic: &[u8], blob: &[u8], pass: &str) -> Result<Zeroizing<Vec<u8>>> {
    let rest = blob.strip_prefix(magic).ok_or(Error::BadSecrets)?;
    if rest.len() < SALT_LEN + NONCE_LEN + 16 {
        return Err(Error::BadSecrets);
    }
    let (salt, rest) = rest.split_at(SALT_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    let key = derive_key(pass, salt)?;
    Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&*key))
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: magic,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| Error::BadSecrets)
}

/// Seal `plain` and write it atomically (temp file, fsync, rename) to `dir/name`
/// with mode 0600.
pub fn save_sealed(dir: &Path, name: &str, magic: &[u8], plain: &[u8], pass: &str) -> Result<()> {
    let blob = seal(magic, plain, pass)?;
    let tmp = dir.join(format!("{name}.tmp"));
    // A stale temp file is from an interrupted earlier save; it never holds the only copy.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io(e.to_string())),
    }
    let write = || -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(&blob)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dir.join(name))?;
        std::fs::File::open(dir)?.sync_all()
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::Io(e.to_string())
    })
}

/// Decrypt and decode a `.vaultctl` blob.
pub fn open_secrets(blob: &[u8], pass: &str) -> Result<Secrets> {
    let plain = unseal(MAGIC, blob, pass)?;
    postcard::from_bytes(&plain).map_err(|_| Error::BadSecrets)
}

/// Read and open `dir/.vaultctl`.
pub fn load_secrets(dir: &Path, pass: &str) -> Result<Secrets> {
    let blob = std::fs::read(dir.join(SECRETS_FILE)).map_err(|e| {
        Error::Io(format!(
            "cannot read {}/{SECRETS_FILE}: {e} (run `vaultctl create`?)",
            dir.display()
        ))
    })?;
    open_secrets(&blob, pass)
}

/// Seal and atomically write `dir/.vaultctl`.
pub fn save_secrets(dir: &Path, s: &Secrets, pass: &str) -> Result<()> {
    let plain = Zeroizing::new(postcard::to_allocvec(s).map_err(|e| Error::Format(e.to_string()))?);
    save_sealed(dir, SECRETS_FILE, MAGIC, &plain, pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_binding() {
        let blob = seal(b"magic-a", b"hello", "pw").unwrap();
        assert_eq!(&*unseal(b"magic-a", &blob, "pw").unwrap(), b"hello");
        assert!(matches!(
            unseal(b"magic-a", &blob, "other"),
            Err(Error::BadSecrets)
        ));
        assert!(matches!(
            unseal(b"magic-b", &blob, "pw"),
            Err(Error::BadSecrets)
        ));
    }
}
