//! Backup manifest generation and parsing.
//!
//! The manifest is a JSON file embedded in the backup archive that records
//! version info, timestamps, file list with CRC32 checksums, and total size.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::BackupError;

pub(crate) const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Metadata about a backup archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "ManifestWire")]
pub struct BackupManifest {
    #[serde(rename = "backupFormatVersion")]
    pub backup_format_version: u32,
    #[serde(
        rename = "canonicalSchemaVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub canonical_schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<super::schema_manifest::BackupManifestV2>,
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
#[serde(deny_unknown_fields)]
pub struct BackupFileEntry {
    /// Filename relative to data_dir (e.g. "default/vectors.dat").
    pub name: String,

    /// File size in bytes.
    pub size_bytes: u64,

    /// CRC32 checksum for integrity verification.
    pub crc32: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<[u8; 32]>,
}

fn legacy_version() -> u32 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestWire {
    #[serde(rename = "backupFormatVersion", default = "legacy_version")]
    backup_format_version: u32,
    #[serde(rename = "canonicalSchemaVersion")]
    canonical_schema_version: Option<u32>,
    snapshot: Option<super::schema_manifest::BackupManifestV2>,
    recalld_version: String,
    created_at: String,
    source_data_dir: String,
    hostname: String,
    files: Vec<BackupFileEntry>,
    total_size_bytes: u64,
}
impl TryFrom<ManifestWire> for BackupManifest {
    type Error = String;
    fn try_from(w: ManifestWire) -> Result<Self, Self::Error> {
        let manifest = Self {
            backup_format_version: w.backup_format_version,
            canonical_schema_version: w.canonical_schema_version,
            snapshot: w.snapshot,
            recalld_version: w.recalld_version,
            created_at: w.created_at,
            source_data_dir: w.source_data_dir,
            hostname: w.hostname,
            files: w.files,
            total_size_bytes: w.total_size_bytes,
        };
        manifest.validate_format()?;
        Ok(manifest)
    }
}
impl BackupManifest {
    pub(crate) fn validate_format(&self) -> Result<(), String> {
        let valid = match self.backup_format_version {
            1 => {
                self.canonical_schema_version.is_none()
                    && self.snapshot.is_none()
                    && self.files.iter().all(|f| f.sha256.is_none())
            }
            2 => {
                self.canonical_schema_version == Some(2)
                    && self.snapshot.is_some()
                    && self.files.iter().all(|f| f.sha256.is_some())
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err("unsupported or incomplete backup manifest format".into())
        }
    }
}

/// Generate a manifest for the given set of files.
///
/// Computes CRC32 checksums and file sizes for each file,
/// and stores paths relative to `data_dir`.
pub fn generate_manifest(
    data_dir: &Path,
    files: &[PathBuf],
) -> Result<BackupManifest, BackupError> {
    let canonical = crate::storage::validation::canonical_files(data_dir)
        .map_err(|e| BackupError::ManifestFailed(e.to_string()))?;
    let mut supplied = files.to_vec();
    supplied.sort();
    let mut required = canonical;
    required.sort();
    if supplied != required {
        return Err(BackupError::ManifestFailed(
            "canonical file inventory mismatch".into(),
        ));
    }
    let snapshot = super::schema_manifest::capture(data_dir)
        .map_err(|e| BackupError::ManifestFailed(e.to_string()))?;
    let mut entries = Vec::new();
    let mut total_size = 0u64;

    for file_path in files {
        let metadata = std::fs::metadata(file_path)?;
        let size = metadata.len();
        total_size = total_size
            .checked_add(size)
            .ok_or_else(|| BackupError::ManifestFailed("backup size overflow".into()))?;

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
            sha256: if snapshot.is_some() {
                Some(super::schema_manifest::sha256_file(file_path)?)
            } else {
                None
            },
        });
    }

    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    Ok(BackupManifest {
        backup_format_version: if snapshot.is_some() { 2 } else { 1 },
        canonical_schema_version: snapshot.as_ref().map(|_| 2),
        snapshot,
        recalld_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        source_data_dir: data_dir.display().to_string(),
        hostname,
        files: entries,
        total_size_bytes: total_size,
    })
}
