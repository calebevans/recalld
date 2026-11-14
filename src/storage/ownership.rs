//! Stable ownership outside the directory that restore replaces.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use super::StorageError;

/// Owns one fresh file description and only a successfully acquired lock.
/// Close alone leaves a flock alive in an unrelated fork-to-exec child. Release
/// it explicitly at the owner's lifetime boundary, before closing this handle.
/// Inherited engine/file handles must never be used to write after fork.
pub(crate) struct OwnedFileLock {
    file: File,
    acquired: bool,
    owner_pid: u32,
}

impl OwnedFileLock {
    /// Only pass a newly opened, owned file; never a cloned/borrowed descriptor.
    pub(crate) fn acquire(file: File) -> std::io::Result<Self> {
        let mut lock = Self {
            file,
            acquired: false,
            owner_pid: std::process::id(),
        };
        lock.file.try_lock_exclusive()?;
        lock.acquired = true;
        Ok(lock)
    }
}
impl std::ops::Deref for OwnedFileLock {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}
impl std::ops::DerefMut for OwnedFileLock {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.file
    }
}
impl Drop for OwnedFileLock {
    fn drop(&mut self) {
        // A failed acquisition or a child's inherited guard owns no release.
        // Keep fs2's existing platform lock/unlock semantics; no retry or sync.
        if self.acquired
            && self.owner_pid == std::process::id()
            && let Err(error) = FileExt::unlock(&self.file)
        {
            tracing::warn!(%error, "failed to release owned file lock");
        }
    }
}

pub(crate) struct StoreOwnership {
    pub path: PathBuf,
    pub pending: PathBuf,
    _file: OwnedFileLock,
}

impl StoreOwnership {
    pub fn acquire(path: &Path) -> Result<Self, StorageError> {
        let absolute = std::path::absolute(path)?;
        let name = absolute.file_name().ok_or(StorageError::InvalidPath)?;
        let parent = absolute.parent().ok_or(StorageError::InvalidPath)?;
        std::fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        let path = parent.join(name);
        if path.is_symlink() {
            return Err(StorageError::InvalidPath);
        }
        let mut lock_name = name.to_os_string();
        lock_name.push(".recalld-owner.lock");
        let lock_path = parent.join(lock_name);
        if lock_path.is_symlink() {
            return Err(StorageError::InvalidPath);
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        let file = OwnedFileLock::acquire(file).map_err(|_| StorageError::DatabaseLocked)?;
        let mut pending_name = name.to_os_string();
        pending_name.push(".recalld-restore-pending");
        let pending = parent.join(pending_name);
        if pending.try_exists()? {
            return Err(StorageError::RecoveryRequired(format!(
                "interrupted restore: inspect {} and retain the old/staged stores",
                pending.display()
            )));
        }
        Ok(Self {
            path,
            pending,
            _file: file,
        })
    }
}

/// Also exclude binaries predating the stable sibling lock.
pub(crate) fn lock_directory(path: &Path) -> Result<OwnedFileLock, StorageError> {
    let lock = path.join("recalld.lock");
    if lock.is_symlink() {
        return Err(StorageError::InvalidPath);
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock)?;
    OwnedFileLock::acquire(file).map_err(|_| StorageError::DatabaseLocked)
}

#[cfg(all(test, unix))]
#[path = "ownership_tests.rs"]
mod tests;
