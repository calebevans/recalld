//! Backup manifest generation and parsing.
//!
//! The manifest is a JSON file embedded in the backup archive that records
//! version info, timestamps, file list with CRC32 checksums, and total size.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::BackupError;

/// Metadata about a backup archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Recalld version that created this backup.
    pub recalld_version: String,

    /// Timestamp when backup was created (RFC 3339 format).
    pub created_at: String,

    /// Original data_dir path (informational only).
    pub source_data_dir: String,

    /// Hostname where backup was created.
    pub hostname: String,

    /// List of files included in the backup.
    pub files: Vec<BackupFileEntry>,

    /// Total uncompressed size in bytes.
    pub total_size_bytes: u64,
}

/// A single file entry in the backup manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupFileEntry {
    /// Filename relative to data_dir (e.g. "default/vectors.dat").
    pub name: String,

    /// File size in bytes.
    pub size_bytes: u64,

    /// CRC32 checksum for integrity verification.
    pub crc32: u32,
}

/// Generate a manifest for the given set of files.
///
/// Computes CRC32 checksums and file sizes for each file,
/// and stores paths relative to `data_dir`.
pub fn generate_manifest(
    data_dir: &Path,
    files: &[PathBuf],
) -> Result<BackupManifest, BackupError> {
    let mut entries = Vec::new();
    let mut total_size = 0u64;

    for file_path in files {
        let metadata = std::fs::metadata(file_path)?;
        let size = metadata.len();
        total_size += size;

        // Compute CRC32.
        let mut file = std::fs::File::open(file_path)?;
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let crc = hasher.finalize();

        // Store path relative to data_dir (preserves namespace subdirs).
        let name = file_path
            .strip_prefix(data_dir)
            .map_err(|_| {
                BackupError::ManifestFailed(format!(
                    "file {} not under data_dir {}",
                    file_path.display(),
                    data_dir.display()
                ))
            })?
            .to_string_lossy()
            .to_string();

        entries.push(BackupFileEntry {
            name,
            size_bytes: size,
            crc32: crc,
        });
    }

    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    Ok(BackupManifest {
        recalld_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        source_data_dir: data_dir.display().to_string(),
        hostname,
        files: entries,
        total_size_bytes: total_size,
    })
}
