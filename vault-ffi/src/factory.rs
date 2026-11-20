// SPDX-License-Identifier: MIT
//! [`VaultFactory`]: creates, opens, restores and re-wraps vaults.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vault_container::RecoveryKey;
use vault_container::backend::LocalBackend;
use vault_container::engine::{Container, check_format};
use vault_container::replicate::{ReplicaId, restore};
use vault_proto::PairOffer;
use vault_server::{Observer, ServerConfig, SharedBackend, VaultServer};
use zeroize::Zeroizing;

use crate::error::FfiError;
use crate::handle::{Bridge, Shared, VaultHandle, Watch};
use crate::replica::backend_for;
use crate::runtime::OwnedRuntime;
use crate::state::{Persisted, StateCell};
use crate::types::{
    CreatedVault, FactoryConfig, HighWaterStore, OfferInfo, ReplicaTarget, RestoredVault,
};

/// Entry point for the app: owns the tokio runtime and the shared settings.
#[derive(uniffi::Object)]
pub struct VaultFactory {
    rt: Arc<OwnedRuntime>,
    config: FactoryConfig,
}

impl std::fmt::Debug for VaultFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultFactory")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Parses `pem` as one or more certificates (`Tls` error otherwise).
fn check_ca(pem: &str) -> Result<(), FfiError> {
    use rustls_pki_types::{CertificateDer, pem::PemObject};
    let certs: Result<Vec<_>, _> = CertificateDer::pem_slice_iter(pem.as_bytes()).collect();
    match certs {
        Ok(c) if !c.is_empty() => Ok(()),
        _ => Err(FfiError::Tls {
            detail: "extra_ca_pem holds no PEM certificate".into(),
        }),
    }
}

/// The container directory must exist and hold a header (no directory is
/// created by looking).
fn existing_vault(dir: &str) -> Result<PathBuf, FfiError> {
    let p = PathBuf::from(dir);
    match std::fs::read(p.join("header")) {
        Ok(bytes) => {
            vault_container::engine::check_header(&bytes)?;
            Ok(p)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FfiError::NotFound {
            detail: format!("no vault in {dir}"),
        }),
        Err(e) => Err(FfiError::io(e)),
    }
}

/// `dir` must be absent or an empty directory; returns whether it existed.
fn empty_target(dir: &Path) -> Result<bool, FfiError> {
    match std::fs::read_dir(dir) {
        Ok(mut it) => {
            if it.next().is_some() {
                Err(FfiError::AlreadyExists {
                    detail: format!("{} is not empty", dir.display()),
                })
            } else {
                Ok(true)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(FfiError::io(e)),
    }
}

/// Removes a directory on drop unless disarmed (a failed or cancelled
/// create/restore leaves nothing behind).
struct RemoveOnDrop {
    path: PathBuf,
    /// Recreate it empty afterwards (it existed, empty, before).
    keep_empty: bool,
    armed: bool,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
            if self.keep_empty {
                let _ = std::fs::create_dir(&self.path);
            }
        }
    }
}

fn parse_recovery_key(text: Zeroizing<String>) -> Result<RecoveryKey, FfiError> {
    RecoveryKey::parse(&text).map_err(FfiError::from)
}

impl VaultFactory {
    fn server_config(&self, watch: Arc<Watch>) -> ServerConfig {
        ServerConfig {
            approval_timeout: Duration::from_secs(self.config.approval_timeout_secs.max(1)),
            idle_timeout: self.config.idle_timeout_secs.map(Duration::from_secs),
            extra_ca_pem: self.config.extra_ca_pem.clone().map(String::into_bytes),
            observer: Some(Observer(watch)),
            ..ServerConfig::default()
        }
    }

    /// A handle around an open container (`Some`) or a locked one.
    async fn build(
        &self,
        backend: SharedBackend,
        container: Option<vault_server::VaultContainer>,
        state: Arc<StateCell>,
    ) -> Result<Arc<VaultHandle>, FfiError> {
        let shared = Shared::new(state);
        let watch = Watch::start(&self.rt, shared.clone());
        let cfg = self.server_config(watch);
        let approver = Arc::new(Bridge(shared.clone()));
        let server = match container {
            Some(c) => VaultServer::new(c, approver, cfg)?,
            None => VaultServer::new_locked(backend.clone(), approver, cfg).await?,
        };
        Ok(VaultHandle::new(
            self.rt.clone(),
            server,
            backend,
            shared,
            self.config.extra_ca_pem.clone(),
        ))
    }
}

#[uniffi::export]
impl VaultFactory {
    /// The factory and its runtime. Fails with `Tls` if `extra_ca_pem` is set
    /// but holds no certificate.
    #[uniffi::constructor]
    pub fn new(config: FactoryConfig) -> Result<Arc<Self>, FfiError> {
        if let Some(pem) = &config.extra_ca_pem {
            check_ca(pem)?;
        }
        Ok(Arc::new(Self {
            rt: OwnedRuntime::new()?,
            config,
        }))
    }

    /// Checks a scanned pairing code and returns what it asks for, without
    /// dialing anything (to pick the vault for its app and origin).
    pub fn parse_offer(&self, qr: String) -> Result<OfferInfo, FfiError> {
        let offer = PairOffer::from_qr(qr.trim())?;
        Ok(OfferInfo {
            relay_host: vault_server::relay_host(&offer.relay_url).to_string(),
            app_id: offer.app_id,
            origin: offer.origin,
        })
    }

    /// Create a vault for `app_id` at `origin` in `dir` (absent or empty; on
    /// failure nothing is left there). The handle is unlocked.
    ///
    /// Secret exports, each exactly once, here and nowhere else: the recovery
    /// key (show it once), the CRK (wrap it under K_auth at once) and the
    /// manifest key (seal it under K_bg at once). Wipe the `ByteArray`s after.
    /// The initial state is saved to `store` before this returns.
    pub async fn create(
        &self,
        dir: String,
        app_id: String,
        origin: String,
        store: Arc<dyn HighWaterStore>,
    ) -> Result<CreatedVault, FfiError> {
        let this = self.clone_ref();
        self.rt
            .run(async move {
                vault_container::Binding::new(&app_id, &origin)?;
                let path = PathBuf::from(&dir);
                let existed = empty_target(&path)?;
                let mut cleanup = RemoveOnDrop {
                    path: path.clone(),
                    keep_empty: existed,
                    armed: true,
                };
                let backend: SharedBackend = Arc::new(LocalBackend::open(&path).await?);
                let (c, rk) = Container::create(backend.clone(), &app_id, &origin).await?;
                let crk = c.crk().to_bytes();
                let mk = c.crk().subkeys().manifest;
                let state = StateCell::with(
                    store,
                    Persisted {
                        high_water: Some(c.high_water()),
                        ..Persisted::default()
                    },
                );
                state.save()?;
                let handle = this.build(backend, Some(c), state).await?;
                cleanup.armed = false;
                Ok(CreatedVault {
                    handle,
                    recovery_key: rk.to_display().to_string(),
                    crk: crk.to_vec(),
                    manifest_key: mk.to_vec(),
                })
            })
            .await
    }

    /// Open the vault in `dir`, locked: it pairs and checks the binding
    /// without a key and unlocks when a session is approved with the CRK.
    /// `NotFound` if `dir` holds no vault.
    pub async fn open(
        &self,
        dir: String,
        store: Arc<dyn HighWaterStore>,
    ) -> Result<Arc<VaultHandle>, FfiError> {
        let this = self.clone_ref();
        self.rt
            .run(async move {
                let path = existing_vault(&dir)?;
                let state = StateCell::load_existing(store)?;
                let backend: SharedBackend = Arc::new(LocalBackend::open(&path).await?);
                this.build(backend, None, state).await
            })
            .await
    }

    /// Restore a vault into `dir` (absent or empty) from the replica `source`
    /// (its stable id `source_id`) with the recovery key. Built in a sibling
    /// staging directory and moved into place only once complete; on failure
    /// or cancellation nothing is left. The source stays known as a replica
    /// (its mark is in the saved state). The handle is unlocked.
    ///
    /// No key is exported here: get the CRK for K_auth with
    /// [`VaultFactory::rewrap_with_recovery_key`] and the manifest key for
    /// K_bg with [`VaultHandle::export_manifest_key`].
    pub async fn restore(
        &self,
        dir: String,
        source: ReplicaTarget,
        source_id: String,
        recovery_key: String,
        store: Arc<dyn HighWaterStore>,
    ) -> Result<RestoredVault, FfiError> {
        let rk_text = Zeroizing::new(recovery_key);
        let this = self.clone_ref();
        self.rt
            .run(async move {
                let rk = parse_recovery_key(rk_text)?;
                if source_id.is_empty() {
                    return Err(FfiError::invalid("empty replica id"));
                }
                let path = PathBuf::from(&dir);
                let existed = empty_target(&path)?;
                let src = backend_for(source, this.config.extra_ca_pem.as_deref()).await?;
                check_format(src.as_ref()).await?;
                let staging = staging_dir(&path)?;
                let mut cleanup = RemoveOnDrop {
                    path: staging.clone(),
                    keep_empty: false,
                    armed: true,
                };
                let report = {
                    let target = LocalBackend::open(&staging).await?;
                    restore(
                        &[(ReplicaId(source_id), src)],
                        &rk,
                        &ReplicaId(String::new()),
                        &target,
                        None,
                    )
                    .await?
                };
                if existed {
                    std::fs::remove_dir(&path).map_err(FfiError::io)?;
                }
                std::fs::rename(&staging, &path).map_err(FfiError::io)?;
                cleanup.path = path.clone();
                cleanup.keep_empty = existed;
                let backend: SharedBackend = Arc::new(LocalBackend::open(&path).await?);
                let (c, _crk) = Container::open_with_recovery(backend.clone(), &rk, None).await?;
                let seq = c.seq();
                let state = StateCell::with(
                    store,
                    Persisted {
                        high_water: Some(c.high_water()),
                        replicas: report.state.clone(),
                    },
                );
                state.save()?;
                let handle = this.build(backend, Some(c), state).await?;
                cleanup.armed = false;
                Ok(RestoredVault { handle, seq })
            })
            .await
    }

    /// Secret export: open the vault in `dir` with the recovery key (against
    /// the saved high-water mark) and return a fresh copy of its CRK, to wrap
    /// under a new K_auth (after the old one was invalidated, or after a
    /// restore). Wipe the `ByteArray` after wrapping. The saved state's mark
    /// is raised to the opened manifest.
    pub async fn rewrap_with_recovery_key(
        &self,
        dir: String,
        recovery_key: String,
        store: Arc<dyn HighWaterStore>,
    ) -> Result<Vec<u8>, FfiError> {
        let rk_text = Zeroizing::new(recovery_key);
        self.rt
            .run(async move {
                let rk = parse_recovery_key(rk_text)?;
                let path = existing_vault(&dir)?;
                let state = StateCell::load_existing(store)?;
                let backend = LocalBackend::open(&path).await?;
                let floor = state.get().high_water;
                let (c, crk) = Container::open_with_recovery(backend, &rk, floor).await?;
                let hw = c.high_water();
                drop(c);
                state.update(|s| s.raise(hw))?;
                Ok(crk.to_bytes().to_vec())
            })
            .await
    }
}

impl VaultFactory {
    /// A cheap copy for moving into a task (same runtime and settings).
    fn clone_ref(&self) -> Arc<Self> {
        Arc::new(Self {
            rt: self.rt.clone(),
            config: self.config.clone(),
        })
    }
}

/// `.<name>.restoring-<hex>` next to `dir`.
fn staging_dir(dir: &Path) -> Result<PathBuf, FfiError> {
    use rand::RngCore;
    let parent = dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(FfiError::io)?;
    let name = dir
        .file_name()
        .map_or("vault".into(), |n| n.to_string_lossy().into_owned());
    let mut tag = [0u8; 6];
    rand::rng().fill_bytes(&mut tag);
    Ok(parent.join(format!(
        ".{name}.restoring-{}",
        data_encoding::HEXLOWER.encode(&tag)
    )))
}
