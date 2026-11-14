//! Synthetic actual-engine/consumer proofs for resumable namespace deletion.
use super::*;
use crate::model::{NamespaceConfig, NamespaceId};
use crate::search::VectorIndex;
use crate::storage::control::{MutationContext, MutationScope};
use crate::storage::purge::PurgeStage;
use crate::test_support::{DIM, Fixture};

fn context(e: &RedbStorageEngine, ns: NamespaceId) -> MutationContext {
    let c = e.control_state().unwrap();
    MutationContext {
        store_id: Some(c.store_id),
        scope: MutationScope::Namespace(ns),
        idempotency_key: Some("durable-purge".into()),
        expected_generation: Some(c.committed_generation),
        contract_version: 1,
    }
}
fn resume(
    e: &mut RedbStorageEngine,
    p: &purge::PurgeProgress,
    limit: usize,
) -> purge::PurgeProgress {
    e.resume_namespace_purge(
        p.store_id,
        p.namespace_id,
        p.start_receipt.operation_id,
        limit,
    )
    .unwrap()
}
fn validate(path: &std::path::Path) -> Vec<purge::PurgeProgress> {
    let (_copy, db) = validation::inspect_database_copy(&path.join("meta.db"), true).unwrap();
    let tx = db.begin_read().unwrap();
    let state = control::validate_existing(&tx).unwrap();
    metadata::purge::validate_existing(&tx, state.as_ref()).unwrap()
}

#[tokio::test]
async fn purge_multichunk_reopens_automatically_and_replays_before_cas_without_name_reuse_damage() {
    let fx = Fixture::new();
    let ns = fx.create_namespace("purged");
    let first = fx.insert_memory_in(ns, &[]).await;
    let second = fx.insert_memory_in(ns, &[]).await;
    fx.tombstone(second);
    let third = fx.insert_memory_in(ns, &[]).await;
    let dir = fx.close();
    let mut e = RedbStorageEngine::open(dir.path()).unwrap();
    let ctx = context(&e, ns);
    let start = e.begin_namespace_purge(&ctx, true).unwrap();
    let start_bytes = serde_json::to_vec(&start.start_receipt).unwrap();
    assert!(e.get_record(first).is_err());
    assert!(e.meta_store().tombstone(first).is_err());
    assert!(e.meta_store().delete_namespace(ns).is_err());
    let replay = e.begin_namespace_purge(&ctx, true).unwrap();
    assert!(replay.start_receipt.replayed);
    assert_eq!(replay.next_operation_id, start.next_operation_id);
    let mut changed = ctx.clone();
    changed.idempotency_key = Some("other".into());
    assert!(e.begin_namespace_purge(&changed, true).is_err());
    assert!(matches!(
        e.begin_namespace_purge(&ctx, false),
        Err(StorageError::IdempotencyConflict)
    ));
    let partial = resume(&mut e, &start, 1);
    assert_eq!(partial.removed, 1);
    assert_eq!(
        partial.last_receipt.operation_id,
        start.next_operation_id.unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&partial.start_receipt).unwrap(),
        start_bytes
    );
    assert_eq!(
        e.lookup_operation(start.store_id, ctx.scope, start.start_receipt.operation_id)
            .unwrap()
            .unwrap(),
        start.start_receipt
    );
    drop(e);
    let before = std::fs::read(dir.path().join("meta.db")).unwrap();
    assert!(validation::canonical_files(dir.path()).is_err());
    assert_eq!(std::fs::read(dir.path().join("meta.db")).unwrap(), before);
    let mut e = RedbStorageEngine::open(dir.path()).unwrap();
    let done = e
        .lookup_namespace_purge(start.store_id, ns, start.start_receipt.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(done.stage, PurgeStage::Complete);
    assert_eq!(done.removed, 3);
    assert_eq!(done.tombstones_removed, 1);
    assert!(done.next_operation_id.is_none());
    for id in [first, second, third] {
        assert!(e.get_record(id).unwrap().is_none());
    }
    assert!(!dir.path().join("purged").exists());
    let generation = e.control_state().unwrap().committed_generation;
    let replay = e.begin_namespace_purge(&ctx, true).unwrap();
    assert!(replay.start_receipt.replayed);
    assert_eq!(replay.removed, 3);
    assert_eq!(e.control_state().unwrap().committed_generation, generation);
    let mut c = NamespaceConfig::default_namespace(0);
    c.name = "purged".into();
    c.embedding_dim = DIM as u32;
    let new = e.create_namespace(&c).unwrap();
    assert_ne!(new, ns);
    let vector = std::fs::read(dir.path().join("purged/vectors.dat")).unwrap();
    resume(&mut e, &done, 1);
    assert_eq!(
        std::fs::read(dir.path().join("purged/vectors.dat")).unwrap(),
        vector
    );
    drop(e);
    assert_eq!(validate(dir.path())[0].removed, 3);
    validation::canonical_files(dir.path()).unwrap();
}

#[tokio::test]
async fn purge_unknown_child_reconciles_reserved_id_and_never_repeats_removal() {
    let fx = Fixture::new();
    let ns = fx.create_namespace("unknown");
    let id = fx.insert_memory_in(ns, &[]).await;
    let dir = fx.close();
    let mut e = RedbStorageEngine::open(dir.path()).unwrap();
    let p = e.begin_namespace_purge(&context(&e, ns), true).unwrap();
    metadata::effects::FAIL_AFTER_RECORD_COMMIT.with(|f| f.set(true));
    let error = e
        .resume_namespace_purge(p.store_id, ns, p.start_receipt.operation_id, 1)
        .unwrap_err();
    assert!(
        matches!(error,StorageError::CommitOutcomeUnknown {operation_id,..} if Some(operation_id)==p.next_operation_id)
    );
    assert_eq!(
        e.lookup_namespace_purge(p.store_id, ns, p.start_receipt.operation_id)
            .unwrap()
            .unwrap()
            .removed,
        1
    );
    drop(e);
    for _ in 0..2 {
        let e = RedbStorageEngine::open(dir.path()).unwrap();
        let p = e
            .lookup_namespace_purge(p.store_id, ns, p.start_receipt.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(p.removed, 1);
        assert_eq!(p.sequence, 3);
        assert_eq!(p.stage, PurgeStage::Complete);
        assert!(e.get_record(id).unwrap().is_none());
    }
    let (_copy, db) = validation::inspect_database(&dir.path().join("meta.db")).unwrap();
    let tx = db.begin_read().unwrap();
    let state = control::validate_existing(&tx).unwrap();
    let ledger = metadata::removal_ledger::read_export(&tx, state.as_ref())
        .unwrap()
        .unwrap();
    let rows: Vec<_> = ledger
        .entries
        .iter()
        .filter(|r| r.memory_uuid == id.into_inner())
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(Some(rows[0].operation_id), p.next_operation_id);
}

#[test]
fn purge_empty_final_unknown_keeps_lease_until_durable_cleanup_then_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = RedbStorageEngine::open(dir.path()).unwrap();
    let mut c = NamespaceConfig::default_namespace(0);
    c.name = "cleanup".into();
    c.embedding_dim = DIM as u32;
    let ns = e.create_namespace(&c).unwrap();
    let start = e.begin_namespace_purge(&context(&e, ns), false).unwrap();
    let cleanup = resume(&mut e, &start, 1);
    assert_eq!(cleanup.stage, PurgeStage::Cleanup);
    assert!(e.create_namespace(&c).is_err());
    assert!(dir.path().join("cleanup").exists());
    metadata::effects::FAIL_AFTER_RECORD_COMMIT.with(|f| f.set(true));
    let error = e
        .resume_namespace_purge(start.store_id, ns, start.start_receipt.operation_id, 1)
        .unwrap_err();
    assert!(
        matches!(error,StorageError::CommitOutcomeUnknown {operation_id,..} if Some(operation_id)==cleanup.next_operation_id)
    );
    drop(e);
    let mut e = RedbStorageEngine::open(dir.path()).unwrap();
    let done = e
        .lookup_namespace_purge(start.store_id, ns, start.start_receipt.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(done.stage, PurgeStage::Complete);
    assert_eq!(done.removed, 0);
    assert_eq!(done.sequence, 2);
    assert_ne!(e.create_namespace(&c).unwrap(), ns);
}

#[tokio::test]
async fn purge_force_precondition_uses_canonical_rows_and_changes_nothing_on_rejection() {
    let fx = Fixture::new();
    let ns = fx.create_namespace("force");
    fx.insert_memory_in(ns, &[]).await;
    let mut e = fx.storage.write().unwrap();
    let ctx = context(&e, ns);
    let before = e.control_state().unwrap();
    assert!(matches!(
        e.begin_namespace_purge(&ctx, false),
        Err(StorageError::InvalidMutation(_))
    ));
    assert_eq!(e.control_state().unwrap(), before);
    assert!(e.meta_store().purge_progress().unwrap().is_empty());
    assert!(
        e.lookup_operation(before.store_id, ctx.scope, uuid::Uuid::new_v4())
            .unwrap()
            .is_none()
    );
}

#[test]
fn purge_process_child() {
    let Some(path) = std::env::var_os("RECALLD_PURGE_TEST_STORE") else {
        return;
    };
    let mut e = RedbStorageEngine::open(path).unwrap();
    let ns = NamespaceId::new(
        std::env::var("RECALLD_PURGE_TEST_NS")
            .unwrap()
            .parse()
            .unwrap(),
    );
    let mut p = e.begin_namespace_purge(&context(&e, ns), true).unwrap();
    while p.stage != PurgeStage::Complete {
        p = resume(&mut e, &p, 1);
    }
    panic!("process fault boundary not reached");
}

#[tokio::test]
async fn purge_process_boundaries_recover_start_chunk_edges_and_directory_cleanup() {
    for point in [
        "before_purge_commit",
        "after_purge_commit",
        "after_metadata_commit",
        "after_edge_commit",
        "after_canonical_ack",
        "after_purge_directory_cleanup",
    ] {
        let fx = Fixture::new();
        let ns = fx.create_namespace("crash");
        let id = fx.insert_memory_in(ns, &[]).await;
        let dir = fx.close();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::purge_tests::purge_process_child",
                "--nocapture",
            ])
            .env("RECALLD_PURGE_TEST_STORE", dir.path())
            .env("RECALLD_PURGE_TEST_NS", ns.get().to_string())
            .env("RECALLD_TEST_EFFECT_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(91),
            "{point}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        for _ in 0..2 {
            let e = RedbStorageEngine::open(dir.path()).unwrap();
            if point == "before_purge_commit" {
                assert!(e.get_record(id).unwrap().is_some());
                assert!(e.meta_store().purge_progress().unwrap().is_empty());
            } else {
                assert!(e.get_record(id).unwrap().is_none());
                let p = &e.meta_store().purge_progress().unwrap()[0];
                assert_eq!(p.stage, PurgeStage::Complete);
                assert_eq!(p.removed, 1);
                assert_eq!(p.sequence, 3);
            }
        }
        validate(dir.path());
    }
}

#[test]
fn purge_malformed_progress_fails_disposable_preflight_without_source_change() {
    for mode in [
        "version",
        "counts",
        "name",
        "reserved",
        "missing-progress",
        "missing-schema",
        "wrong-type",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut e = RedbStorageEngine::open(dir.path()).unwrap();
        let mut config = NamespaceConfig::default_namespace(0);
        config.name = "corrupt".into();
        config.embedding_dim = DIM as u32;
        let ns = e.create_namespace(&config).unwrap();
        let p = e.begin_namespace_purge(&context(&e, ns), false).unwrap();
        drop(e);
        {
            let db = redb::Database::open(dir.path().join("meta.db")).unwrap();
            let tx = db.begin_write().unwrap();
            match mode {
                "missing-progress" => {
                    tx.delete_table(metadata::purge::PROGRESS).unwrap();
                }
                "missing-schema" => {
                    tx.delete_table(metadata::purge::SCHEMA).unwrap();
                }
                "wrong-type" => {
                    tx.delete_table(metadata::purge::PROGRESS).unwrap();
                    tx.open_table(redb::TableDefinition::<u64, u64>::new(
                        "namespace_purge_progress",
                    ))
                    .unwrap()
                    .insert(1, 1)
                    .unwrap();
                }
                _ => {
                    let mut corrupt = p.clone();
                    match mode {
                        "version" => corrupt.version = 999,
                        "counts" => corrupt.removed = 1,
                        "name" => corrupt.name = "unowned".into(),
                        _ => corrupt.next_operation_id = Some(p.start_receipt.operation_id),
                    }
                    tx.open_table(metadata::purge::PROGRESS)
                        .unwrap()
                        .insert(
                            p.start_receipt.operation_id.as_bytes().as_slice(),
                            control::encode(&corrupt).unwrap().as_slice(),
                        )
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let before = std::fs::read(dir.path().join("meta.db")).unwrap();
        assert!(RedbStorageEngine::open(dir.path()).is_err(), "{mode}");
        assert_eq!(
            std::fs::read(dir.path().join("meta.db")).unwrap(),
            before,
            "{mode}"
        );
        assert!(
            MetadataStore::open(&dir.path().join("meta.db")).is_err(),
            "direct: {mode}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("meta.db")).unwrap(),
            before,
            "direct: {mode}"
        );
        assert!(dir.path().join("corrupt/vectors.dat").exists());
    }
}

fn publisher(fx: &Fixture) -> crate::derived_completion::DerivedCompletion {
    crate::derived_completion::DerivedCompletion {
        storage: fx.storage.clone(),
        cache: fx.cache.clone(),
        graph: fx.graph.clone(),
        vectors: fx.vector_index.clone(),
        fts: fx.fts_index.clone(),
        entities: fx.entity_index.clone(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_actual_completion_resumes_accepted_work_and_reopens_all_views_without_restart() {
    use crate::mcp::bridge::StorageEngine as _;
    let fx = Fixture::new();
    let ns = fx.create_namespace("online");
    let id = fx.insert_memory_in(ns, &["entity/removed"]).await;
    let survivor = fx.insert_memory().await;
    let start = {
        let mut e = fx.storage.write().unwrap();
        let ctx = context(&e, ns);
        e.begin_namespace_purge(&ctx, true).unwrap()
    };
    assert!(fx.storage_adapter().get_memory(survivor).await.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(5), publisher(&fx).complete())
        .await
        .unwrap()
        .unwrap();
    assert!(fx.storage_adapter().get_memory(id).await.unwrap().is_none());
    assert!(
        fx.storage_adapter()
            .get_memory(survivor)
            .await
            .unwrap()
            .is_some()
    );
    assert!(!fx.graph.read().await.contains(&id));
    assert!(fx.vector_index.read().await.get_vector(id).is_none());
    let e = fx.storage.read().unwrap();
    assert!(e.meta_store().pending_effects().unwrap().is_empty());
    assert_eq!(
        e.lookup_namespace_purge(start.store_id, ns, start.start_receipt.operation_id)
            .unwrap()
            .unwrap()
            .stage,
        PurgeStage::Complete
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_actual_consumer_publication_failure_retains_completion_receipt_then_retries() {
    use crate::mcp::bridge::{NamespaceRegistry, StorageEngine as _};
    let fx = Fixture::new();
    let ns = fx.create_namespace("publication");
    let removed = fx.insert_memory_in(ns, &[]).await;
    let survivor = fx.insert_memory().await;
    *fx.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM + 1);
    let error = fx
        .namespace_adapter()
        .delete_namespace(crate::mcp::bridge::DeleteNamespaceInput {
            name: "publication".into(),
            force: true,
        })
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("commitState=committed") && text.contains("receipt="),
        "{text}"
    );
    assert!(fx.storage_adapter().get_memory(survivor).await.is_err());
    let (progress, generation) = {
        let e = fx.storage.read().unwrap();
        (
            e.meta_store().purge_progress().unwrap()[0].clone(),
            e.control_state().unwrap().committed_generation,
        )
    };
    assert_eq!(progress.stage, PurgeStage::Complete);
    assert!(text.contains(&progress.last_receipt.operation_id.to_string()));
    *fx.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM);
    publisher(&fx).complete().await.unwrap();
    assert!(
        fx.storage_adapter()
            .get_memory(removed)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fx.storage_adapter()
            .get_memory(survivor)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fx.storage
            .read()
            .unwrap()
            .control_state()
            .unwrap()
            .committed_generation,
        generation
    );
}

#[tokio::test]
async fn purge_offline_snapshot_backup_and_schema_migration_preserve_completed_history() {
    use std::io::Read;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let mut e = RedbStorageEngine::open(&source).unwrap();
    let mut c = NamespaceConfig::default_namespace(0);
    c.name = "history".into();
    c.embedding_dim = DIM as u32;
    let ns = e.create_namespace(&c).unwrap();
    let p = e.begin_namespace_purge(&context(&e, ns), false).unwrap();
    drop(e);
    assert!(offline_snapshot::OfflineSnapshot::open(&source).is_err());
    let archive = root.path().join("pending.zip");
    assert!(
        crate::backup::run_backup(
            &crate::config::RecalldConfig::default(),
            &archive,
            Some(&source),
            false
        )
        .await
        .is_err()
    );
    assert!(!archive.exists());
    let e = RedbStorageEngine::open(&source).unwrap();
    let generation = e.control_state().unwrap();
    drop(e);
    let done = validate(&source)[0].clone();
    assert_eq!(done.start_receipt, p.start_receipt);
    let snapshot = offline_snapshot::OfflineSnapshot::open(&source).unwrap();
    let tx = snapshot.metadata.begin_read().unwrap();
    let original = tx
        .open_table(metadata::purge::PROGRESS)
        .unwrap()
        .get(p.start_receipt.operation_id.as_bytes().as_slice())
        .unwrap()
        .unwrap()
        .value()
        .to_vec();
    drop(tx);
    drop(snapshot);
    let archive = root.path().join("done.zip");
    crate::backup::run_backup(
        &crate::config::RecalldConfig::default(),
        &archive,
        Some(&source),
        false,
    )
    .await
    .unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    let mut backed_up = vec![];
    zip.by_name("meta.db")
        .unwrap()
        .read_to_end(&mut backed_up)
        .unwrap();
    assert_eq!(backed_up, std::fs::read(source.join("meta.db")).unwrap());
    let target = root.path().join("migrated");
    schema_migration::migrate_schema(
        &source,
        &target,
        &schema_migration::MigrationOptions {
            migration_now: 123,
            expected_source: control::GenerationRef {
                store_id: generation.store_id,
                generation: generation.committed_generation,
            },
            attestations: vec![],
        },
    )
    .unwrap();
    let migrated = offline_snapshot::OfflineSnapshot::open(&target).unwrap();
    let tx = migrated.metadata.begin_read().unwrap();
    assert_eq!(
        tx.open_table(metadata::purge::PROGRESS)
            .unwrap()
            .get(p.start_receipt.operation_id.as_bytes().as_slice())
            .unwrap()
            .unwrap()
            .value(),
        original
    );
    assert_eq!(validate(&target)[0], done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_actual_transport_retains_current_child_receipt_and_unknown_operation() {
    use crate::mcp::bridge::NamespaceRegistry;
    use std::sync::atomic::Ordering;
    for unknown in [false, true] {
        let fx = Fixture::new();
        let ns = fx.create_namespace("childidentity");
        fx.insert_memory_in(ns, &[]).await;
        {
            let e = fx.storage.read().unwrap();
            if unknown {
                e.meta_store()
                    .fail_purge_child_commit
                    .store(true, Ordering::SeqCst);
            } else {
                e.meta_store()
                    .fail_effect_application
                    .store(true, Ordering::SeqCst);
            }
        }
        let error = fx
            .namespace_adapter()
            .delete_namespace(crate::mcp::bridge::DeleteNamespaceInput {
                name: "childidentity".into(),
                force: true,
            })
            .await
            .unwrap_err();
        let text = error.to_string();
        let progress = fx
            .storage
            .read()
            .unwrap()
            .meta_store()
            .purge_progress()
            .unwrap()[0]
            .clone();
        let child = progress.last_receipt.clone();
        assert_ne!(child.operation_id, progress.start_receipt.operation_id);
        assert_eq!(progress.sequence, 1);
        assert_eq!(progress.removed, 1);
        if unknown {
            assert!(
                text.contains(&format!(
                    "commit outcome unknown for operation {}:",
                    child.operation_id
                )),
                "{text}"
            );
            assert!(
                !text.contains("commitState=committed"),
                "unknown child must stay top-level unknown: {text}"
            );
        } else {
            let json = text
                .split("receipt=")
                .nth(1)
                .unwrap()
                .split(" cause=")
                .next()
                .unwrap();
            let returned: control::WriteReceipt = serde_json::from_str(json).unwrap();
            assert_eq!(returned, child, "current child, not previous start receipt");
        }
        let dir = fx.close();
        let e = RedbStorageEngine::open(dir.path()).unwrap();
        let done = e
            .lookup_namespace_purge(progress.store_id, ns, progress.start_receipt.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(done.removed, 1);
        assert_eq!(done.sequence, 3);
        assert_eq!(done.stage, PurgeStage::Complete);
        assert_eq!(
            e.lookup_operation(
                progress.store_id,
                MutationScope::Namespace(ns),
                child.operation_id
            )
            .unwrap()
            .unwrap(),
            child
        );
    }
}
