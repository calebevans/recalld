//! Engine-exclusive generation publication. No marker participates in selection.
use super::*;
use crate::storage::fsync::fsync_dir;
use crate::storage::text::generation::{cleanup_orphans, validate_rejection_gate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    EntriesWritten,
    FileSynced,
    DirectorySynced,
    Selected,
    Published,
    Cleanup,
    Unlinked,
}

impl RedbStorageEngine {
    // &mut self drains the engine's readers for the entire protocol. The shared
    // adapter's write guard supplies this exclusion; no cross-file atomic claim.
    pub(super) fn compact_text_at(
        &mut self,
        mut boundary: impl FnMut(Stage) -> Result<(), StorageError>,
    ) -> Result<CompactionResult, StorageError> {
        self.ensure_available()?;
        let old = self.meta_store.text_selection()?.ok_or_else(|| StorageError::RecoveryRequired(
            "legacy text compaction requires offline fresh-destination migration: recalld-text-migrate SOURCE DESTINATION".into()
        ))?;
        validate_rejection_gate(&self.db_path, old.store_id)?;
        let records = self.meta_store.scan_all()?;
        let refs: Vec<_> = records
            .iter()
            .filter(|(_, r)| r.text_offset != 0 || r.text_length != 0)
            .map(|(id, r)| {
                (
                    *id,
                    TextRef {
                        file_offset: r.text_offset,
                        length: r.text_length,
                    },
                )
            })
            .collect();
        // Validate retained extents BEFORE any migration writes or selection.
        for (_, reference) in &refs {
            self.text_store.read(*reference)?;
        }
        let state = self.control_state()?;
        let old_size = self.text_store.file_size();
        let old_count = self.text_store.complete_entry_count()?;
        let (mut prepared, id) = TextStore::create_generation(&self.db_path, state.store_id)?;
        let mut new_refs = Vec::with_capacity(refs.len());
        for (memory, reference) in &refs {
            let text = self.text_store.read(*reference)?;
            new_refs.push((*memory, prepared.append(&text)?));
        }
        boundary(Stage::EntriesWritten)?;
        prepared.sync_all()?;
        boundary(Stage::FileSynced)?;
        fsync_dir(&self.db_path)?;
        boundary(Stage::DirectorySynced)?;
        let descriptor = prepared.seal(state.store_id, id, 2)?;
        // Validate the exact prepared bytes before the one metadata transaction.
        let prepared = TextStore::open_generation(&self.db_path, &descriptor, true)?;
        for (_, reference) in &new_refs {
            prepared.read(*reference)?;
        }
        self.meta_store.select_text_generation(
            state.committed_generation,
            &descriptor,
            &new_refs,
        )?;
        boundary(Stage::Selected).inspect_err(|_| self.mark_unavailable())?;
        self.text_store = prepared; // infallible publication, while readers remain drained
        boundary(Stage::Published)?;
        let result = CompactionResult {
            old_size,
            new_size: self.text_store.file_size(),
            entries_removed: old_count.saturating_sub(refs.len()),
            entries_kept: refs.len(),
            new_refs,
        };
        // A cleanup error cannot turn a committed operation into a rolled-back error.
        // Unknown legacy copies are retained; this old selection has validated ownership.
        let cleanup = (|| -> Result<(), StorageError> {
            boundary(Stage::Cleanup)?;
            std::fs::remove_file(self.db_path.join(&old.filename))?;
            boundary(Stage::Unlinked)?;
            cleanup_orphans(&self.db_path, &descriptor)?;
            fsync_dir(&self.db_path)?;
            Ok(())
        })();
        if let Err(error) = cleanup {
            tracing::warn!(%error, "text generation committed; cleanup pending");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    fn generation_fixture(path: &Path) -> (RedbStorageEngine, Vec<(MemoryId, String)>) {
        let mut engine = RedbStorageEngine::open(path).unwrap();
        let mut config = NamespaceConfig::default_namespace(0);
        config.embedding_dim = 4;
        let ns = engine.create_namespace(&config).unwrap();
        let mut kept = Vec::new();
        for (index, text) in [
            "dead text preceding live entries",
            "first retained text",
            "résumé: 日本語",
        ]
        .into_iter()
        .enumerate()
        {
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
                summary: "fixture".into(),
                tags: vec![],
                access_history: vec![],
                text_offset: 0,
                text_length: 0,
            };
            engine
                .insert_memory(id, ns, &mut record, &[1.; 4], Some(text))
                .unwrap();
            if index == 0 {
                engine.delete_memory(id).unwrap();
            } else {
                kept.push((id, text.into()));
            }
            if index == 2 {
                engine
                    .update_decay_state(id, DecayPhase::Summary, 0.1, 0.1, 1., false)
                    .unwrap();
            }
        }
        (engine, kept)
    }

    fn assert_texts(engine: &RedbStorageEngine, kept: &[(MemoryId, String)]) {
        for (id, text) in kept {
            let record = engine.get_record(*id).unwrap().unwrap();
            assert_eq!(
                engine
                    .get_text(TextRef {
                        file_offset: record.text_offset,
                        length: record.text_length
                    })
                    .unwrap()
                    .as_ref(),
                Some(text)
            );
        }
    }

    #[test]
    fn compaction_interruption_child() {
        let Ok(path) = std::env::var("RECALLD_TEXT_CRASH_PATH") else {
            return;
        };
        let stop = std::env::var("RECALLD_TEXT_CRASH_STAGE").unwrap();
        let mut engine = RedbStorageEngine::open(path).unwrap();
        engine
            .compact_text_at(|stage| {
                if format!("{stage:?}") == stop {
                    std::process::exit(73);
                }
                Ok(())
            })
            .unwrap();
        panic!("boundary not reached");
    }

    #[test]
    fn compaction_process_interruption_matrix_reopens_metadata_selection_twice() {
        for stage in [
            Stage::EntriesWritten,
            Stage::FileSynced,
            Stage::DirectorySynced,
            Stage::Selected,
            Stage::Published,
            Stage::Cleanup,
            Stage::Unlinked,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (engine, kept) = generation_fixture(dir.path());
            let before = engine.control_state().unwrap().committed_generation.get();
            let original_offset = engine.get_record(kept[0].0).unwrap().unwrap().text_offset;
            drop(engine);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::engine::text_compaction::tests::compaction_interruption_child",
                    "--nocapture",
                ])
                .env("RECALLD_TEXT_CRASH_PATH", dir.path())
                .env("RECALLD_TEXT_CRASH_STAGE", format!("{stage:?}"))
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(73), "{stage:?}");
            for _ in 0..2 {
                let engine = RedbStorageEngine::open(dir.path()).unwrap();
                assert_texts(&engine, &kept);
                let delta = match stage {
                    Stage::Selected | Stage::Published | Stage::Cleanup | Stage::Unlinked => 1,
                    _ => 0,
                };
                assert_eq!(
                    engine.control_state().unwrap().committed_generation.get(),
                    before + delta
                );
                if delta == 1 {
                    assert_ne!(
                        engine.get_record(kept[0].0).unwrap().unwrap().text_offset,
                        original_offset
                    );
                    assert_eq!(
                        engine
                            .meta_store
                            .text_selection()
                            .unwrap()
                            .unwrap()
                            .format_version,
                        2
                    );
                }
            }
        }
    }

    #[test]
    fn compaction_repeated_empty_and_tail_append_preserve_receipts_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let (mut engine, kept) = generation_fixture(dir.path());
        let before = engine.control_state().unwrap().committed_generation.get();
        let first = engine.compact_text_log().unwrap();
        assert_eq!((first.entries_kept, first.entries_removed), (2, 1));
        assert!(first.new_size < first.old_size);
        let selected = engine.meta_store.text_selection().unwrap().unwrap();
        assert_eq!(
            engine.control_state().unwrap().committed_generation.get(),
            before + 1
        );
        assert!(
            TextStore::open(&dir.path().join("fulltext.dat")).is_err(),
            "old reader must reject upgraded layout"
        );
        let appended = engine.text_store.append("valid unreferenced tail").unwrap();
        engine.text_store.sync_data().unwrap();
        assert!(appended.file_offset >= selected.sealed_length);
        assert_texts(&engine, &kept);
        engine.compact_text_log().unwrap();
        assert_ne!(
            engine.meta_store.text_selection().unwrap().unwrap().id,
            selected.id
        );
        for (id, _) in &kept {
            engine.delete_memory(*id).unwrap();
        }
        let empty_before = engine.control_state().unwrap().committed_generation.get();
        assert_eq!(engine.compact_text_log().unwrap().entries_kept, 0);
        assert_eq!(
            engine.control_state().unwrap().committed_generation.get(),
            empty_before + 1
        );
        drop(engine);
        let engine = RedbStorageEngine::open(dir.path()).unwrap();
        assert_eq!(engine.text_store.file_size(), 48);
    }

    #[test]
    fn compaction_selected_missing_corrupt_header_prefix_and_extent_fail_closed() {
        for fault in ["missing", "header", "prefix", "extent"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut engine, kept) = generation_fixture(dir.path());
            engine.compact_text_log().unwrap();
            let selected = engine.meta_store.text_selection().unwrap().unwrap();
            drop(engine);
            if fault == "extent" {
                use redb::ReadableTable;
                let db = redb::Database::open(dir.path().join("meta.db")).unwrap();
                let tx = db.begin_write().unwrap();
                {
                    let mut table = tx.open_table(crate::storage::metadata::META_TABLE).unwrap();
                    let key = kept[0].0.as_bytes().as_slice();
                    let mut record =
                        DiskRecord::from_bytes(table.get(key).unwrap().unwrap().value()).unwrap();
                    record.text_offset = u64::MAX - 1;
                    table.insert(key, record.to_bytes().as_slice()).unwrap();
                }
                tx.commit().unwrap();
            }
            let path = dir.path().join(&selected.filename);
            if fault == "missing" {
                std::fs::remove_file(&path).unwrap();
            }
            if fault == "header" || fault == "prefix" {
                let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.seek(SeekFrom::Start(if fault == "header" { 32 } else { 56 }))
                    .unwrap();
                file.write_all(b"changed").unwrap();
                file.sync_all().unwrap();
            }
            // Unselected but valid data must never win over the broken selection.
            let (orphan, _) = TextStore::create_generation(dir.path(), selected.store_id).unwrap();
            let orphan_path = orphan.path().to_owned();
            orphan.sync_all().unwrap();
            drop(orphan);
            let metadata_before = std::fs::read(dir.path().join("meta.db")).unwrap();
            for _ in 0..2 {
                assert!(RedbStorageEngine::open(dir.path()).is_err(), "{fault}");
                assert_eq!(
                    std::fs::read(dir.path().join("meta.db")).unwrap(),
                    metadata_before,
                    "failed preflight mutated metadata: {fault}"
                );
            }
            assert!(
                orphan_path.exists(),
                "no cleanup before successful validation"
            );
        }
    }

    #[test]
    fn compaction_orphans_require_header_identity_and_cleanup_failure_is_committed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut engine, kept) = generation_fixture(dir.path());
        engine.compact_text_log().unwrap();
        let old = engine.meta_store.text_selection().unwrap().unwrap();
        let (foreign, _) = TextStore::create_generation(dir.path(), uuid::Uuid::new_v4()).unwrap();
        let foreign_path = foreign.path().to_owned();
        foreign.sync_all().unwrap();
        drop(foreign);
        let malformed = dir.path().join(
            crate::storage::metadata::text_generation::generation_filename(uuid::Uuid::new_v4()),
        );
        std::fs::write(&malformed, b"unowned bytes").unwrap();
        let (owned, _) = TextStore::create_generation(dir.path(), old.store_id).unwrap();
        let owned_path = owned.path().to_owned();
        owned.sync_all().unwrap();
        drop(owned);
        let symlink = dir.path().join(
            crate::storage::metadata::text_generation::generation_filename(uuid::Uuid::new_v4()),
        );
        std::os::unix::fs::symlink(&foreign_path, &symlink).unwrap();
        let result = engine
            .compact_text_at(|stage| {
                if stage == Stage::Cleanup {
                    Err(StorageError::RecoveryRequired(
                        "injected cleanup I/O error".into(),
                    ))
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(result.entries_kept, 2);
        assert!(dir.path().join(old.filename).exists());
        drop(engine);
        let engine = RedbStorageEngine::open(dir.path()).unwrap();
        assert_texts(&engine, &kept);
        assert!(!owned_path.exists());
        assert!(foreign_path.exists() && malformed.exists());
        assert!(
            std::fs::symlink_metadata(symlink)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn compaction_postcommit_publication_error_blocks_old_text_until_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (mut engine, kept) = generation_fixture(dir.path());
        let record = engine.get_record(kept[0].0).unwrap().unwrap();
        assert!(
            engine
                .compact_text_at(|stage| if stage == Stage::Selected {
                    Err(StorageError::RecoveryRequired(
                        "injected publication failure".into(),
                    ))
                } else {
                    Ok(())
                })
                .is_err()
        );
        assert!(engine.get_record(kept[0].0).is_err());
        assert!(engine.scan_all().is_err());
        assert!(
            engine
                .get_text(TextRef {
                    file_offset: record.text_offset,
                    length: record.text_length
                })
                .is_err()
        );
        assert!(engine.compact_text_log().is_err());
        drop(engine);
        assert_texts(&RedbStorageEngine::open(dir.path()).unwrap(), &kept);
    }
    #[test]
    fn compaction_unknown_commit_reconciles_receipt_without_reclaiming_selected_payload() {
        let dir = tempfile::tempdir().unwrap();
        let (mut engine, kept) = generation_fixture(dir.path());
        engine.compact_text_log().unwrap();
        let lineage = engine.control_state().unwrap().store_id;
        crate::storage::metadata::text_generation::FAIL_AFTER_TEXT_COMMIT
            .with(|flag| flag.set(true));
        let error = engine.compact_text_log().unwrap_err();
        let StorageError::CommitOutcomeUnknown { operation_id, .. } = error else {
            panic!("expected unknown commit");
        };
        assert!(engine.compact_text_log().is_err());
        assert!(engine.get_record(kept[0].0).is_err());
        assert!(engine.meta_store.count().is_err());
        drop(engine);
        let engine = RedbStorageEngine::open(dir.path()).unwrap();
        assert_texts(&engine, &kept);
        let receipt = engine
            .lookup_operation(lineage, MutationScope::Catalog, operation_id)
            .unwrap()
            .unwrap();
        let selected = engine.meta_store.text_selection().unwrap().unwrap();
        assert!(
            matches!(receipt.outcome, crate::storage::control::MutationOutcome::TextGenerationSelected { generation_id, .. } if generation_id == selected.id)
        );
    }
    #[test]
    fn compaction_header_identity_cannot_be_rebound_by_matching_prefix_hash() {
        let dir = tempfile::tempdir().unwrap();
        let engine = RedbStorageEngine::open(dir.path()).unwrap();
        let selected = engine.meta_store.text_selection().unwrap().unwrap();
        let mut rebound = selected.clone();
        rebound.id = uuid::Uuid::new_v4();
        rebound.filename =
            crate::storage::metadata::text_generation::generation_filename(rebound.id);
        std::fs::copy(
            dir.path().join(selected.filename),
            dir.path().join(&rebound.filename),
        )
        .unwrap();
        assert!(matches!(
            TextStore::open_generation(dir.path(), &rebound, false),
            Err(StorageError::OwnershipMismatch(_))
        ));
    }
}
