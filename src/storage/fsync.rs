//! Filesystem sync helpers for crash safety.
//!
//! Shared by the text.log compaction path and the SyncManager.

use std::fs::File;
use std::io;
use std::path::Path;

/// Fsync a file by path (open, sync, close).
/// Used for files not held open (e.g., the `.compacting` marker).
pub fn fsync_file(path: &Path) -> io::Result<()> {
    let f = File::open(path)?;
    f.sync_all()
}

/// Fsync a directory to ensure rename/unlink/create durability.
///
/// On macOS, `sync_all()` on a directory fd issues `F_FULLFSYNC`,
/// ensuring the directory entry changes are flushed to disk.
/// On Linux, this is an `fsync()` on the directory fd.
pub fn fsync_dir(path: &Path) -> io::Result<()> {
    let d = File::open(path)?;
    d.sync_all()
}
