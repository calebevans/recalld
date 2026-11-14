//! Real purge, migration, logical transfer and restore consumers on synthetic stores.
use crate::backup::{
    archive::create_archive,
    manifest::generate_manifest,
    restore::{RestoreOptions, restore_from_backup},
};
use crate::model::{MemoryId, NamespaceConfig, NamespaceId};
use crate::storage::*;
use control::MutationContext;
use metadata::restoration::capture;
use std::path::{Path, PathBuf};

const FORGOTTEN: &str = "FORGOTTEN-PURGE-PAYLOAD-593ab";

fn archive(source: &Path, path: &Path) {
    let files = validation::canonical_files(source).unwrap();
    let manifest = generate_manifest(source, &files).unwrap();
    create_archive(source, &files, &manifest, path).unwrap();
}
fn options(backup: &Path, target: &Path, historical: bool) -> RestoreOptions {
    RestoreOptions {
        backup_path: backup.into(),
        data_dir: target.into(),
        force: true,
        no_stop_daemon: true,
        historical,
    }
}
fn setup(root: &Path) -> (PathBuf, NamespaceId, MemoryId, MemoryId) {
    let source = root.join("source");
    let mut engine = RedbStorageEngine::open(&source).unwrap();
    let mut config = NamespaceConfig::default_namespace(0);
    config.embedding_dim = 4;
    let live_namespace = engine.create_namespace(&config).unwrap();
    config.name = "retired".into();
    let retired = engine.create_namespace(&config).unwrap();
    let mut ids = Vec::new();
    for (namespace, text) in [(live_namespace, "retained résumé"), (retired, FORGOTTEN)] {
        let receipt = engine
            .insert_memory_with_receipt(
                &MutationContext::unkeyed(namespace),
                &insertion::InsertMemory {
                    id: None,
                    summary: text.into(),
                    full_text: Some(text.into()),
                    tags: vec![crate::model::Tag::new("entity/retained").unwrap()],
                    embedding: None,
                    initial_stability: Some(3.25),
                    created_at: Some(100),
                    parent_id: None,
                    supersedes: None,
                },
                &[1., -0., 0., 0.],
                100,
            )
            .unwrap();
        ids.push(insertion::inserted_id(&receipt).unwrap());
    }
    engine.sync().unwrap();
    drop(engine);
    (source, retired, ids[0], ids[1])
}
fn complete(source: &Path, namespace: NamespaceId) -> purge::PurgeProgress {
    let mut engine = RedbStorageEngine::open(source).unwrap();
    engine.purge_namespace(namespace).unwrap();
    let progress = engine.meta_store().purge_progress().unwrap().remove(0);
    assert_eq!(progress.stage, purge::PurgeStage::Complete);
    engine.sync().unwrap();
    progress
}
fn migrate(source: &Path, destination: &Path) {
    let state = schema_migration::inspect_schema(source).unwrap();
    schema_migration::migrate_schema(
        source,
        destination,
        &schema_migration::MigrationOptions {
            migration_now: 200,
            expected_source: state.control.generation_ref(),
            attestations: vec![],
        },
    )
    .unwrap();
}

#[tokio::test]
async fn purge_offline_target_history_reconciles_catalog_and_ledger_across_repeated_forks() {
    let root = tempfile::tempdir().unwrap();
    let (source, namespace, live, forgotten) = setup(root.path());
    let backup = root.path().join("before.zip");
    archive(&source, &backup);
    let progress = complete(&source, namespace);
    let original = capture(&source).unwrap();
    let original_rows = original.purge_progress.clone();
    let archive_before = std::fs::read(&backup).unwrap();
    for _ in 0..2 {
        restore_from_backup(options(&backup, &source, false))
            .await
            .unwrap();
        let now = capture(&source).unwrap();
        assert_ne!(now.control.store_id, progress.store_id);
        assert_eq!(now.purge_progress, original_rows);
        assert!(
            original
                .receipts
                .iter()
                .all(|row| now.receipts.contains(row))
        );
        assert!(now.namespace_counter >= namespace.get());
        let engine = RedbStorageEngine::open(&source).unwrap();
        assert!(engine.get_record(forgotten).unwrap().is_none());
        assert!(engine.get_record(live).unwrap().is_some());
        assert!(engine.get_namespace(namespace).unwrap().is_none());
        assert!(!source.join("retired").exists());
        assert_eq!(
            engine.meta_store().purge_progress().unwrap(),
            vec![progress.clone()]
        );
        drop(engine);
    }
    assert_eq!(std::fs::read(&backup).unwrap(), archive_before);
    let mut engine = RedbStorageEngine::open(&source).unwrap();
    let mut config = NamespaceConfig::default_namespace(0);
    config.name = "retired".into();
    config.embedding_dim = 4;
    assert!(engine.create_namespace(&config).unwrap().get() > namespace.get());
}

#[tokio::test]
async fn purge_offline_schema_export_import_and_manifest_restore_keep_exact_completed_history() {
    let root = tempfile::tempdir().unwrap();
    let (source, namespace, live, forgotten) = setup(root.path());
    complete(&source, namespace);
    let original = capture(&source).unwrap();
    let before = std::fs::read(source.join("meta.db")).unwrap();
    let canonical = root.path().join("canonical");
    migrate(&source, &canonical);
    assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
    assert_eq!(
        capture(&canonical).unwrap().purge_progress,
        original.purge_progress
    );
    let stream = root.path().join("export.jsonl");
    let exported = logical_export::export_store(&canonical, &stream).unwrap();
    assert_eq!(exported.records, 1);
    assert!(
        !std::fs::read_to_string(&stream)
            .unwrap()
            .contains(FORGOTTEN)
    );
    let imported = root.path().join("imported");
    assert_eq!(
        logical_export::import_store(&stream, &imported).unwrap(),
        exported
    );
    let second = root.path().join("second.jsonl");
    assert_eq!(
        logical_export::export_store(&imported, &second)
            .unwrap()
            .logical_sha256,
        exported.logical_sha256
    );
    assert_eq!(
        capture(&imported).unwrap().purge_progress,
        original.purge_progress
    );
    let snapshot = offline_snapshot::OfflineSnapshot::open(&imported).unwrap();
    assert!(snapshot.record(forgotten).unwrap().is_none());
    let item = snapshot.record(live).unwrap().unwrap();
    assert_eq!(item.full_text.as_deref(), Some("retained résumé"));
    assert_eq!(
        item.embedding
            .unwrap()
            .iter()
            .map(|f| f.to_bits())
            .collect::<Vec<_>>(),
        vec![1f32.to_bits(), (-0f32).to_bits(), 0, 0]
    );
    assert_eq!(
        item.record.schema.unwrap().labels.provenance,
        crate::model::schema::LabelProvenance::LegacyRetainedOnly
    );
    drop(snapshot);
    let backup = root.path().join("v2.zip");
    let files = validation::canonical_files(&imported).unwrap();
    let manifest = generate_manifest(&imported, &files).unwrap();
    assert_eq!(manifest.backup_format_version, 2);
    create_archive(&imported, &files, &manifest, &backup).unwrap();
    let restored = root.path().join("historical");
    restore_from_backup(options(&backup, &restored, true))
        .await
        .unwrap();
    let restored_capture = capture(&restored).unwrap();
    assert_eq!(restored_capture.purge_progress, original.purge_progress);
    assert!(
        original
            .receipts
            .iter()
            .all(|row| restored_capture.receipts.contains(row))
    );
    let restored_stream = root.path().join("restored.jsonl");
    logical_export::export_store(&restored, &restored_stream).unwrap();
    assert!(
        RedbStorageEngine::open(&restored).is_err(),
        "historical/v2 serving remains gated"
    );
}

#[tokio::test]
async fn purge_offline_active_and_cleanup_states_reject_all_consumers_without_source_changes() {
    for cleanup in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (source, namespace, _, _) = setup(root.path());
        let backup = root.path().join("before.zip");
        archive(&source, &backup);
        let archive_before = std::fs::read(&backup).unwrap();
        let mut engine = RedbStorageEngine::open(&source).unwrap();
        let mut progress = engine
            .begin_namespace_purge(&MutationContext::unkeyed(namespace), true)
            .unwrap();
        if cleanup {
            while progress.stage == purge::PurgeStage::Deleting {
                progress = engine
                    .resume_namespace_purge(
                        progress.store_id,
                        namespace,
                        progress.start_receipt.operation_id,
                        1000,
                    )
                    .unwrap();
            }
            assert_eq!(progress.stage, purge::PurgeStage::Cleanup);
        }
        drop(engine);
        let before = std::fs::read(source.join("meta.db")).unwrap();
        assert!(validation::canonical_files(&source).is_err());
        assert!(capture(&source).is_err());
        assert!(offline_snapshot::OfflineSnapshot::open(&source).is_err());
        let output = root.path().join("out.jsonl");
        assert!(logical_export::export_store(&source, &output).is_err());
        assert!(!output.exists());
        assert!(
            restore_from_backup(options(&backup, &source, false))
                .await
                .is_err()
        );
        let destination = root.path().join("migrated");
        assert!(
            schema_migration::migrate_schema(
                &source,
                &destination,
                &schema_migration::MigrationOptions {
                    migration_now: 200,
                    expected_source: control::GenerationRef {
                        store_id: progress.store_id,
                        generation: progress.last_receipt.committed_generation
                    },
                    attestations: vec![],
                }
            )
            .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(std::fs::read(&backup).unwrap(), archive_before);
        assert!(!root.path().join("source.recalld-restore-pending").exists());
        assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
    }
}

#[tokio::test]
async fn purge_offline_completed_history_survives_each_restore_publication_boundary() {
    use crate::storage::restore_journal::Stage;
    for stop in [
        Stage::Prepared,
        Stage::OldMoved,
        Stage::NewInstalled,
        Stage::Complete,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (source, namespace, live, forgotten) = setup(root.path());
        let backup = root.path().join("before.zip");
        archive(&source, &backup);
        let progress = complete(&source, namespace);
        let before = capture(&source).unwrap();
        let result = super::restore_at(options(&backup, &source, false), |stage| {
            if std::mem::discriminant(&stage) == std::mem::discriminant(&stop) {
                Err(StorageError::RecoveryRequired(
                    "injected purge restore interruption".into(),
                ))
            } else {
                Ok(())
            }
        })
        .await;
        assert!(result.is_err());
        let journal = root.path().join("source.recalld-restore-pending");
        assert!(journal.exists());
        let engine = RedbStorageEngine::open(&source).unwrap();
        assert_ne!(engine.control_state().unwrap().store_id, progress.store_id);
        assert!(engine.get_record(forgotten).unwrap().is_none());
        assert!(engine.get_record(live).unwrap().is_some());
        assert!(engine.get_namespace(namespace).unwrap().is_none());
        drop(engine);
        assert!(!journal.exists());
        let now = capture(&source).unwrap();
        assert_eq!(now.purge_progress, before.purge_progress);
        assert!(before.receipts.iter().all(|row| now.receipts.contains(row)));
    }
}

#[tokio::test]
async fn purge_offline_malformed_history_and_future_batch_tables_reject_immutably() {
    for mode in ["future", "orphan", "origin", "batch"] {
        let root = tempfile::tempdir().unwrap();
        let (source, namespace, _, _) = setup(root.path());
        let progress = complete(&source, namespace);
        let canonical = root.path().join("canonical");
        migrate(&source, &canonical);
        {
            let db = redb::Database::open(canonical.join("meta.db")).unwrap();
            let tx = db.begin_write().unwrap();
            match mode {
                "future" => {
                    tx.open_table(metadata::purge::SCHEMA)
                        .unwrap()
                        .insert("version", 2)
                        .unwrap();
                }
                "orphan" => {
                    tx.open_table(metadata::purge::PROGRESS)
                        .unwrap()
                        .remove(progress.start_receipt.operation_id.as_bytes().as_slice())
                        .unwrap();
                }
                "origin" => {
                    let mut bad = progress.clone();
                    bad.store_id = uuid::Uuid::new_v4();
                    tx.open_table(metadata::purge::PROGRESS)
                        .unwrap()
                        .insert(
                            progress.start_receipt.operation_id.as_bytes().as_slice(),
                            control::encode(&bad).unwrap().as_slice(),
                        )
                        .unwrap();
                }
                "batch" => {
                    tx.open_table(redb::TableDefinition::<&str, u32>::new("batch_schema"))
                        .unwrap()
                        .insert("version", 2)
                        .unwrap();
                    tx.open_table(redb::TableDefinition::<&[u8], &[u8]>::new(
                        "mutation_batches",
                    ))
                    .unwrap();
                }
                _ => unreachable!(),
            }
            tx.commit().unwrap();
        }
        let before = std::fs::read(canonical.join("meta.db")).unwrap();
        let output = root.path().join("bad.jsonl");
        assert!(
            logical_export::export_store(&canonical, &output).is_err(),
            "{mode}"
        );
        assert!(capture(&canonical).is_err(), "{mode}");
        assert!(!output.exists());
        assert_eq!(
            std::fs::read(canonical.join("meta.db")).unwrap(),
            before,
            "{mode}"
        );
    }
}

#[tokio::test]
async fn purge_offline_restore_rejects_record_loss_without_ledger_proof() {
    use redb::ReadableTable;
    let root = tempfile::tempdir().unwrap();
    let (source, namespace, _, forgotten) = setup(root.path());
    let backup = root.path().join("before.zip");
    archive(&source, &backup);
    complete(&source, namespace);
    {
        let db = redb::Database::open(source.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        let mut table = tx.open_table(metadata::removal_ledger::LEDGER).unwrap();
        let keys: Vec<_> = table
            .iter()
            .unwrap()
            .filter_map(|row| {
                let (key, bytes) = row.unwrap();
                let entry: metadata::removal_ledger::RemovalEntry =
                    control::decode(bytes.value()).unwrap();
                (entry.memory_uuid == forgotten.into_inner()).then(|| key.value().to_vec())
            })
            .collect();
        assert!(!keys.is_empty());
        for key in keys {
            table.remove(key.as_slice()).unwrap();
        }
        drop(table);
        tx.commit().unwrap();
    }
    let before = std::fs::read(source.join("meta.db")).unwrap();
    let archive_before = std::fs::read(&backup).unwrap();
    let error = restore_from_backup(options(&backup, &source, false))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("hard-delete ledger proof"),
        "{error}"
    );
    assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), archive_before);
    assert!(!root.path().join("source.recalld-restore-pending").exists());
}

// Exercise the real parser, catalog reservation and reserved child writers. No
// hand-written completed manifests are used for the positive offline fixtures.
fn batch_offline_run(
    source: &Path,
    kind: batch::BatchKind,
    items: Vec<serde_json::Value>,
    key: &str,
    finish: bool,
) -> batch::BatchProgress {
    use batch::{BatchError, BatchInput, BatchItemPlan, BatchItemState};
    let mut engine = RedbStorageEngine::open(source).unwrap();
    let input = BatchInput {
        result_version: 1,
        kind,
        items,
        default_namespace: "default".into(),
        store_id: Some(engine.control_state().unwrap().store_id),
        idempotency_key: Some(key.into()),
        batch_operation_id: None,
    };
    let plans: Vec<_> = crate::mcp::batches::prepare(&input)
        .unwrap()
        .into_iter()
        .map(|p| p.plan)
        .collect();
    let progress = engine
        .meta_store()
        .reserve_batch(&input.context(), kind, &plans)
        .unwrap();
    if finish {
        for (index, plan) in plans.iter().enumerate() {
            if !matches!(progress.items[index].state, BatchItemState::NotAttempted {}) {
                continue;
            }
            let child = progress.child(index).unwrap();
            match plan {
                BatchItemPlan::Insert { input, .. } if input.summary == "final-error" => {
                    engine
                        .meta_store()
                        .finalize_batch_error(&child, BatchError::new("CONFLICT"))
                        .unwrap();
                }
                BatchItemPlan::Insert { input, .. } => {
                    engine
                        .insert_memory_reserved(&child.context(), input, &[], 123, Some(&child))
                        .unwrap();
                }
                BatchItemPlan::Forget { id, .. } => {
                    engine
                        .tombstone_memory_reserved(&child.context(), *id, Some(&child))
                        .unwrap();
                }
                BatchItemPlan::Invalid { .. } => unreachable!(),
            }
        }
    }
    let result = engine
        .meta_store()
        .lookup_batch(
            progress.reservation_receipt.store_id,
            progress.reservation_receipt.operation_id,
        )
        .unwrap()
        .unwrap();
    engine.sync().unwrap();
    result
}
fn batch_offline_item(namespace: &str, summary: &str) -> serde_json::Value {
    serde_json::json!({"namespace":namespace,"summary":summary,"fullText":summary,
        "embedding":[1.,0.,0.,0.],"checkDuplicates":false})
}
fn batch_offline_history(source: &Path) -> Vec<batch::BatchProgress> {
    let first = batch_offline_run(
        source,
        batch::BatchKind::Insert,
        vec![
            batch_offline_item("retired", "batch-retired-payload"),
            serde_json::json!(17),
            batch_offline_item("default", "final-error"),
            batch_offline_item("default", "batch-retained"),
        ],
        "insert-history",
        true,
    );
    assert_eq!(first.unresolved(), 0);
    assert_eq!(first.bookkeeping_receipts.len(), 1);
    let batch::BatchItemState::Ok { receipt } = &first.items[0].state else {
        panic!()
    };
    let id = insertion::inserted_id(receipt).unwrap();
    let second = batch_offline_run(
        source,
        batch::BatchKind::Forget,
        vec![
            serde_json::json!(id.to_string()),
            serde_json::json!("not-a-uuid"),
        ],
        "forget-history",
        true,
    );
    assert_eq!(second.unresolved(), 0);
    vec![first, second]
}

#[tokio::test]
async fn batch_offline_completed_manifests_survive_purge_transfer_and_repeated_restore() {
    let root = tempfile::tempdir().unwrap();
    let (source, retired, _, _) = setup(root.path());
    let progress = batch_offline_history(&source);
    complete(&source, retired);
    // Reuse the NAME, never the original immutable namespace ID/defaults.
    let mut engine = RedbStorageEngine::open(&source).unwrap();
    let mut config = NamespaceConfig::default_namespace(0);
    config.name = "retired".into();
    config.embedding_dim = 4;
    assert!(engine.create_namespace(&config).unwrap().get() > retired.get());
    drop(engine);
    let original = capture(&source).unwrap();
    assert_eq!(original.batch_progress.as_ref().unwrap().len(), 2);
    let before = std::fs::read(source.join("meta.db")).unwrap();
    let canonical = root.path().join("canonical");
    migrate(&source, &canonical);
    assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
    let stream = root.path().join("batch.jsonl");
    let exported = logical_export::export_store(&canonical, &stream).unwrap();
    let bytes = std::fs::read_to_string(&stream).unwrap();
    assert!(!bytes.contains("batch-retired-payload"));
    assert!(!bytes.contains("final-error"));
    let imported = root.path().join("imported");
    assert_eq!(
        logical_export::import_store(&stream, &imported).unwrap(),
        exported
    );
    assert_eq!(
        logical_export::export_store(&imported, &root.path().join("again.jsonl"))
            .unwrap()
            .logical_sha256,
        exported.logical_sha256
    );
    let backup = root.path().join("batch.zip");
    archive(&imported, &backup);
    let archive_before = std::fs::read(&backup).unwrap();
    for n in 0..2 {
        let restored = root.path().join(format!("historical-{n}"));
        restore_from_backup(options(&backup, &restored, true))
            .await
            .unwrap();
        let now = capture(&restored).unwrap();
        assert_eq!(now.batch_progress, original.batch_progress);
        assert_eq!(now.purge_progress, original.purge_progress);
        assert!(
            original
                .receipts
                .iter()
                .all(|row| now.receipts.contains(row))
        );
        let rows = now.batch_progress.as_ref().unwrap();
        for p in &progress {
            let row = rows
                .iter()
                .find(|(k, _)| k == p.reservation_receipt.operation_id.as_bytes())
                .unwrap();
            assert_eq!(control::decode::<batch::BatchProgress>(&row.1).unwrap(), *p);
        }
        assert!(RedbStorageEngine::open(&restored).is_err());
        if n == 0 {
            archive(&restored, &root.path().join("fork.zip"));
        }
    }
    // Also restore a backup of an already-forked store, proving bounded ancestors.
    let twice = root.path().join("twice");
    restore_from_backup(options(&root.path().join("fork.zip"), &twice, true))
        .await
        .unwrap();
    assert_eq!(
        capture(&twice).unwrap().batch_progress,
        original.batch_progress
    );
    assert_eq!(std::fs::read(&backup).unwrap(), archive_before);
}

#[tokio::test]
async fn batch_offline_restore_merges_target_only_history_without_recreating_child_payloads() {
    let root = tempfile::tempdir().unwrap();
    let (source, retired, _, _) = setup(root.path());
    let backup = root.path().join("before.zip");
    archive(&source, &backup);
    let batches = batch_offline_history(&source);
    complete(&source, retired);
    let before = capture(&source).unwrap();
    let batch::BatchItemState::Ok { receipt } = &batches[0].items[3].state else {
        panic!()
    };
    let absent = insertion::inserted_id(receipt).unwrap();
    for _ in 0..2 {
        restore_from_backup(options(&backup, &source, false))
            .await
            .unwrap();
        let after = capture(&source).unwrap();
        assert_eq!(after.batch_progress, before.batch_progress);
        assert_eq!(after.purge_progress, before.purge_progress);
        assert!(
            before
                .receipts
                .iter()
                .all(|row| after.receipts.contains(row))
        );
        let engine = RedbStorageEngine::open(&source).unwrap();
        assert!(engine.get_record(absent).unwrap().is_none());
        // Historical manifests cannot be resumed in the new retry domain.
        assert!(matches!(
            engine.meta_store().lookup_batch(
                batches[0].reservation_receipt.store_id,
                batches[0].reservation_receipt.operation_id
            ),
            Err(StorageError::StoreLineageMismatch)
        ));
    }
}

#[tokio::test]
async fn batch_offline_unresolved_rejects_all_offline_consumers_immutably() {
    let root = tempfile::tempdir().unwrap();
    let (source, _, _, _) = setup(root.path());
    let backup = root.path().join("before.zip");
    archive(&source, &backup);
    let p = batch_offline_run(
        &source,
        batch::BatchKind::Insert,
        vec![batch_offline_item("default", "pending-secret")],
        "pending",
        false,
    );
    assert_eq!(p.unresolved(), 1);
    let before = std::fs::read(source.join("meta.db")).unwrap();
    let archived = std::fs::read(&backup).unwrap();
    assert!(validation::canonical_files(&source).is_err());
    assert!(capture(&source).is_err());
    assert!(offline_snapshot::OfflineSnapshot::open(&source).is_err());
    let output = root.path().join("out.jsonl");
    assert!(logical_export::export_store(&source, &output).is_err());
    assert!(!output.exists());
    let destination = root.path().join("migration");
    assert!(
        schema_migration::migrate_schema(
            &source,
            &destination,
            &schema_migration::MigrationOptions {
                migration_now: 200,
                expected_source: control::GenerationRef {
                    store_id: p.reservation_receipt.store_id,
                    generation: p.reservation_receipt.committed_generation,
                },
                attestations: vec![],
            }
        )
        .is_err()
    );
    assert!(!destination.exists());
    assert!(
        restore_from_backup(options(&backup, &source, false))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), archived);
    assert!(!root.path().join("source.recalld-restore-pending").exists());
}

#[tokio::test]
async fn batch_offline_conflicting_completed_bytes_reject_before_publication() {
    let root = tempfile::tempdir().unwrap();
    let (source, _, _, _) = setup(root.path());
    let p = batch_offline_run(
        &source,
        batch::BatchKind::Insert,
        vec![batch_offline_item("default", "kept")],
        "same-parent",
        true,
    );
    let backup = root.path().join("before.zip");
    archive(&source, &backup);
    {
        let db = redb::Database::open(source.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        // Individually valid, semantically equal, but not the original immutable row bytes.
        tx.open_table(metadata::batch::BATCHES)
            .unwrap()
            .insert(
                p.reservation_receipt.operation_id.as_bytes().as_slice(),
                serde_json::to_vec_pretty(&p).unwrap().as_slice(),
            )
            .unwrap();
        tx.commit().unwrap();
    }
    capture(&source).unwrap();
    let before = std::fs::read(source.join("meta.db")).unwrap();
    let archived = std::fs::read(&backup).unwrap();
    let err = restore_from_backup(options(&backup, &source, false))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("conflicting historical"), "{err}");
    assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), archived);
    assert!(!root.path().join("source.recalld-restore-pending").exists());
}

#[tokio::test]
async fn batch_offline_malformed_schema_parent_and_child_evidence_reject_immutably() {
    for mode in ["future", "parent", "child", "origin"] {
        let root = tempfile::tempdir().unwrap();
        let (source, _, _, _) = setup(root.path());
        let p = batch_offline_run(
            &source,
            batch::BatchKind::Insert,
            vec![batch_offline_item("default", "evidence")],
            "evidence",
            true,
        );
        let backup = root.path().join("valid.zip");
        archive(&source, &backup);
        {
            let db = redb::Database::open(source.join("meta.db")).unwrap();
            let tx = db.begin_write().unwrap();
            match mode {
                "future" => {
                    tx.open_table(metadata::batch::SCHEMA)
                        .unwrap()
                        .insert("version", 2)
                        .unwrap();
                }
                "parent" => {
                    tx.open_table(metadata::batch::BATCHES)
                        .unwrap()
                        .remove(p.reservation_receipt.operation_id.as_bytes().as_slice())
                        .unwrap();
                }
                "child" => {
                    let batch::BatchItemState::Ok { receipt } = &p.items[0].state else {
                        panic!()
                    };
                    let key = control::scoped_key(
                        receipt.store_id,
                        control::MutationScope::Namespace(
                            p.items[0].defaults.as_ref().unwrap().namespace_id,
                        ),
                        receipt.operation_id.as_bytes(),
                    );
                    tx.open_table(control::RECEIPTS)
                        .unwrap()
                        .remove(key.as_slice())
                        .unwrap();
                }
                "origin" => {
                    let mut bad = p.clone();
                    bad.reservation_receipt.store_id = uuid::Uuid::new_v4();
                    tx.open_table(metadata::batch::BATCHES)
                        .unwrap()
                        .insert(
                            p.reservation_receipt.operation_id.as_bytes().as_slice(),
                            control::encode(&bad).unwrap().as_slice(),
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
            tx.commit().unwrap();
        }
        let before = std::fs::read(source.join("meta.db")).unwrap();
        assert!(capture(&source).is_err(), "{mode}");
        assert!(validation::canonical_files(&source).is_err(), "{mode}");
        let output = root.path().join("invalid.jsonl");
        assert!(
            logical_export::export_store(&source, &output).is_err(),
            "{mode}"
        );
        assert!(!output.exists());
        assert!(
            restore_from_backup(options(&backup, &source, false))
                .await
                .is_err(),
            "{mode}"
        );
        assert_eq!(
            std::fs::read(source.join("meta.db")).unwrap(),
            before,
            "{mode}"
        );
    }
}

#[test]
fn batch_offline_origin_bound_includes_child_and_error_receipts_not_just_reservation() {
    let root = tempfile::tempdir().unwrap();
    let (source, _, _, _) = setup(root.path());
    let p = batch_offline_run(
        &source,
        batch::BatchKind::Insert,
        vec![
            batch_offline_item("default", "child"),
            batch_offline_item("default", "final-error"),
        ],
        "bounded",
        true,
    );
    let (_copy, db) = validation::inspect_database(&source.join("meta.db")).unwrap();
    let tx = db.begin_read().unwrap();
    let mut state = control::validate_existing(&tx).unwrap().unwrap();
    assert!(metadata::batch::validate_existing(&tx, Some(&state)).is_ok());
    state.committed_generation = p.reservation_receipt.committed_generation;
    let error = metadata::batch::validate_existing(&tx, Some(&state)).unwrap_err();
    assert!(
        error.to_string().contains("identity/version/item count"),
        "{error}"
    );
    let batch::BatchItemState::Ok { receipt } = &p.items[0].state else {
        panic!()
    };
    state.committed_generation = receipt.committed_generation;
    let error = metadata::batch::validate_existing(&tx, Some(&state)).unwrap_err();
    assert!(
        error.to_string().contains("identity/version/item count"),
        "{error}"
    );
}
