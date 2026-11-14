//! Backup and restore for Recalld data.
//!
//! Provides two main operations:
//! - **Backup**: Create a portable zip archive of all Recalld data files
//! - **Restore**: Restore data from a backup archive with safety checks

mod archive;
pub mod manifest;
pub mod restore;
pub mod schema_manifest;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::RecalldConfig;

pub use restore::{RestoreError, RestoreOptions, restore_from_backup};

// ── Error type ──────────────────────────────────────────────────────

/// Errors that can occur during a backup operation.
#[derive(Debug, Error)]
pub enum BackupError {
    #[error("failed to lock {file}: {source}")]
    LockFailed {
        file: String,
        source: std::io::Error,
    },

    #[error("data directory not found: {0}")]
    DataDirNotFound(String),

    #[error("destination path invalid: {0}")]
    InvalidDestination(String),

    #[error("failed to create temporary directory: {0}")]
    TempDirFailed(std::io::Error),

    #[error("failed to copy {file}: {source}")]
    CopyFailed {
        file: String,
        source: std::io::Error,
    },

    #[error("failed to create archive: {0}")]
    ArchiveCreationFailed(std::io::Error),

    #[error("manifest generation failed: {0}")]
    ManifestFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ── Public API ──────────────────────────────────────────────────────

/// Create a backup of all Recalld data.
///
/// Returns the path to the created backup archive.
pub async fn run_backup(
    config: &RecalldConfig,
    destination: &Path,
    source_data_dir: Option<&Path>,
    _force: bool,
) -> Result<PathBuf, BackupError> {
    // 1. Resolve paths.
    let data_dir = source_data_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(&config.storage.data_dir));

    if !data_dir.exists() {
        return Err(BackupError::DataDirNotFound(data_dir.display().to_string()));
    }

    let archive_path = resolve_archive_path(destination)?;

    println!("Starting backup...");
    println!("  Source: {}", data_dir.display());
    println!("  Destination: {}", archive_path.display());

    let ownership = crate::storage::ownership::StoreOwnership::acquire(&data_dir)
        .map_err(|e| BackupError::ManifestFailed(e.to_string()))?;
    let data_dir = ownership.path.clone();
    let _lock = crate::storage::ownership::lock_directory(&data_dir)
        .map_err(|e| BackupError::ManifestFailed(e.to_string()))?;
    let files = crate::storage::validation::canonical_files(&data_dir)
        .map_err(|e| BackupError::ManifestFailed(e.to_string()))?;

    // 4. Generate manifest.
    println!("Generating manifest...");
    let manifest = manifest::generate_manifest(&data_dir, &files)?;

    // 5. Create archive (locks still held during this).
    println!("Creating archive...");
    let parent = archive_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staged = tempfile::NamedTempFile::new_in(parent)?;
    archive::create_archive(&data_dir, &files, &manifest, staged.path())?;
    staged.as_file().sync_all()?;
    staged
        .persist(&archive_path)
        .map_err(|e| BackupError::Io(e.error))?;
    crate::storage::fsync::fsync_dir(parent)?;

    // 6. Locks dropped automatically here.
    let archive_size = std::fs::metadata(&archive_path)?.len();
    let archive_size_mb = archive_size as f64 / 1024.0 / 1024.0;

    println!("Backup complete!");
    println!("  Archive: {}", archive_path.display());
    println!("  Size: {:.2} MB (compressed)", archive_size_mb);
    println!(
        "  Uncompressed: {:.2} MB",
        manifest.total_size_bytes as f64 / 1024.0 / 1024.0
    );
    if manifest.total_size_bytes > 0 {
        println!(
            "  Compression ratio: {:.1}%",
            (1.0 - archive_size as f64 / manifest.total_size_bytes as f64) * 100.0
        );
    }

    Ok(archive_path)
}

// ── Internal helpers ────────────────────────────────────────────────

/// Determine the final archive path from the user-supplied destination.
///
/// - If destination is an existing directory (or doesn't exist and has no
///   `.zip` extension), treat it as a directory and auto-generate a
///   timestamped filename.
/// - If destination ends in `.zip`, use it as-is.
fn resolve_archive_path(destination: &Path) -> Result<PathBuf, BackupError> {
    if destination.is_dir() {
        // Existing directory — auto-generate filename.
        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S");
        let filename = format!("recalld-backup-{}.zip", timestamp);
        Ok(destination.join(filename))
    } else if destination.extension().and_then(|s| s.to_str()) == Some("zip") {
        // Explicit .zip file path.
        if let Some(parent) = destination.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Ok(destination.to_path_buf())
    } else if !destination.exists() {
        // Assume it's a directory that doesn't exist yet.
        std::fs::create_dir_all(destination)?;
        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S");
        let filename = format!("recalld-backup-{}.zip", timestamp);
        Ok(destination.join(filename))
    } else {
        Err(BackupError::InvalidDestination(
            "destination must be a directory or end in .zip".to_string(),
        ))
    }
}
