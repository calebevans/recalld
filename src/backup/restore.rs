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
    /// Explicit fresh-destination historical inspection; ordinary serving is blocked.
    pub historical: bool,
}

// ── Main entry point ────────────────────────────────────────────────

/// Restore Recalld data from a backup zip archive.
pub async fn restore_from_backup(opts: RestoreOptions) -> Result<(), RestoreError> {
    restore_at(opts, |_| Ok(())).await
}

async fn restore_at(
    opts: RestoreOptions,
    boundary: impl FnMut(crate::storage::restore_journal::Stage) -> Result<(), StorageError>,
) -> Result<(), RestoreError> {
    use crate::storage::{
        lineage::RestoreMode,
        metadata::restoration,
        restore_journal::{self, RestoreJournal},
    };
    if !opts.backup_path.exists() {
        return Err(RestoreError::BackupNotFound(
            opts.backup_path.display().to_string(),
        ));
    }
    let ownership = StoreOwnership::acquire(&opts.data_dir)?;
    let target = &ownership.path;
    let parent = target.parent().ok_or(StorageError::InvalidPath)?;
    if opts.historical && target.try_exists()? {
        return Err(RestoreError::InvalidArchive(
            "historical restore requires a fresh destination".into(),
        ));
    }
    if !opts.historical && !target.try_exists()? {
        return Err(RestoreError::InvalidArchive("retained-state restore requires an existing target; use explicit historical mode for a fresh destination".into()));
    }
    let _old_lock = if target.exists() {
        Some(lock_directory(target)?)
    } else {
        None
    };
    let captured = if target.exists() {
        Some(restoration::capture(target)?)
    } else {
        None
    };
    // Pin one private archive copy: manifest, bytes and provenance hash must
    // describe the same input even if the source path changes during restore.
    let mut archive = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut File::open(&opts.backup_path)?, archive.as_file_mut())?;
    archive.as_file().sync_all()?;
    let archive_sha256 = restore_journal::hash_file(archive.path())?;
    let manifest = read_manifest(archive.path())?;
    validate_manifest(&manifest)?;
    if !opts.force {
        request_confirmation(&opts, &manifest)?;
    }
    let stage = tempfile::Builder::new()
        .prefix(".recalld-restore-")
        .tempdir_in(parent)?;
    extract_backup(archive.path(), stage.path(), &manifest)?;
    if stage.path().join("fts.db").exists() {
        fs::remove_file(stage.path().join("fts.db"))?;
    }
    let _new_lock = lock_directory(stage.path())?;
    let snapshot = restoration::capture(stage.path())?;
    if captured
        .as_ref()
        .is_some_and(|t| !restoration::compatible(&snapshot, t))
    {
        return Err(StorageError::StoreLineageMismatch.into());
    }
    let operation = uuid::Uuid::new_v4();
    let new_store_id = uuid::Uuid::new_v4();
    let mode = if opts.historical {
        RestoreMode::Historical
    } else {
        RestoreMode::Serving
    };
    restoration::reconcile(
        stage.path(),
        &snapshot,
        captured.as_ref(),
        mode,
        operation,
        new_store_id,
        archive_sha256,
    )?;
    let journal = RestoreJournal::prepared(
        &ownership,
        stage.path().to_owned(),
        captured,
        new_store_id,
        operation,
        archive_sha256,
    )?;
    // Keep the fully validated stage before journal publication. Once the marker
    // is durable, every failure preserves both captured evidence and copies.
    let _staged = stage.keep();
    restore_journal::publish(&ownership, journal, boundary)?;
    tracing::info!(%new_store_id, historical = opts.historical, "restore published; later deletion coverage cannot be inferred from missing copies");
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
    use redb::ReadableTable;

    fn fixture(root: &Path) -> (PathBuf, BackupManifest) {
        fixture_with_embedding(root, &[1., 2., 3., 4.])
    }

    pub(super) fn fixture_with_embedding(
        root: &Path,
        embedding: &[f32],
    ) -> (PathBuf, BackupManifest) {
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
            .insert_memory(id, ns, &mut record, embedding, Some("retained text"))
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
            historical: false,
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
            assert!(
                extract_backup(
                    &path,
                    &root.path().join(format!("stage-{missing}")),
                    &incomplete
                )
                .is_err()
            );
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
        assert!(extract_backup(&archive, &root.path().join("stage"), &forged_manifest).is_err());
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
        assert!(extract_backup(&path, &root.path().join("stage"), &manifest).is_err());
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
        for entry in &manifest.files {
            let dest = target.join(&entry.name);
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(source.join(&entry.name), dest).unwrap();
        }
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
            let mut opts = retained_options(&path, &target);
            opts.historical = true;
            assert!(restore_from_backup(opts).await.is_err(), "accepted {mode}");
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
        restore(archive_path, &source).await.unwrap();
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
                historical: false,
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
    fn retained_fixture(
        root: &Path,
    ) -> (
        PathBuf,
        PathBuf,
        MemoryId,
        MemoryId,
        MemoryId,
        crate::storage::control::WriteReceipt,
    ) {
        use crate::model::{AccessKind, Tag};
        use crate::storage::control::{MutationContext, MutationScope};
        let (source, _) = fixture(root);
        let mut engine = RedbStorageEngine::open(&source).unwrap();
        let a = engine.scan_all().unwrap()[0].0;
        let original = engine.get_record(a).unwrap().unwrap();
        let ns = crate::model::NamespaceId::new(original.namespace_id);
        let mut add = |summary: &str| {
            let id = MemoryId::new();
            let mut record = original.clone();
            record.id = *id.as_bytes();
            record.summary = summary.into();
            record.tags = vec![Tag::new("retained-tag").unwrap()];
            record.text_offset = 0;
            record.text_length = 0;
            engine
                .insert_memory(
                    id,
                    ns,
                    &mut record,
                    &[1., 2., 3., 4.],
                    Some("retained payload"),
                )
                .unwrap();
            id
        };
        let b = add("summary to remove");
        let c = add("survivor");
        engine
            .add_edge(c, a, crate::model::EdgeType::Associative, 1., false, 0)
            .unwrap();
        engine.update_edge_count(c, 1).unwrap();
        let state = engine.control_state().unwrap();
        let ctx = MutationContext {
            store_id: Some(state.store_id),
            scope: MutationScope::Namespace(ns),
            idempotency_key: Some("before-restore".into()),
            expected_generation: None,
            contract_version: 1,
        };
        let receipt = engine
            .meta_store()
            .update_access_with_receipt(&ctx, c, 100, AccessKind::DirectRetrieval)
            .unwrap();
        engine.sync().unwrap();
        drop(engine);
        let files = canonical_files(&source).unwrap();
        let manifest = generate_manifest(&source, &files).unwrap();
        let backup = archive(root, &source, &manifest);
        let mut engine = RedbStorageEngine::open(&source).unwrap();
        engine.delete_memory(a).unwrap();
        let nsconfig = engine.get_namespace(ns).unwrap().unwrap();
        let b_record = engine.get_record(b).unwrap().unwrap();
        engine
            .apply_legacy_decay(b, &b_record, &nsconfig, DecayPhase::Summary, 0.6, 0.6)
            .unwrap();
        let b_record = engine.get_record(b).unwrap().unwrap();
        engine
            .apply_legacy_decay(b, &b_record, &nsconfig, DecayPhase::Ghost, 0.1, 0.1)
            .unwrap();
        engine.sync().unwrap();
        drop(engine);
        (source, backup, a, b, c, receipt)
    }

    pub(super) fn retained_options(backup: &Path, target: &Path) -> RestoreOptions {
        RestoreOptions {
            backup_path: backup.to_owned(),
            data_dir: target.to_owned(),
            force: true,
            no_stop_daemon: true,
            historical: false,
        }
    }

    #[tokio::test]
    async fn retained_restore_applies_newer_removals_forks_lineage_and_preserves_historical_receipts()
     {
        use crate::model::AccessKind;
        use crate::storage::control::{MutationContext, MutationScope};
        let root = tempfile::tempdir().unwrap();
        let (target, backup, removed, ghost, live, old_receipt) = retained_fixture(root.path());
        let before = crate::storage::metadata::restoration::capture(&target).unwrap();
        restore_from_backup(retained_options(&backup, &target))
            .await
            .unwrap();
        let mut engine = RedbStorageEngine::open(&target).unwrap();
        let state = engine.control_state().unwrap();
        assert_ne!(state.store_id, before.control.store_id);
        assert_eq!(state.committed_generation.get(), 1);
        assert!(engine.get_record(removed).unwrap().is_none());
        let r = engine.get_record(ghost).unwrap().unwrap();
        assert_eq!(r.phase, DecayPhase::Ghost);
        assert!(r.summary.is_empty());
        assert_eq!((r.text_offset, r.text_length), (0, 0));
        assert_eq!(r.tags.len(), 1, "v1 retained tags must survive");
        assert_eq!(engine.get_record(live).unwrap().unwrap().edge_count, 0);
        assert!(engine.edge_store().load_all_edges().unwrap().is_empty());
        let ns = crate::model::NamespaceId::new(r.namespace_id);
        let fts = crate::search::FtsIndex::new(&target).unwrap();
        assert_eq!(fts.search(ns, "survivor", 10).unwrap()[0].0, live);
        assert!(fts.search(ns, "summary to remove", 10).unwrap().is_empty());
        assert!(fts.search(ns, "text", 10).unwrap().is_empty());
        drop(fts);
        let stale = MutationContext {
            store_id: Some(old_receipt.store_id),
            scope: MutationScope::Namespace(ns),
            idempotency_key: Some("before-restore".into()),
            expected_generation: None,
            contract_version: 1,
        };
        assert!(matches!(
            engine.meta_store().update_access_with_receipt(
                &stale,
                live,
                200,
                AccessKind::DirectRetrieval
            ),
            Err(StorageError::StoreLineageMismatch)
        ));
        assert!(
            engine
                .meta_store()
                .lookup_operation(
                    state.store_id,
                    MutationScope::Namespace(ns),
                    old_receipt.operation_id
                )
                .unwrap()
                .is_none()
        );
        let fresh = MutationContext {
            store_id: Some(state.store_id),
            ..stale
        };
        let new_receipt = engine
            .meta_store()
            .update_access_with_receipt(&fresh, live, 200, AccessKind::DirectRetrieval)
            .unwrap();
        assert!(!new_receipt.replayed);
        assert_ne!(new_receipt.operation_id, old_receipt.operation_id);
        engine.sync().unwrap();
        drop(engine);
        let capture = crate::storage::metadata::restoration::capture(&target).unwrap();
        assert_eq!(capture.ledger.entries.len(), before.ledger.entries.len());
        assert!(capture.receipts.iter().any(|(_, bytes)| {
            crate::storage::control::validate_receipt(bytes)
                .unwrap()
                .receipt
                .operation_id
                == old_receipt.operation_id
        }));
        assert!(
            capture
                .lineage
                .as_ref()
                .unwrap()
                .permits(before.control.generation_ref())
        );
        // Repeat from the original ancestor snapshot: evidence from both origins
        // remains, and ancestor receipts never become active retry keys.
        restore_from_backup(retained_options(&backup, &target))
            .await
            .unwrap();
        let reopened = RedbStorageEngine::open(&target).unwrap();
        assert_ne!(reopened.control_state().unwrap().store_id, state.store_id);
        assert!(reopened.get_record(removed).unwrap().is_none());
        assert!(
            reopened
                .get_record(ghost)
                .unwrap()
                .unwrap()
                .summary
                .is_empty()
        );
    }

    #[tokio::test]
    async fn restore_journal_restart_finishes_every_durable_publication_boundary() {
        use crate::storage::restore_journal::Stage;
        for stop in [
            Stage::Prepared,
            Stage::OldMoved,
            Stage::NewInstalled,
            Stage::Complete,
        ] {
            let root = tempfile::tempdir().unwrap();
            let (target, backup, removed, ghost, _, receipt) = retained_fixture(root.path());
            let outcome = restore_at(retained_options(&backup, &target), |stage| {
                if std::mem::discriminant(&stage) == std::mem::discriminant(&stop) {
                    Err(StorageError::RecoveryRequired(
                        "injected publication interruption".into(),
                    ))
                } else {
                    Ok(())
                }
            })
            .await;
            assert!(outcome.is_err());
            let pending = root.path().join("source.recalld-restore-pending");
            let mut journal: serde_json::Value =
                serde_json::from_slice(&fs::read(&pending).unwrap()).unwrap();
            assert!(
                journal["captured"]["ledger"]["entries"]
                    .as_array()
                    .unwrap()
                    .len()
                    >= 2
            );
            // A rename can reach disk before its state update. Recovery must
            // inspect identities, not trust the marker's state alone.
            journal["state"] = "prepared".into();
            fs::write(&pending, serde_json::to_vec(&journal).unwrap()).unwrap();
            let journal_before = fs::read(&pending).unwrap();
            assert!(
                StoreOwnership::acquire(&target).is_err(),
                "inspection cannot recover/publish a pending restore"
            );
            assert_eq!(fs::read(&pending).unwrap(), journal_before);
            let engine = RedbStorageEngine::open(&target).unwrap();
            assert_ne!(engine.control_state().unwrap().store_id, receipt.store_id);
            assert!(engine.get_record(removed).unwrap().is_none());
            assert!(
                engine
                    .get_record(ghost)
                    .unwrap()
                    .unwrap()
                    .summary
                    .is_empty()
            );
            assert!(!pending.exists());
            drop(engine);
            let again = RedbStorageEngine::open(&target).unwrap();
            assert!(again.get_record(removed).unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn missing_staging_after_old_move_restores_retained_target_without_resurrection() {
        use crate::storage::restore_journal::Stage;
        let root = tempfile::tempdir().unwrap();
        let (target, backup, removed, _, _, receipt) = retained_fixture(root.path());
        assert!(
            restore_at(retained_options(&backup, &target), |stage| {
                if matches!(stage, Stage::OldMoved) {
                    Err(StorageError::RecoveryRequired("stop".into()))
                } else {
                    Ok(())
                }
            })
            .await
            .is_err()
        );
        let pending = root.path().join("source.recalld-restore-pending");
        let journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&pending).unwrap()).unwrap();
        fs::remove_dir_all(journal["staged"].as_str().unwrap()).unwrap();
        assert!(RedbStorageEngine::open(&target).is_err()); // reports rollback
        assert!(!pending.exists());
        let original = RedbStorageEngine::open(&target).unwrap();
        assert_eq!(original.control_state().unwrap().store_id, receipt.store_id);
        assert!(original.get_record(removed).unwrap().is_none());
    }

    #[tokio::test]
    async fn unrelated_or_unreadable_target_never_downgrades_to_historical_restore() {
        let root = tempfile::tempdir().unwrap();
        let (target, backup, removed, _, _, _) = retained_fixture(root.path());
        let other_root = tempfile::tempdir().unwrap();
        let (other, _) = fixture(other_root.path());
        let bytes = fs::read(other.join("meta.db")).unwrap();
        assert!(
            restore_from_backup(retained_options(&backup, &other))
                .await
                .is_err()
        );
        assert_eq!(fs::read(other.join("meta.db")).unwrap(), bytes);
        let db = redb::Database::open(target.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        tx.delete_table(crate::storage::metadata::removal_ledger::LEDGER)
            .unwrap();
        tx.commit().unwrap();
        drop(db);
        let bytes = fs::read(target.join("meta.db")).unwrap();
        assert!(
            restore_from_backup(retained_options(&backup, &target))
                .await
                .is_err()
        );
        assert_eq!(fs::read(target.join("meta.db")).unwrap(), bytes);
        assert!(!root.path().join("source.recalld-restore-pending").exists());
        assert!(!bytes.is_empty());
        let _ = removed;
    }

    #[tokio::test]
    async fn retained_restore_terminal_ledger_survives_tombstone_and_namespace_purge() {
        for purge in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (target, manifest) = fixture(root.path());
            let backup = archive(root.path(), &target, &manifest);
            let mut engine = RedbStorageEngine::open(&target).unwrap();
            let (id, original) = engine.scan_all().unwrap().remove(0);
            let ns = crate::model::NamespaceId::new(original.namespace_id);
            engine.tombstone_memory(id).unwrap();
            if purge {
                engine.purge_namespace(ns).unwrap();
            }
            engine.sync().unwrap();
            drop(engine);
            restore_from_backup(retained_options(&backup, &target))
                .await
                .unwrap();
            let mut engine = RedbStorageEngine::open(&target).unwrap();
            if purge {
                assert!(engine.get_record(id).unwrap().is_none());
            } else {
                let record = engine.get_record(id).unwrap().unwrap();
                assert_eq!(record.phase, DecayPhase::Tombstone);
                assert!(record.summary.is_empty() && record.tags.is_empty());
                assert_eq!((record.text_offset, record.text_length), (0, 0));
            }
            assert!(
                engine
                    .vector_manager()
                    .get(ns)
                    .unwrap()
                    .is_free(original.vector_slot)
            );
            let fts = crate::search::FtsIndex::new(&target).unwrap();
            assert!(fts.search(ns, "backup fixture", 10).unwrap().is_empty());
            let mut reused = original;
            assert!(
                engine
                    .insert_memory(id, ns, &mut reused, &[1.; 4], Some("resurrect"))
                    .is_err()
            );
            let snapshot = engine.meta_store().control_state().unwrap();
            assert_eq!(
                snapshot.committed_generation.get(),
                1,
                "rejected UUID reuse cannot write"
            );
        }
    }

    #[tokio::test]
    async fn restore_recovery_rejects_unknown_journal_lineage_and_changed_staging() {
        use crate::storage::restore_journal::Stage;
        for bad in ["version", "path", "captured", "lineage", "fts"] {
            let root = tempfile::tempdir().unwrap();
            let (target, backup, _, _, _, _) = retained_fixture(root.path());
            assert!(
                restore_at(retained_options(&backup, &target), |stage| {
                    if matches!(stage, Stage::Prepared) {
                        Err(StorageError::RecoveryRequired("stop before rename".into()))
                    } else {
                        Ok(())
                    }
                })
                .await
                .is_err()
            );
            let before = fs::read(target.join("meta.db")).unwrap();
            let pending = root.path().join("source.recalld-restore-pending");
            let mut journal: serde_json::Value =
                serde_json::from_slice(&fs::read(&pending).unwrap()).unwrap();
            let stage = PathBuf::from(journal["staged"].as_str().unwrap());
            match bad {
                "version" => journal["version"] = 99.into(),
                "path" => {
                    journal["staged"] = root
                        .path()
                        .join("unrelated")
                        .to_string_lossy()
                        .as_ref()
                        .into()
                }
                "captured" => journal["captured"]["ledger"]["entries"] = serde_json::json!([]),
                "lineage" => {
                    let db = redb::Database::open(stage.join("meta.db")).unwrap();
                    let tx = db.begin_write().unwrap();
                    {
                        let mut table = tx.open_table(crate::storage::lineage::LINEAGE).unwrap();
                        let mut lineage: serde_json::Value =
                            serde_json::from_slice(table.get("state").unwrap().unwrap().value())
                                .unwrap();
                        lineage["version"] = 99.into();
                        table
                            .insert("state", serde_json::to_vec(&lineage).unwrap().as_slice())
                            .unwrap();
                    }
                    tx.commit().unwrap();
                }
                "fts" => fs::write(stage.join("fts.db"), b"changed after journal").unwrap(),
                _ => unreachable!(),
            }
            fs::write(&pending, serde_json::to_vec(&journal).unwrap()).unwrap();
            assert!(RedbStorageEngine::open(&target).is_err(), "accepted {bad}");
            assert_eq!(fs::read(target.join("meta.db")).unwrap(), before);
            assert!(pending.exists() && stage.exists());
        }
    }

    #[tokio::test]
    async fn retained_restore_refuses_legacy_in_place_and_accepts_explicit_fresh_text_migration() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("legacy");
        fs::create_dir(&legacy).unwrap();
        drop(crate::storage::MetadataStore::open(&legacy.join("meta.db")).unwrap());
        drop(crate::storage::EdgeStore::open(&legacy.join("edges.db")).unwrap());
        drop(crate::storage::TextStore::open(&legacy.join("fulltext.dat")).unwrap());
        let before = fs::read(legacy.join("meta.db")).unwrap();
        let migrated = root.path().join("migrated");
        crate::storage::text_migration::migrate_legacy_text(&legacy, &migrated).unwrap();
        let files = canonical_files(&migrated).unwrap();
        let manifest = generate_manifest(&migrated, &files).unwrap();
        let backup = archive(root.path(), &migrated, &manifest);
        let error = restore_from_backup(retained_options(&backup, &legacy))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("old-reader-fenced"));
        assert_eq!(fs::read(legacy.join("meta.db")).unwrap(), before);
        assert!(crate::storage::TextStore::open(&legacy.join("fulltext.dat")).is_ok());
        restore_from_backup(retained_options(&backup, &migrated))
            .await
            .unwrap();
        assert!(RedbStorageEngine::open(&migrated).is_ok());
        assert!(crate::storage::TextStore::open(&migrated.join("fulltext.dat")).is_err());
    }

    #[tokio::test]
    async fn explicit_historical_restore_is_fresh_gated_and_excluded_from_writes_and_serving() {
        let root = tempfile::tempdir().unwrap();
        let (source, manifest) = fixture(root.path());
        let backup = archive(root.path(), &source, &manifest);
        let historical = root.path().join("historical");
        assert!(
            restore_from_backup(retained_options(&backup, &historical))
                .await
                .is_err()
        );
        let mut opts = retained_options(&backup, &historical);
        opts.historical = true;
        restore_from_backup(opts.clone()).await.unwrap();
        assert!(
            restore_from_backup(opts).await.is_err(),
            "historical destination cannot overwrite an existing target"
        );
        assert!(RedbStorageEngine::open(&historical).is_err());
        assert!(
            crate::storage::TextStore::open(&historical.join("fulltext.dat")).is_err(),
            "old reader must reject the real fence"
        );
        let captured = crate::storage::metadata::restoration::capture(&historical).unwrap();
        assert_eq!(
            captured.lineage.unwrap().mode,
            crate::storage::lineage::RestoreMode::Historical
        );
        let before = fs::read(historical.join("meta.db")).unwrap();
        assert!(crate::storage::MetadataStore::open(&historical.join("meta.db")).is_err());
        assert_eq!(fs::read(historical.join("meta.db")).unwrap(), before);
        assert!(
            restore_from_backup(retained_options(&backup, &historical))
                .await
                .is_err(),
            "restoration cannot implicitly promote historical mode"
        );
    }
}

#[cfg(test)]
#[path = "schema_restore_tests.rs"]
mod schema_tests;
