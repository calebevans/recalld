//! Restore Recalld data from a backup zip archive.
//!
//! Steps:
//! 1. Validate backup file and manifest
//! 2. Request user confirmation (unless `--force`)
//! 3. Stop daemon if running (unless `--no-stop-daemon`)
//! 4. Back up current data directory to `.bak-{timestamp}`
//! 5. Extract backup to data directory
//! 6. Verify extracted files via CRC32 checksums
//! 7. Report success (or rollback on failure)

use std::fs::{self, File, create_dir_all};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use zip::ZipArchive;

use super::manifest::BackupManifest;

// ── Error type ──────────────────────────────────────────────────────

/// Errors that can occur during a restore operation.
#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("backup file not found: {0}")]
    BackupNotFound(String),

    #[error("invalid backup archive: {0}")]
    InvalidArchive(String),

    #[error("manifest error: {0}")]
    ManifestError(String),

    #[error("version incompatibility: backup v{backup_version}, recalld v{recalld_version}")]
    VersionMismatch {
        backup_version: String,
        recalld_version: String,
    },

    #[error("checksum mismatch for {file}: expected {expected:08x}, got {actual:08x}")]
    ChecksumMismatch {
        file: String,
        expected: u32,
        actual: u32,
    },

    #[error("daemon error: {0}")]
    DaemonError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("backup entry attempts path traversal: {0}")]
    PathTraversal(String),

    #[error("user cancelled restore")]
    Cancelled,
}

// ── Options ─────────────────────────────────────────────────────────

/// Configuration for a restore operation.
#[derive(Clone, Debug)]
pub struct RestoreOptions {
    /// Path to the backup zip file.
    pub backup_path: PathBuf,

    /// Target data directory (from config).
    pub data_dir: PathBuf,

    /// Skip interactive confirmation.
    pub force: bool,

    /// Skip daemon shutdown attempt.
    pub no_stop_daemon: bool,
}

// ── Main entry point ────────────────────────────────────────────────

/// Restore Recalld data from a backup zip archive.
pub async fn restore_from_backup(opts: RestoreOptions) -> Result<(), RestoreError> {
    tracing::info!(backup = %opts.backup_path.display(), "starting restore");

    // Step 1: Validate backup file exists.
    if !opts.backup_path.exists() {
        return Err(RestoreError::BackupNotFound(
            opts.backup_path.display().to_string(),
        ));
    }

    // Step 2: Read and validate manifest.
    let manifest = read_manifest(&opts.backup_path)?;
    validate_manifest(&manifest)?;

    // Step 3: Request confirmation (unless --force).
    if !opts.force {
        request_confirmation(&opts, &manifest)?;
    }

    // Step 4: Stop daemon if running.
    if !opts.no_stop_daemon {
        stop_daemon_if_running().await?;
    }

    // Step 5: Back up current data directory.
    let backup_dir = backup_current_data(&opts.data_dir)?;
    tracing::info!(backup_dir = %backup_dir.display(), "backed up current data");

    // Step 6: Extract backup to data directory.
    match extract_backup(&opts.backup_path, &opts.data_dir, &manifest) {
        Ok(()) => {
            tracing::info!("successfully extracted backup");
            Ok(())
        }
        Err(e) => {
            // Rollback: restore the pre-existing data.
            tracing::error!(%e, "extraction failed, rolling back");
            rollback_restore(&opts.data_dir, &backup_dir)?;
            Err(e)
        }
    }
}

// ── Manifest validation ─────────────────────────────────────────────

/// Read the manifest from the backup archive.
fn read_manifest(backup_path: &Path) -> Result<BackupManifest, RestoreError> {
    let file = File::open(backup_path)?;
    let mut archive = ZipArchive::new(file)?;

    // Find manifest file.
    let mut manifest_file = archive
        .by_name("backup_manifest.json")
        .map_err(|_| RestoreError::InvalidArchive("missing backup_manifest.json".to_string()))?;

    let mut contents = String::new();
    manifest_file.read_to_string(&mut contents)?;

    serde_json::from_str(&contents).map_err(|e| RestoreError::ManifestError(e.to_string()))
}

/// Validate manifest version compatibility.
///
/// Checks that the backup's recalld_version has the same major.minor
/// as the currently running binary.
fn validate_manifest(manifest: &BackupManifest) -> Result<(), RestoreError> {
    let backup_version = &manifest.recalld_version;
    let current_version = env!("CARGO_PKG_VERSION");

    let backup_parts: Vec<&str> = backup_version.split('.').collect();
    let current_parts: Vec<&str> = current_version.split('.').collect();

    if backup_parts.len() < 2 || current_parts.len() < 2 {
        return Err(RestoreError::ManifestError(
            "invalid version format".to_string(),
        ));
    }

    if backup_parts[0] != current_parts[0] || backup_parts[1] != current_parts[1] {
        return Err(RestoreError::VersionMismatch {
            backup_version: backup_version.clone(),
            recalld_version: current_version.to_string(),
        });
    }

    Ok(())
}

// ── Interactive confirmation ────────────────────────────────────────

fn request_confirmation(
    opts: &RestoreOptions,
    manifest: &BackupManifest,
) -> Result<(), RestoreError> {
    eprintln!();
    eprintln!("WARNING: This will overwrite all data in the current data directory!");
    eprintln!();
    eprintln!("Current data directory: {}", opts.data_dir.display());
    eprintln!("Backup created at:      {}", manifest.created_at);
    eprintln!("Backup version:         {}", manifest.recalld_version);
    eprintln!("Backup source:          {}", manifest.source_data_dir);
    eprintln!();
    eprintln!(
        "Current data will be backed up to: {}.bak-<timestamp>",
        opts.data_dir.display()
    );
    eprintln!();

    eprint!("Type 'yes' to continue: ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    if input.trim().to_lowercase() != "yes" {
        return Err(RestoreError::Cancelled);
    }

    Ok(())
}

// ── Daemon shutdown ─────────────────────────────────────────────────

async fn stop_daemon_if_running() -> Result<(), RestoreError> {
    use crate::daemon::{DaemonClient, is_daemon_alive, socket_path};

    let socket = socket_path();

    if !is_daemon_alive(&socket).unwrap_or(false) {
        tracing::debug!("daemon not running, skipping shutdown");
        return Ok(());
    }

    eprintln!("Stopping daemon...");

    let client = DaemonClient::connect(&socket)
        .await
        .map_err(|e| RestoreError::DaemonError(format!("failed to connect: {e}")))?;

    client
        .call("shutdown", serde_json::json!({}))
        .await
        .map_err(|e| RestoreError::DaemonError(format!("shutdown call failed: {e}")))?;

    // Wait for daemon to exit (max 15 seconds).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while socket.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    if socket.exists() {
        return Err(RestoreError::DaemonError(
            "daemon did not shut down cleanly within 15 seconds".to_string(),
        ));
    }

    eprintln!("Daemon stopped successfully");
    Ok(())
}

// ── Back up current data ────────────────────────────────────────────

fn backup_current_data(data_dir: &Path) -> Result<PathBuf, RestoreError> {
    if !data_dir.exists() {
        // No current data to back up — create the directory.
        create_dir_all(data_dir)?;
        return Ok(data_dir.to_path_buf());
    }

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let backup_name = format!(
        "{}.bak-{}",
        data_dir.file_name().unwrap_or_default().to_string_lossy(),
        timestamp
    );
    let backup_dir = data_dir
        .parent()
        .unwrap_or(Path::new("."))
        .join(backup_name);

    tracing::info!(
        from = %data_dir.display(),
        to = %backup_dir.display(),
        "backing up current data"
    );

    eprintln!("Backing up current data to {}", backup_dir.display());

    // Rename the directory (atomic on most filesystems).
    fs::rename(data_dir, &backup_dir)?;

    Ok(backup_dir)
}

// ── Extract backup ──────────────────────────────────────────────────

fn extract_backup(
    backup_path: &Path,
    data_dir: &Path,
    manifest: &BackupManifest,
) -> Result<(), RestoreError> {
    create_dir_all(data_dir)?;

    let file = File::open(backup_path)?;
    let mut archive = ZipArchive::new(file)?;

    eprintln!("Extracting backup...");

    for entry in &manifest.files {
        eprintln!("  - {}", entry.name);

        // Extract file from archive.
        let mut zip_file = archive.by_name(&entry.name).map_err(|_| {
            RestoreError::InvalidArchive(format!(
                "manifest lists {} but not found in archive",
                entry.name
            ))
        })?;

        let target_path = data_dir.join(&entry.name);

        // ── Zip Slip prevention ──────────────────────────────────────
        // Reject entry names that could escape the data directory via
        // path traversal (e.g. "../../etc/passwd" or absolute paths).
        {
            // Reject absolute paths.
            if Path::new(&entry.name).is_absolute() {
                return Err(RestoreError::PathTraversal(entry.name.clone()));
            }

            // Reject any component that is "..".
            if Path::new(&entry.name)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(RestoreError::PathTraversal(entry.name.clone()));
            }

            // Canonicalize data_dir and verify the target resolves within it.
            // We canonicalize data_dir (which exists), then ensure the target
            // path — after joining — starts with the canonical data_dir prefix.
            let canonical_data_dir = data_dir.canonicalize()?;
            // Parent dirs of target_path already exist or will be created below,
            // so we build the canonical form by joining on the canonical base.
            let canonical_target = canonical_data_dir.join(&entry.name);
            if !canonical_target.starts_with(&canonical_data_dir) {
                return Err(RestoreError::PathTraversal(entry.name.clone()));
            }
        }

        // Ensure parent directory exists (critical for namespace subdirectories).
        if let Some(parent) = target_path.parent() {
            create_dir_all(parent)?;
        }

        let mut output = File::create(&target_path)?;
        let mut hasher = crc32fast::Hasher::new();
        let mut buffer = [0u8; 8192];

        loop {
            let n = zip_file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n])?;
            hasher.update(&buffer[..n]);
        }

        // Verify checksum.
        let actual_crc = hasher.finalize();
        if actual_crc != entry.crc32 {
            return Err(RestoreError::ChecksumMismatch {
                file: entry.name.clone(),
                expected: entry.crc32,
                actual: actual_crc,
            });
        }
    }

    eprintln!("All files extracted and verified");
    Ok(())
}

// ── Rollback ────────────────────────────────────────────────────────

fn rollback_restore(data_dir: &Path, backup_dir: &Path) -> Result<(), RestoreError> {
    tracing::warn!(
        data_dir = %data_dir.display(),
        backup_dir = %backup_dir.display(),
        "rolling back restore"
    );

    // Remove partially extracted data directory.
    if data_dir.exists() {
        fs::remove_dir_all(data_dir)?;
    }

    // Restore original data.
    fs::rename(backup_dir, data_dir)?;

    eprintln!("Rollback complete: original data restored");
    Ok(())
}
