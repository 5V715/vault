// SPDX-License-Identifier: MIT

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::RngCore;
use sha2::{Digest, Sha256};

use super::{ContainerBackend, Etag, PutOutcome, validate_name, validate_prefix};
use crate::{Error, Result};

const TMP_PREFIX: &str = ".tmp-";
const LOCK_FILE: &str = ".lock";

/// Backend storing each object as a file under a root directory.
///
/// Writes are atomic (temp file in the same directory, fsync, rename, fsync of the
/// directory). The etag is the lowercase hex SHA-256 of the content. Conditional
/// writes and deletes are serialized by an exclusive advisory lock on `<root>/.lock`,
/// so independent processes cannot race either.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    root: Arc<PathBuf>,
}

fn io_err(e: io::Error) -> Error {
    Error::Io(e.to_string())
}

fn etag_of(data: &[u8]) -> Etag {
    Etag(data_encoding::HEXLOWER.encode(&Sha256::digest(data)))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Removes leftover temp files from interrupted writes in `dir`.
fn clean_temp(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(TMP_PREFIX)
            && entry.file_type()?.is_file()
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn read_object(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(d) => Ok(Some(d)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().expect("object path has a parent");
    fs::create_dir_all(dir)?;
    let mut rnd = [0u8; 8];
    rand::rng().fill_bytes(&mut rnd);
    let tmp = dir.join(format!(
        "{TMP_PREFIX}{}",
        data_encoding::HEXLOWER.encode(&rnd)
    ));
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Held exclusive lock on `<root>/.lock`; released on drop.
struct LockGuard(File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl LocalBackend {
    /// Opens (creating if needed) a backend rooted at `root`, removing stale temp files.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        run(move || Self::open_blocking(root)).await
    }

    /// Blocking variant of [`LocalBackend::open`].
    pub fn open_blocking(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root_existed = root.is_dir();
        fs::create_dir_all(root.join("seg")).map_err(io_err)?;
        fs::create_dir_all(root.join("probe")).map_err(io_err)?;
        // Make the new directory entries durable before any object is written into them.
        sync_dir(&root).map_err(io_err)?;
        if !root_existed && let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
            sync_dir(parent).map_err(io_err)?;
        }
        let this = Self {
            root: Arc::new(root),
        };
        let _guard = this.lock()?;
        for dir in [
            this.root.to_path_buf(),
            this.root.join("seg"),
            this.root.join("probe"),
        ] {
            clean_temp(&dir).map_err(io_err)?;
        }
        drop(_guard);
        Ok(this)
    }

    fn lock(&self) -> Result<LockGuard> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(LOCK_FILE))
            .map_err(io_err)?;
        f.lock().map_err(io_err)?;
        Ok(LockGuard(f))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

async fn run<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Io(format!("blocking task failed: {e}")))?
}

#[async_trait::async_trait]
impl ContainerBackend for LocalBackend {
    async fn get(&self, name: &str) -> Result<Option<(Vec<u8>, Etag)>> {
        validate_name(name)?;
        let (this, name) = (self.clone(), name.to_owned());
        run(move || {
            // Renames are atomic, so readers never see partial content; no lock needed.
            Ok(read_object(&this.path(&name)).map_err(io_err)?.map(|d| {
                let e = etag_of(&d);
                (d, e)
            }))
        })
        .await
    }

    async fn put_if_absent(&self, name: &str, data: &[u8]) -> Result<PutOutcome> {
        validate_name(name)?;
        let (this, name, data) = (self.clone(), name.to_owned(), data.to_vec());
        run(move || {
            let _g = this.lock()?;
            let path = this.path(&name);
            if path.exists() {
                return Ok(PutOutcome::Conflict);
            }
            atomic_write(&path, &data).map_err(io_err)?;
            Ok(PutOutcome::Written(etag_of(&data)))
        })
        .await
    }

    async fn put_if_match(&self, name: &str, data: &[u8], etag: &Etag) -> Result<PutOutcome> {
        validate_name(name)?;
        let (this, name, data, etag) = (self.clone(), name.to_owned(), data.to_vec(), etag.clone());
        run(move || {
            let _g = this.lock()?;
            let path = this.path(&name);
            match read_object(&path).map_err(io_err)? {
                Some(cur) if etag_of(&cur) == etag => {
                    atomic_write(&path, &data).map_err(io_err)?;
                    Ok(PutOutcome::Written(etag_of(&data)))
                }
                _ => Ok(PutOutcome::Conflict),
            }
        })
        .await
    }

    async fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let (this, name) = (self.clone(), name.to_owned());
        run(move || {
            let _g = this.lock()?;
            let path = this.path(&name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(io_err(e)),
            }
            sync_dir(path.parent().expect("object path has a parent")).map_err(io_err)
        })
        .await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        validate_prefix(prefix)?;
        let this = self.clone();
        run(move || {
            let mut names = Vec::new();
            let entries = match fs::read_dir(this.root.join("seg")) {
                Ok(e) => e,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(names),
                Err(e) => return Err(io_err(e)),
            };
            for entry in entries {
                let entry = entry.map_err(io_err)?;
                let name = format!("seg/{}", entry.file_name().to_string_lossy());
                // skips temp files and anything else that is not a valid segment name
                if validate_name(&name).is_ok() {
                    names.push(name);
                }
            }
            names.sort();
            Ok(names)
        })
        .await
    }
}
