//! Stable ownership outside the directory that restore replaces.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use super::StorageError;

pub(crate) struct StoreOwnership {
    pub path: PathBuf,
    pub pending: PathBuf,
    _file: File,
}

impl StoreOwnership {
    pub fn acquire(path: &Path) -> Result<Self, StorageError> {
        Self::acquire_mode(path, false)
    }

    /// Engine startup may finish a journal; inspection/backup must not publish it.
    pub(crate) fn acquire_recovering(path: &Path) -> Result<Self, StorageError> {
        Self::acquire_mode(path, true)
    }

    fn acquire_mode(path: &Path, recover: bool) -> Result<Self, StorageError> {
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
        file.try_lock_exclusive()
            .map_err(|_| StorageError::DatabaseLocked)?;
        let mut pending_name = name.to_os_string();
        pending_name.push(".recalld-restore-pending");
        let pending = parent.join(pending_name);
        let owner = Self {
            path,
            pending,
            _file: file,
        };
        if owner.pending.is_symlink() || owner.pending.try_exists()? {
            if !recover {
                return Err(StorageError::RecoveryRequired(format!(
                    "interrupted restore at {}; recover through engine startup before inspection or another operation",
                    owner.pending.display()
                )));
            }
            super::restore_journal::recover(&owner)?;
        }
        Ok(owner)
    }
}

/// Also exclude binaries predating the stable sibling lock.
pub(crate) fn lock_directory(path: &Path) -> Result<File, StorageError> {
    let lock = path.join("recalld.lock");
    if lock.is_symlink() {
        return Err(StorageError::InvalidPath);
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock)?;
    file.try_lock_exclusive()
        .map_err(|_| StorageError::DatabaseLocked)?;
    Ok(file)
}
