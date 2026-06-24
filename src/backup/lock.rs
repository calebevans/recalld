//! File locking utilities for consistent backups.
//!
//! Uses `fs2::FileExt::try_lock_exclusive()` for non-blocking,
//! cross-platform file locks. Locks are released automatically
//! when the `FileLock` guard is dropped.

use std::fs::File;
use std::path::Path;

use fs2::FileExt;

use super::BackupError;

/// RAII guard that holds an exclusive lock on a file.
/// Automatically unlocks on drop via fs2.
pub struct FileLock {
    _file: File,
    path: String,
}

impl FileLock {
    /// Acquire an exclusive lock on the file at `path`.
    ///
    /// Uses `try_lock_exclusive` so this never blocks — it fails
    /// immediately if another process holds the lock.
    pub fn acquire(path: &Path) -> Result<Self, BackupError> {
        let file = File::open(path).map_err(|e| BackupError::LockFailed {
            file: path.display().to_string(),
            source: e,
        })?;

        file.try_lock_exclusive()
            .map_err(|e| BackupError::LockFailed {
                file: path.display().to_string(),
                source: e,
            })?;

        Ok(FileLock {
            _file: file,
            path: path.display().to_string(),
        })
    }

    /// Returns the path this lock is held on.
    #[allow(dead_code)]
    pub fn path(&self) -> &str {
        &self.path
    }
}

// fs2 unlocks automatically when the file handle closes.
impl Drop for FileLock {
    fn drop(&mut self) {
        tracing::trace!(path = %self.path, "releasing file lock");
    }
}

/// Acquire exclusive locks on all database files in the data directory.
///
/// Lock order (to avoid deadlocks):
/// 1. meta.db
/// 2. fts.db
/// 3. edges.db
/// 4. text.log
/// 5. Per-namespace vectors.dat files
///
/// Returns a vec of locks that must be held until backup completes.
pub fn lock_all_files(data_dir: &Path, force: bool) -> Result<Vec<FileLock>, BackupError> {
    let mut locks = Vec::new();

    let lock_targets = ["meta.db", "fts.db", "edges.db", "text.log"];

    for filename in &lock_targets {
        let path = data_dir.join(filename);
        if !path.exists() {
            if force {
                eprintln!("warning: {} not found, skipping", filename);
                continue;
            } else {
                return Err(BackupError::LockFailed {
                    file: filename.to_string(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "file does not exist",
                    ),
                });
            }
        }

        match FileLock::acquire(&path) {
            Ok(lock) => locks.push(lock),
            Err(e) => {
                if force {
                    eprintln!("warning: failed to lock {}, continuing anyway", filename);
                } else {
                    return Err(e);
                }
            }
        }
    }

    // Lock all namespace subdirectory vectors.dat files.
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let vectors_file = path.join("vectors.dat");
            if vectors_file.exists() {
                match FileLock::acquire(&vectors_file) {
                    Ok(lock) => locks.push(lock),
                    Err(e) => {
                        if force {
                            eprintln!(
                                "warning: failed to lock {:?}, continuing",
                                vectors_file
                            );
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    Ok(locks)
}
