//! Restore Recalld data from a backup zip archive.
//!
//! Validate in staging, then publish under stable target ownership. Interrupted
//! publication fails closed; the journal names both retained copies for recovery.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, create_dir_all};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::storage::{
    StorageError,
    fsync::fsync_dir,
    ownership::{StoreOwnership, lock_directory},
    validation::validate_existing,
};

use thiserror::Error;
use zip::ZipArchive;

use super::manifest::BackupManifest;

// ── Error type ──────────────────────────────────────────────────────

/// Errors that can occur during a restore operation.
#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("storage validation/ownership: {0}")]
    Storage(#[from] StorageError),
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

    /// Legacy compatibility flag. Restore never stops an unrelated daemon;
    /// the target must already be quiescent regardless of this value.
    pub no_stop_daemon: bool,
}

// ── Main entry point ────────────────────────────────────────────────

/// Restore Recalld data from a backup zip archive.
pub async fn restore_from_backup(opts: RestoreOptions) -> Result<(), RestoreError> {
    if !opts.backup_path.exists() {
        return Err(RestoreError::BackupNotFound(
            opts.backup_path.display().to_string(),
        ));
    }
    let manifest = read_manifest(&opts.backup_path)?;
    validate_manifest(&manifest)?;
    if !opts.force {
        request_confirmation(&opts, &manifest)?;
    }

    // No daemon shutdown by global socket: it may own a different target.
    // Both force and no_stop_daemon still require exclusive target ownership.
    let ownership = StoreOwnership::acquire(&opts.data_dir)?;
    let target = &ownership.path;
    let parent = target.parent().ok_or(StorageError::InvalidPath)?;
    let _old_lock = if target.exists() {
        Some(lock_directory(target)?)
    } else {
        None
    };
    let stage = tempfile::Builder::new()
        .prefix(".recalld-restore-")
        .tempdir_in(parent)?;
    extract_backup(&opts.backup_path, stage.path(), &manifest)?;
    // FTS is derived. Never serve an old checkpoint as authoritative; normal
    // system startup rebuilds an absent FTS database from the restored records.
    if stage.path().join("fts.db").exists() {
        fs::remove_file(stage.path().join("fts.db"))?;
    }
    let _new_lock = lock_directory(stage.path())?;
    fsync_dir(stage.path())?;
    let previous = parent.join(format!(
        "{}.bak-{}",
        target.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let staged = stage.keep();
    let journal = serde_json::json!({"target": target, "staged": staged, "previous": previous});
    let mut marker = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&ownership.pending)?;
    marker.write_all(journal.to_string().as_bytes())?;
    marker.sync_all()?;
    fsync_dir(parent)?;
    // From this point every error leaves the marker and all available copies.
    // Startup refuses service until an operator resolves the interrupted swap.
    if target.exists() {
        fs::rename(target, &previous)?;
        fsync_dir(parent)?;
    }
    fs::rename(&staged, target)?;
    fsync_dir(parent)?;
    validate_existing(target)?;
    fs::remove_file(&ownership.pending)?;
    fsync_dir(parent)?;
    tracing::info!(retained = %previous.display(), "restore published; pre-restore data retained");
    Ok(())
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

// ── Complete legacy archive validation ──────────────────────────────

fn valid_entry_name(name: &str) -> bool {
    if matches!(name, "meta.db" | "edges.db" | "fulltext.dat" | "fts.db") {
        return true;
    }
    if crate::storage::metadata::text_generation::is_generation_filename(name) {
        return true;
    }
    let Some(namespace) = name.strip_suffix("/vectors.dat") else {
        return false;
    };
    crate::model::validation::validate_namespace_name(namespace).is_ok()
}

// zip 2.x collapses duplicate names into an IndexMap. Count the original
// central headers before relying on its inventory; duplicate entries must not
// disappear from validation. Header payload lengths are bounded by the file.
fn reject_duplicate_archive_entries(
    path: &Path,
    archive: &ZipArchive<File>,
) -> Result<(), RestoreError> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(archive.central_directory_start()))?;
    let mut count = 0;
    loop {
        let mut signature = [0; 4];
        file.read_exact(&mut signature)?;
        if signature != *b"PK\x01\x02" {
            break;
        }
        count += 1;
        if count > archive.len() {
            return Err(RestoreError::InvalidArchive(
                "duplicate archive entries".into(),
            ));
        }
        let mut rest = [0; 42];
        file.read_exact(&mut rest)?;
        let variable_length: u64 = [24, 26, 28]
            .into_iter()
            .map(|offset| u64::from(u16::from_le_bytes([rest[offset], rest[offset + 1]])))
            .sum();
        let end = file
            .stream_position()?
            .checked_add(variable_length)
            .filter(|end| *end <= length)
            .ok_or_else(|| {
                RestoreError::InvalidArchive("central directory extent exceeds archive".into())
            })?;
        file.seek(SeekFrom::Start(end))?;
    }
    if count != archive.len() {
        return Err(RestoreError::InvalidArchive(
            "inconsistent central directory".into(),
        ));
    }
    Ok(())
}

fn extract_backup(
    backup_path: &Path,
    data_dir: &Path,
    manifest: &BackupManifest,
) -> Result<(), RestoreError> {
    let invalid = |message: &str| RestoreError::InvalidArchive(message.to_owned());
    let mut declared = HashMap::new();
    let mut total = 0u64;
    for entry in &manifest.files {
        if !valid_entry_name(&entry.name)
            || entry.size_bytes == 0
            || declared.insert(entry.name.as_str(), entry).is_some()
        {
            return Err(invalid("invalid, empty or duplicate manifest entry"));
        }
        total = total
            .checked_add(entry.size_bytes)
            .ok_or_else(|| invalid("manifest size overflow"))?;
    }
    if total != manifest.total_size_bytes
        || ["meta.db", "edges.db", "fulltext.dat"]
            .iter()
            .any(|name| !declared.contains_key(name))
    {
        return Err(invalid(
            "incomplete core inventory or inconsistent total size",
        ));
    }
    let mut archive = ZipArchive::new(File::open(backup_path)?)?;
    reject_duplicate_archive_entries(backup_path, &archive)?;
    let mut seen = HashSet::new();
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let name = file.name();
        if !seen.insert(name.to_owned())
            || file.is_dir()
            || file
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000))
        {
            return Err(invalid("duplicate archive entry, link or special file"));
        }
        if name == "backup_manifest.json" {
            continue;
        }
        let entry = declared
            .get(name)
            .ok_or_else(|| invalid("undeclared archive entry"))?;
        if file.size() != entry.size_bytes {
            return Err(invalid("archive size disagrees with manifest"));
        }
    }
    if seen.len() != declared.len() + 1 || !seen.contains("backup_manifest.json") {
        return Err(invalid("archive and manifest inventories disagree"));
    }
    create_dir_all(data_dir)?;
    for entry in &manifest.files {
        let mut input = archive.by_name(&entry.name)?;
        let path = data_dir.join(&entry.name);
        create_dir_all(path.parent().unwrap())?;
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        let mut hasher = crc32fast::Hasher::new();
        let mut size = 0u64;
        let mut buffer = [0; 8192];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if size > entry.size_bytes {
                return Err(invalid("entry expands beyond declared size"));
            }
            output.write_all(&buffer[..n])?;
            hasher.update(&buffer[..n]);
        }
        let crc = hasher.finalize();
        if crc != entry.crc32 {
            return Err(RestoreError::ChecksumMismatch {
                file: entry.name.clone(),
                expected: entry.crc32,
                actual: crc,
            });
        }
        if size != entry.size_bytes {
            return Err(invalid("extracted size disagrees with manifest"));
        }
        output.sync_all()?;
        fsync_dir(path.parent().unwrap())?;
    }
    // Required namespace files come from meta.db, never from the manifest alone.
    let canonical = validate_existing(data_dir)?;
    if declared
        .keys()
        .any(|name| *name != "fts.db" && !canonical.contains(*name))
    {
        return Err(invalid("manifest contains a noncanonical namespace file"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{archive::create_archive, manifest::generate_manifest};
    use crate::model::{DecayPhase, DiskRecord, MemoryId, NamespaceConfig};
    use crate::storage::{RedbStorageEngine, StorageEngine, validation::canonical_files};

    fn fixture(root: &Path) -> (PathBuf, BackupManifest) {
        let source = root.join("source");
        let mut engine = RedbStorageEngine::open(&source).unwrap();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.embedding_dim = 4;
        let ns = engine.create_namespace(&ns).unwrap();
        let id = MemoryId::new();
        let mut record = DiskRecord {
            schema: None,
            version: DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: ns.get(),
            created_at: 0,
            last_accessed_at: 0,
            phase: DecayPhase::Full,
            strength: 1.,
            decay_strength: 1.,
            stability: 1.,
            difficulty: 5.,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "backup fixture".into(),
            tags: vec![],
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        };
        engine
            .insert_memory(
                id,
                ns,
                &mut record,
                &[1., 2., 3., 4.],
                Some("retained text"),
            )
            .unwrap();
        engine.sync().unwrap();
        drop(engine);
        let files = canonical_files(&source).unwrap();
        let manifest = generate_manifest(&source, &files).unwrap();
        (source, manifest)
    }

    fn archive(root: &Path, source: &Path, manifest: &BackupManifest) -> PathBuf {
        let path = root.join(format!("{}.zip", uuid::Uuid::new_v4()));
        let files: Vec<_> = manifest
            .files
            .iter()
            .map(|entry| source.join(&entry.name))
            .collect();
        create_archive(source, &files, manifest, &path).unwrap();
        path
    }

    async fn restore(path: PathBuf, target: &Path) -> Result<(), RestoreError> {
        restore_from_backup(RestoreOptions {
            backup_path: path,
            data_dir: target.to_path_buf(),
            force: true,
            no_stop_daemon: true,
        })
        .await
    }

    #[tokio::test]
    async fn containment_restore_rejects_each_missing_canonical_file_before_target_replacement() {
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"preserve").unwrap();
        for missing in [
            "meta.db",
            "edges.db",
            "fulltext.dat",
            "default/vectors.dat",
            "all",
        ] {
            let mut incomplete = manifest.clone();
            incomplete
                .files
                .retain(|f| missing != "all" && f.name != missing);
            incomplete.total_size_bytes = incomplete.files.iter().map(|f| f.size_bytes).sum();
            let path = archive(root.path(), &source, &incomplete);
            assert!(restore(path, &target).await.is_err(), "accepted {missing}");
            assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"preserve");
            assert!(!target.join("meta.db").exists());
        }
    }

    #[tokio::test]
    async fn selected_generation_rejects_self_consistent_legacy_readable_gate_before_restore() {
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        let selected = manifest
            .files
            .iter()
            .find(|f| crate::storage::metadata::text_generation::is_generation_filename(&f.name))
            .unwrap();
        let mut forged = fs::read(source.join(&selected.name)).unwrap();
        forged[4..6].copy_from_slice(&1u16.to_le_bytes());
        fs::write(source.join("fulltext.dat"), forged).unwrap();
        assert!(crate::storage::TextStore::open(&source.join("fulltext.dat")).is_ok());
        let metadata_before = fs::read(source.join("meta.db")).unwrap();
        assert!(canonical_files(&source).is_err());
        assert!(RedbStorageEngine::open(&source).is_err());
        assert_eq!(fs::read(source.join("meta.db")).unwrap(), metadata_before);
        // Recompute every manifest checksum/size: rejection must be semantic.
        let files: Vec<_> = manifest
            .files
            .iter()
            .map(|f| source.join(&f.name))
            .collect();
        let forged_manifest = generate_manifest(&source, &files).unwrap();
        let archive = archive(root.path(), &source, &forged_manifest);
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"preserve").unwrap();
        assert!(restore(archive, &target).await.is_err());
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"preserve");
    }

    #[tokio::test]
    async fn text_generation_restore_requires_selected_payload_even_if_manifest_omits_it() {
        let root = tempfile::tempdir().unwrap();
        let (source, mut manifest) = fixture(root.path());
        let selected = manifest
            .files
            .iter()
            .find(|entry| {
                crate::storage::metadata::text_generation::is_generation_filename(&entry.name)
            })
            .unwrap()
            .name
            .clone();
        manifest.files.retain(|entry| entry.name != selected);
        manifest.total_size_bytes = manifest.files.iter().map(|entry| entry.size_bytes).sum();
        let path = archive(root.path(), &source, &manifest);
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"old").unwrap();
        assert!(restore(path, &target).await.is_err());
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn containment_restore_round_trip_and_backup_validation_leave_source_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        let before: Vec<_> = manifest
            .files
            .iter()
            .map(|f| fs::read(source.join(&f.name)).unwrap())
            .collect();
        canonical_files(&source).unwrap();
        for (entry, bytes) in manifest.files.iter().zip(before) {
            assert_eq!(fs::read(source.join(&entry.name)).unwrap(), bytes);
        }
        let path = archive(root.path(), &source, &manifest);
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"old").unwrap();
        restore(path, &target).await.unwrap();
        let engine = RedbStorageEngine::open(&target).unwrap();
        let records = engine.scan_all().unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0].1;
        assert_eq!(
            engine
                .get_text(crate::storage::TextRef {
                    file_offset: record.text_offset,
                    length: record.text_length
                })
                .unwrap()
                .unwrap(),
            "retained text"
        );
        assert_eq!(
            engine
                .get_vector(
                    crate::model::NamespaceId::new(record.namespace_id),
                    record.vector_slot
                )
                .unwrap()
                .unwrap(),
            vec![1., 2., 3., 4.]
        );
        let retained = fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().starts_with("target.bak-"))
            .unwrap();
        assert_eq!(fs::read(retained.path().join("sentinel")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn containment_restore_rejects_zero_vectors_sizes_checksums_and_duplicate_manifest() {
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        let target = root.path().join("target");
        for mode in ["size", "checksum", "duplicate", "total", "zero"] {
            let mut bad = manifest.clone();
            match mode {
                "size" => {
                    bad.files[0].size_bytes += 1;
                    bad.total_size_bytes += 1;
                }
                "checksum" => bad.files[0].crc32 ^= 1,
                "duplicate" => {
                    bad.files.push(bad.files[0].clone());
                    bad.total_size_bytes += bad.files[0].size_bytes;
                }
                "total" => bad.total_size_bytes += 1,
                "zero" => {
                    let entry = bad
                        .files
                        .iter_mut()
                        .find(|f| f.name.ends_with("vectors.dat"))
                        .unwrap();
                    fs::write(source.join(&entry.name), []).unwrap();
                    bad.total_size_bytes -= entry.size_bytes;
                    entry.size_bytes = 0;
                    entry.crc32 = 0;
                }
                _ => unreachable!(),
            }
            // ZipWriter rejects duplicate file names, so a duplicate manifest
            // deliberately lists the same single archive member twice.
            let path = if mode == "duplicate" {
                let path = root.path().join("duplicate.zip");
                let files: Vec<_> = manifest
                    .files
                    .iter()
                    .map(|f| source.join(&f.name))
                    .collect();
                create_archive(&source, &files, &bad, &path).unwrap();
                path
            } else {
                archive(root.path(), &source, &bad)
            };
            assert!(restore(path, &target).await.is_err(), "accepted {mode}");
            assert!(!target.exists());
        }
    }

    #[tokio::test]
    async fn containment_backup_force_cannot_bypass_ownership_and_quiescent_archive_restores() {
        let root = tempfile::tempdir().unwrap();
        let (source, _) = fixture(root.path());
        let archive_path = root.path().join("backup.zip");
        let config = crate::config::RecalldConfig::default();
        let active = RedbStorageEngine::open(&source).unwrap();
        assert!(
            crate::backup::run_backup(&config, &archive_path, Some(&source), true)
                .await
                .is_err()
        );
        assert!(!archive_path.exists());
        drop(active);
        crate::backup::run_backup(&config, &archive_path, Some(&source), false)
            .await
            .unwrap();
        restore(archive_path, &root.path().join("restored"))
            .await
            .unwrap();
    }

    #[test]
    fn containment_archive_rejects_undeclared_paths_and_links() {
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        for name in [
            "unexpected",
            "../escape",
            "/absolute",
            "default/../vectors.dat",
        ] {
            let path = root.path().join("bad.zip");
            let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("backup_manifest.json", options).unwrap();
            writer
                .write_all(&serde_json::to_vec(&manifest).unwrap())
                .unwrap();
            writer.start_file(name, options).unwrap();
            writer.write_all(b"bad").unwrap();
            writer.finish().unwrap();
            assert!(extract_backup(&path, &root.path().join("stage"), &manifest).is_err());
        }
        let path = root.path().join("link.zip");
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        writer
            .add_symlink(
                "meta.db",
                "elsewhere",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.finish().unwrap();
        assert!(extract_backup(&path, &root.path().join("stage"), &manifest).is_err());
        assert!(source.exists());
    }

    #[test]
    fn containment_duplicate_zip_central_entries_cannot_hide_in_library_inventory() {
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        let path = archive(root.path(), &source, &manifest);
        let parsed = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let directory = parsed.central_directory_start() as usize;
        drop(parsed);
        let mut bytes = fs::read(&path).unwrap();
        let first_length = 46
            + [28, 30, 32]
                .into_iter()
                .map(|offset| {
                    u16::from_le_bytes([bytes[directory + offset], bytes[directory + offset + 1]])
                        as usize
                })
                .sum::<usize>();
        let duplicate = bytes[directory..directory + first_length].to_vec();
        let footer = bytes.len() - 22; // fixture has no archive comment
        assert_eq!(&bytes[footer..footer + 4], b"PK\x05\x06");
        bytes.splice(footer..footer, duplicate);
        let footer = bytes.len() - 22;
        for offset in [8, 10] {
            let count = u16::from_le_bytes(
                bytes[footer + offset..footer + offset + 2]
                    .try_into()
                    .unwrap(),
            ) + 1;
            bytes[footer + offset..footer + offset + 2].copy_from_slice(&count.to_le_bytes());
        }
        let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap())
            + first_length as u32;
        bytes[footer + 12..footer + 16].copy_from_slice(&size.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            ZipArchive::new(File::open(&path).unwrap()).unwrap().len(),
            manifest.files.len() + 1
        );
        assert!(extract_backup(&path, &root.path().join("stage"), &manifest).is_err());
    }

    #[tokio::test]
    async fn integration_restore_rejects_partial_or_future_control_before_replacing_target() {
        use crate::storage::control;
        for future in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (source, manifest) = fixture(root.path());
            let engine = RedbStorageEngine::open(&source).unwrap();
            let mut state = engine.control_state().unwrap();
            drop(engine);
            let db = redb::Database::open(source.join("meta.db")).unwrap();
            let tx = db.begin_write().unwrap();
            if future {
                state.control_schema_version = 2;
                tx.open_table(control::CONTROL)
                    .unwrap()
                    .insert("state", control::encode(&state).unwrap().as_slice())
                    .unwrap();
            } else {
                tx.delete_table(control::KEYS).unwrap();
            }
            tx.commit().unwrap();
            drop(db);
            let source_before = fs::read(source.join("meta.db")).unwrap();
            assert!(canonical_files(&source).is_err());
            assert_eq!(fs::read(source.join("meta.db")).unwrap(), source_before);
            // Use a checksum-valid archive to isolate semantic validation.
            let files: Vec<_> = manifest
                .files
                .iter()
                .map(|f| source.join(&f.name))
                .collect();
            let manifest = generate_manifest(&source, &files).unwrap();
            let archive = archive(root.path(), &source, &manifest);
            let target = root.path().join("target");
            fs::create_dir(&target).unwrap();
            fs::write(target.join("sentinel"), b"original target").unwrap();
            assert!(restore(archive, &target).await.is_err());
            assert_eq!(
                fs::read(target.join("sentinel")).unwrap(),
                b"original target"
            );
            assert!(!target.join("meta.db").exists());
        }
    }

    #[test]
    fn containment_restore_lock_child() {
        let Some(target) = std::env::var_os("RECALLD_TEST_LOCK_TARGET") else {
            return;
        };
        let _engine = RedbStorageEngine::open(Path::new(&target)).unwrap();
        fs::write(Path::new(&target).join("ready"), []).unwrap();
        let mut release = [0u8];
        io::stdin().read_exact(&mut release).unwrap();
    }

    #[tokio::test]
    async fn containment_restore_cannot_replace_another_process_even_with_force() {
        use std::process::{Command, Stdio};
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        let path = archive(root.path(), &source, &manifest);
        let target = root.path().join("target");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "backup::restore::tests::containment_restore_lock_child",
                "--nocapture",
            ])
            .env("RECALLD_TEST_LOCK_TARGET", &target)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !target.join("ready").exists() {
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                panic!("lock child did not start");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        for no_stop_daemon in [false, true] {
            let result = restore_from_backup(RestoreOptions {
                backup_path: path.clone(),
                data_dir: target.clone(),
                force: true,
                no_stop_daemon,
            })
            .await;
            assert!(matches!(
                result,
                Err(RestoreError::Storage(StorageError::DatabaseLocked))
            ));
        }
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(target.join("ready").exists());
    }

    #[test]
    fn containment_interrupted_restore_and_symlink_target_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let ownership = StoreOwnership::acquire(&target).unwrap();
        fs::write(&ownership.pending, b"staged and previous paths").unwrap();
        drop(ownership);
        for _ in 0..2 {
            assert!(matches!(
                RedbStorageEngine::open(&target),
                Err(StorageError::RecoveryRequired(_))
            ));
        }
        assert!(!target.exists());
        #[cfg(unix)]
        {
            let alias = root.path().join("alias");
            std::os::unix::fs::symlink(&target, &alias).unwrap();
            assert!(StoreOwnership::acquire(&alias).is_err());
        }
    }
}
