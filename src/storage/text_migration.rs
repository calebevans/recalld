//! Offline text-format migration into a fresh destination. Never edits the source.
use super::control::{Generation, WriteReceipt};
use super::fsync::fsync_dir;
use super::ownership::{StoreOwnership, lock_directory};
use super::text::generation::write_rejection_gate;
use super::{MetadataStore, StorageError, TextRef, TextStore};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMigrationReport {
    pub store_id: uuid::Uuid,
    pub source_generation: Generation,
    pub receipt: WriteReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Copied,
    PayloadSynced,
    Gated,
    Selected,
    Validated,
    Published,
}

/// Holds source and destination ownership for the full operation. The caller
/// must keep the old source offline when cutting over: both copies retain lineage.
pub fn migrate_legacy_text(
    source: &Path,
    destination: &Path,
) -> Result<TextMigrationReport, StorageError> {
    migrate_at(source, destination, |_| Ok(()))
}

fn migrate_at(
    source: &Path,
    destination: &Path,
    mut boundary: impl FnMut(Stage) -> Result<(), StorageError>,
) -> Result<TextMigrationReport, StorageError> {
    if !source.is_dir() {
        return Err(StorageError::InvalidPath);
    }
    let source_owner = StoreOwnership::acquire(source)?;
    let source = &source_owner.path;
    let _source_lock = lock_directory(source)?;
    let absolute_target = std::path::absolute(destination)?;
    let target_parent = absolute_target
        .parent()
        .ok_or(StorageError::InvalidPath)?
        .canonicalize()?;
    let resolved_target = target_parent.join(
        absolute_target
            .file_name()
            .ok_or(StorageError::InvalidPath)?,
    );
    if resolved_target.starts_with(source) || source.starts_with(&resolved_target) {
        return Err(StorageError::InvalidPath);
    }
    let target_owner = StoreOwnership::acquire(&resolved_target)?;
    let target = &target_owner.path;
    if target.try_exists()? || target.starts_with(source) || source.starts_with(target) {
        return Err(StorageError::InvalidMutation(
            "migration destination must be fresh and outside the source".into(),
        ));
    }
    let files = super::validation::canonical_files(source)?;
    {
        let (_copy, db) = super::validation::inspect_database(&source.join("meta.db"))?;
        if super::metadata::text_generation::read_selection(&db.begin_read()?)?.is_some() {
            return Err(StorageError::InvalidMutation(
                "source already has selected text; use ordinary generation compaction".into(),
            ));
        }
    }
    let parent = target.parent().ok_or(StorageError::InvalidPath)?;
    let stage = tempfile::Builder::new()
        .prefix(".recalld-text-migration-")
        .tempdir_in(parent)?;
    for file in files {
        let relative = file
            .strip_prefix(source)
            .map_err(|_| StorageError::InvalidPath)?;
        let copied = stage.path().join(relative);
        std::fs::create_dir_all(copied.parent().ok_or(StorageError::InvalidPath)?)?;
        std::fs::copy(file, &copied)?;
        std::fs::File::open(&copied)?.sync_all()?;
        fsync_dir(copied.parent().ok_or(StorageError::InvalidPath)?)?;
    }
    boundary(Stage::Copied)?;
    let receipt;
    let source_generation;
    let store_id;
    {
        // All writable opens and intermediate transactions are confined to staging.
        let metadata = MetadataStore::open(&stage.path().join("meta.db"))?;
        let state = metadata.control_state()?;
        source_generation = state.committed_generation;
        store_id = state.store_id;
        let old = TextStore::open_read_only(&stage.path().join("fulltext.dat"))?;
        let (mut prepared, id) = TextStore::create_generation(stage.path(), store_id)?;
        let mut relocated = Vec::new();
        for (memory, record) in metadata.scan_all()? {
            if record.text_offset != 0 || record.text_length != 0 {
                let text = old.read(TextRef {
                    file_offset: record.text_offset,
                    length: record.text_length,
                })?;
                relocated.push((memory, prepared.append(&text)?));
            }
        }
        let descriptor = prepared.seal(store_id, id, 2)?;
        boundary(Stage::PayloadSynced)?;
        // Old readers are fenced BEFORE the selection transaction. Any process
        // interruption here affects only an unservable staging directory.
        write_rejection_gate(stage.path(), &descriptor)?;
        boundary(Stage::Gated)?;
        receipt = metadata.select_text_generation(source_generation, &descriptor, &relocated)?;
        boundary(Stage::Selected)?;
    }
    for file in super::validation::canonical_files(stage.path())? {
        std::fs::File::open(&file)?.sync_all()?;
    }
    fsync_dir(stage.path())?;
    boundary(Stage::Validated)?;
    if target.try_exists()? {
        return Err(StorageError::InvalidMutation(
            "migration destination appeared during staging".into(),
        ));
    }
    // Destination is fresh, never the serving source. No old/new serving swap.
    std::fs::rename(stage.path(), target)?;
    fsync_dir(parent).map_err(|e| {
        StorageError::RecoveryRequired(format!(
            "migration published at {}; verify operation {} before retrying: {e}",
            target.display(),
            receipt.operation_id
        ))
    })?;
    boundary(Stage::Published)?;
    Ok(TextMigrationReport {
        store_id,
        source_generation,
        receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DecayPhase, DiskRecord, MemoryId, NamespaceConfig};
    use crate::storage::{EdgeStore, RedbStorageEngine, StorageEngine};

    fn legacy(path: &Path) -> (MemoryId, uuid::Uuid, Vec<u8>) {
        std::fs::create_dir_all(path).unwrap();
        drop(MetadataStore::open(&path.join("meta.db")).unwrap());
        drop(EdgeStore::open(&path.join("edges.db")).unwrap());
        drop(TextStore::open(&path.join("fulltext.dat")).unwrap());
        let mut engine = RedbStorageEngine::open(path).unwrap();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.embedding_dim = 4;
        let ns = engine.create_namespace(&ns).unwrap();
        let id = MemoryId::new();
        let mut r = DiskRecord {
            schema: None,
            version: 1,
            id: *id.as_bytes(),
            namespace_id: ns.get(),
            created_at: 0,
            last_accessed_at: 7,
            phase: DecayPhase::Summary,
            strength: 0.2,
            decay_strength: 0.2,
            stability: 1.,
            difficulty: 5.,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "retained".into(),
            tags: vec![],
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        };
        engine
            .insert_memory(id, ns, &mut r, &[1.; 4], Some("retained résumé"))
            .unwrap();
        let lineage = engine.control_state().unwrap().store_id;
        engine.sync().unwrap();
        drop(engine);
        (id, lineage, std::fs::read(path.join("meta.db")).unwrap())
    }

    #[test]
    fn legacy_in_place_compaction_refuses_without_selection_or_source_changes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let (id, _, _) = legacy(&source);
        let mut engine = RedbStorageEngine::open(&source).unwrap();
        let state = engine.control_state().unwrap();
        let text = std::fs::read(source.join("fulltext.dat")).unwrap();
        assert!(engine.compact_text_log().is_err());
        assert_eq!(engine.control_state().unwrap(), state);
        assert_eq!(std::fs::read(source.join("fulltext.dat")).unwrap(), text);
        let r = engine.get_record(id).unwrap().unwrap();
        assert_eq!(
            engine
                .get_text(TextRef {
                    file_offset: r.text_offset,
                    length: r.text_length
                })
                .unwrap()
                .unwrap(),
            "retained résumé"
        );
        drop(engine);
        // A legacy writer can still use this untouched source; no stale selection exists.
        assert!(TextStore::open(&source.join("fulltext.dat")).is_ok());
        let metadata = MetadataStore::open(&source.join("meta.db")).unwrap();
        assert!(metadata.text_selection().unwrap().is_none());
        drop(metadata);
        // Simulate the historical writer from the review: it ignores the
        // additive catalog, appends to fulltext.dat, then commits a legacy ref.
        let mut old_text = TextStore::open(&source.join("fulltext.dat")).unwrap();
        let appended = old_text
            .append("old writer tail remains reachable")
            .unwrap();
        old_text.sync_data().unwrap();
        drop(old_text);
        {
            use redb::ReadableTable;
            let db = redb::Database::open(source.join("meta.db")).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut table = tx.open_table(crate::storage::metadata::META_TABLE).unwrap();
                let key = id.as_bytes().as_slice();
                let mut record =
                    DiskRecord::from_bytes(table.get(key).unwrap().unwrap().value()).unwrap();
                record.text_offset = appended.file_offset;
                record.text_length = appended.length;
                table.insert(key, record.to_bytes().as_slice()).unwrap();
            }
            tx.commit().unwrap();
        }
        let engine = RedbStorageEngine::open(&source).unwrap();
        assert_eq!(
            engine.get_text(appended).unwrap().unwrap(),
            "old writer tail remains reachable"
        );
    }

    #[test]
    fn fresh_destination_migration_preserves_source_lineage_and_rejects_old_reader() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        let (id, lineage, before) = legacy(&source);
        let text_before = std::fs::read(source.join("fulltext.dat")).unwrap();
        let report = migrate_legacy_text(&source, &target).unwrap();
        assert_eq!(report.store_id, lineage);
        assert_eq!(
            report.receipt.committed_generation.get(),
            report.source_generation.get() + 1
        );
        assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
        assert_eq!(
            std::fs::read(source.join("fulltext.dat")).unwrap(),
            text_before
        );
        assert!(TextStore::open(&target.join("fulltext.dat")).is_err());
        for _ in 0..2 {
            let engine = RedbStorageEngine::open(&target).unwrap();
            let r = engine.get_record(id).unwrap().unwrap();
            assert_eq!(r.phase, DecayPhase::Summary);
            assert_eq!(r.last_accessed_at, 7);
            assert_eq!(
                engine
                    .get_text(TextRef {
                        file_offset: r.text_offset,
                        length: r.text_length
                    })
                    .unwrap()
                    .unwrap(),
                "retained résumé"
            );
        }
        assert!(migrate_legacy_text(&source, &target).is_err());
        assert!(migrate_legacy_text(&source, &source.join("child")).is_err());
    }

    #[test]
    fn migration_interruption_child() {
        let Ok(source) = std::env::var("RECALLD_MIGRATE_SOURCE") else {
            return;
        };
        let target = std::env::var("RECALLD_MIGRATE_TARGET").unwrap();
        let stop = std::env::var("RECALLD_MIGRATE_STAGE").unwrap();
        migrate_at(Path::new(&source), Path::new(&target), |stage| {
            if format!("{stage:?}") == stop {
                std::process::exit(74);
            }
            Ok(())
        })
        .unwrap();
        panic!("boundary not reached");
    }

    #[test]
    fn offline_migration_interruptions_never_publish_an_ungated_selection() {
        for stage in [
            Stage::Copied,
            Stage::PayloadSynced,
            Stage::Gated,
            Stage::Selected,
            Stage::Validated,
            Stage::Published,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let target = dir.path().join("target");
            let (_, _, before) = legacy(&source);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::text_migration::tests::migration_interruption_child",
                    "--nocapture",
                ])
                .env("RECALLD_MIGRATE_SOURCE", &source)
                .env("RECALLD_MIGRATE_TARGET", &target)
                .env("RECALLD_MIGRATE_STAGE", format!("{stage:?}"))
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(74));
            assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
            assert!(TextStore::open(&source.join("fulltext.dat")).is_ok());
            if stage == Stage::Published {
                assert!(TextStore::open(&target.join("fulltext.dat")).is_err());
                for _ in 0..2 {
                    drop(RedbStorageEngine::open(&target).unwrap());
                }
            } else {
                assert!(!target.exists());
                migrate_legacy_text(&source, &target).unwrap();
            }
        }
    }
}
