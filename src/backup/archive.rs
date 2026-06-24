//! Zip archive creation for backups.
//!
//! Creates a standard zip archive with Deflate compression containing
//! all backup files plus the manifest. Namespace subdirectory structure
//! is preserved in the archive.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use super::BackupError;
use super::manifest::BackupManifest;

/// Create a zip archive containing all backup files plus the manifest.
///
/// Files are stored with paths relative to `data_dir` to preserve
/// namespace subdirectory structure (e.g., "default/vectors.dat").
pub fn create_archive(
    data_dir: &Path,
    files: &[PathBuf],
    manifest: &BackupManifest,
    archive_path: &Path,
) -> Result<(), BackupError> {
    let archive_file = File::create(archive_path).map_err(BackupError::ArchiveCreationFailed)?;

    let mut zip = zip::ZipWriter::new(archive_file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // Add all data files, preserving relative paths.
    for file_path in files {
        let relative_path = file_path
            .strip_prefix(data_dir)
            .map_err(|_| {
                BackupError::ManifestFailed(format!(
                    "file {} not under data_dir {}",
                    file_path.display(),
                    data_dir.display()
                ))
            })?
            .to_string_lossy();

        zip.start_file(relative_path.as_ref(), options)
            .map_err(|e| BackupError::ArchiveCreationFailed(std::io::Error::other(e)))?;

        let mut file = File::open(file_path).map_err(|e| BackupError::CopyFailed {
            file: relative_path.to_string(),
            source: e,
        })?;

        let mut buf = [0u8; 8192];
        loop {
            let n = file.read(&mut buf).map_err(|e| BackupError::CopyFailed {
                file: relative_path.to_string(),
                source: e,
            })?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n])
                .map_err(|e| BackupError::CopyFailed {
                    file: relative_path.to_string(),
                    source: e,
                })?;
        }
    }

    // Add backup_manifest.json.
    let manifest_json = serde_json::to_string_pretty(manifest)
        .map_err(|e| BackupError::ManifestFailed(e.to_string()))?;

    zip.start_file("backup_manifest.json", options)
        .map_err(|e| BackupError::ArchiveCreationFailed(std::io::Error::other(e)))?;

    zip.write_all(manifest_json.as_bytes())
        .map_err(BackupError::ArchiveCreationFailed)?;

    zip.finish()
        .map_err(|e| BackupError::ArchiveCreationFailed(std::io::Error::other(e)))?;

    Ok(())
}
