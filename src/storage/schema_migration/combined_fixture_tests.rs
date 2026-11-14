//! Integration-only probes: actual ledger/effects writers, never synthesized rows.
use super::*;
use crate::model::{AccessKind, EdgeType};
use crate::storage::metadata::{effects, removal_ledger};
use crate::storage::{PersistedEdge, RedbStorageEngine, StorageEngine};
use crate::test_support::Fixture;
use redb::TableDefinition;
use std::collections::BTreeMap;

fn files(path: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(base: &Path, path: &Path, result: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let child = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(base, &child, result);
            } else if child.extension().is_some_and(|e| e == "dat")
                || matches!(entry.file_name().to_str(), Some("meta.db" | "edges.db"))
            {
                result.insert(
                    child.strip_prefix(base).unwrap().to_path_buf(),
                    std::fs::read(child).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(path, path, &mut result);
    result
}

fn byte_rows(
    read: &redb::ReadTransaction,
    table: TableDefinition<&[u8], &[u8]>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    read.open_table(table)
        .unwrap()
        .iter()
        .unwrap()
        .map(|r| {
            let (k, v) = r.unwrap();
            (k.value().to_vec(), v.value().to_vec())
        })
        .collect()
}

fn text_rows(
    read: &redb::ReadTransaction,
    table: TableDefinition<&str, &[u8]>,
) -> Vec<(String, Vec<u8>)> {
    read.open_table(table)
        .unwrap()
        .iter()
        .unwrap()
        .map(|r| {
            let (k, v) = r.unwrap();
            (k.value().to_owned(), v.value().to_vec())
        })
        .collect()
}

fn edge(a: crate::model::MemoryId, b: crate::model::MemoryId) -> PersistedEdge {
    PersistedEdge {
        source: a,
        target: b,
        edge_type: EdgeType::Associative,
        weight: 0.375,
        auto_created: false,
        created_at: 123,
    }
}

#[tokio::test]
async fn combined_migration_preserves_real_ledger_keys_receipts_and_applied_effects() {
    let fixture = Fixture::new();
    let removed = fixture.insert_memory().await;
    let a = fixture.insert_memory().await;
    let b = fixture.insert_memory().await;
    let (expected_source, deletion, access) = {
        let mut engine = fixture.storage.write().unwrap();
        let state = engine.control_state().unwrap();
        let access = engine
            .update_access_with_receipt(
                &MutationContext {
                    store_id: Some(state.store_id),
                    scope: MutationScope::Namespace(NamespaceId::new(1)),
                    idempotency_key: Some("retained-after-delete".into()),
                    expected_generation: Some(state.committed_generation),
                    contract_version: 1,
                },
                removed,
                42,
                AccessKind::DirectRetrieval,
            )
            .unwrap();
        let added = engine
            .put_edges_with_effects(&[edge(removed, a)])
            .unwrap()
            .unwrap();
        engine
            .acknowledge_graph_effects(added.receipt.operation_id)
            .unwrap();
        let deletion = engine
            .delete_memory_with_edges(removed, &[edge(a, b)])
            .unwrap()
            .unwrap();
        let ledger = engine.meta_store().removal_ledger_export().unwrap();
        let entry = ledger
            .entries
            .iter()
            .find(|e| e.memory_uuid == removed.into_inner())
            .unwrap();
        assert_eq!(
            entry.terminal_kind,
            Some(removal_ledger::TerminalKind::HardDelete)
        );
        assert_eq!(entry.operation_id, deletion.receipt.operation_id);
        assert_eq!(entry.generation, deletion.receipt.committed_generation);
        assert_eq!(entry.removed_fields_mask, removal_ledger::ALL);
        assert!(
            engine
                .meta_store()
                .pending_effects()
                .unwrap()
                .iter()
                .all(|e| e.canonical_applied)
        );
        engine.sync().unwrap();
        (
            engine.control_state().unwrap().generation_ref(),
            deletion.receipt,
            access,
        )
    };
    let source = fixture.close();
    let before = files(source.path());
    let target_parent = tempfile::tempdir().unwrap();
    let target = target_parent.path().join("schema");
    let report = migrate_schema(
        source.path(),
        &target,
        &MigrationOptions {
            migration_now: 12345,
            expected_source,
            attestations: vec![],
        },
    )
    .unwrap();
    assert!(report.receipt.is_none());
    let inspected = inspect_schema(&target).unwrap();
    assert_eq!(inspected.control.generation_ref(), expected_source);
    assert_eq!(inspected.control.removal_ledger_version, Some(1));
    assert_eq!(inspected.record_count, 2);
    assert_eq!(files(source.path()), before);
    let (_source_copy, source_db) =
        crate::storage::validation::inspect_database(&source.path().join("meta.db")).unwrap();
    let (_target_copy, target_db) =
        crate::storage::validation::inspect_database(&target.join("meta.db")).unwrap();
    let source_read = source_db.begin_read().unwrap();
    let target_read = target_db.begin_read().unwrap();
    for table in [
        control::RECEIPTS,
        control::KEYS,
        effects::EFFECTS,
        removal_ledger::LEDGER,
    ] {
        assert_eq!(
            byte_rows(&source_read, table),
            byte_rows(&target_read, table)
        );
    }
    for table in [
        removal_ledger::STATE,
        TableDefinition::new("text_generations_v1"),
    ] {
        assert_eq!(
            text_rows(&source_read, table),
            text_rows(&target_read, table)
        );
    }
    assert_eq!(
        source_read
            .open_table(effects::SCHEMA)
            .unwrap()
            .get("version")
            .unwrap()
            .unwrap()
            .value(),
        target_read
            .open_table(effects::SCHEMA)
            .unwrap()
            .get("version")
            .unwrap()
            .unwrap()
            .value()
    );
    assert_eq!(
        source_read
            .open_table(crate::storage::metadata::NAMESPACE_COUNTER)
            .unwrap()
            .get("max_id")
            .unwrap()
            .unwrap()
            .value(),
        target_read
            .open_table(crate::storage::metadata::NAMESPACE_COUNTER)
            .unwrap()
            .get("max_id")
            .unwrap()
            .unwrap()
            .value()
    );
    let old_control = control::validate_existing(&source_read).unwrap().unwrap();
    let mut new_control = control::validate_existing(&target_read).unwrap().unwrap();
    new_control.control_schema_version = old_control.control_schema_version;
    assert_eq!(new_control, old_control);
    assert_eq!(
        removal_ledger::read_export(&source_read, Some(&old_control)).unwrap(),
        removal_ledger::read_export(&target_read, Some(&inspected.control)).unwrap()
    );
    let copied = effects::validate_existing(&target_read).unwrap();
    assert_eq!(copied.len(), 1);
    assert!(copied[0].canonical_applied);
    assert_eq!(copied[0].receipt, deletion);
    assert!(
        target_read
            .open_table(META_TABLE)
            .unwrap()
            .get(removed.as_bytes().as_slice())
            .unwrap()
            .is_none()
    );
    for receipt in [access, deletion] {
        let key = control::scoped_key(
            expected_source.store_id,
            MutationScope::Namespace(NamespaceId::new(1)),
            receipt.operation_id.as_bytes(),
        );
        let rows = target_read.open_table(control::RECEIPTS).unwrap();
        assert_eq!(
            control::validate_receipt(rows.get(key.as_slice()).unwrap().unwrap().value())
                .unwrap()
                .receipt,
            receipt
        );
    }
    let durable = files(&target);
    assert!(RedbStorageEngine::open(&target).is_err());
    assert_eq!(files(&target), durable);
    assert_eq!(files(source.path()), before);
}

#[tokio::test]
async fn combined_pending_effects_reject_migration_immutably_but_startup_preflight_allows_recovery()
{
    let fixture = Fixture::new();
    let removed = fixture.insert_memory().await;
    let peer = fixture.insert_memory().await;
    {
        let mut engine = fixture.storage.write().unwrap();
        let added = engine
            .put_edges_with_effects(&[edge(removed, peer)])
            .unwrap()
            .unwrap();
        engine
            .acknowledge_graph_effects(added.receipt.operation_id)
            .unwrap();
        engine
            .meta_store()
            .fail_effect_application
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            engine.delete_memory_with_edges(removed, &[]),
            Err(StorageError::CommittedEffectsPending { .. })
        ));
    }
    let source = fixture.close();
    let before = files(source.path());
    let expected_source = {
        let (_copy, db) =
            crate::storage::validation::inspect_database(&source.path().join("meta.db")).unwrap();
        let read = db.begin_read().unwrap();
        let state = control::validate_existing(&read).unwrap().unwrap();
        assert!(
            removal_ledger::read_export(&read, Some(&state))
                .unwrap()
                .unwrap()
                .entries
                .iter()
                .any(|e| e.memory_uuid == removed.into_inner())
        );
        assert!(
            effects::validate_existing(&read)
                .unwrap()
                .iter()
                .any(|e| !e.canonical_applied)
        );
        state.generation_ref()
    };
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("must-not-exist");
    let err = migrate_schema(
        source.path(),
        &target,
        &MigrationOptions {
            migration_now: 12345,
            expected_source,
            attestations: vec![],
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("pending canonical effects"),
        "{err}"
    );
    assert!(!target.exists());
    assert!(inspect_schema(source.path()).is_err());
    crate::storage::validation::preflight_text(source.path()).unwrap();
    assert_eq!(files(source.path()), before);
    // Only the explicit subsequent serving-engine reopen applies valid recovery.
    let engine = RedbStorageEngine::open(source.path()).unwrap();
    assert!(engine.get_record(removed).unwrap().is_none());
    assert!(engine.load_all_edges().unwrap().is_empty());
    drop(engine);
    let expected_source = inspect_schema(source.path())
        .unwrap()
        .control
        .generation_ref();
    migrate_schema(
        source.path(),
        &target,
        &MigrationOptions {
            migration_now: 12345,
            expected_source,
            attestations: vec![],
        },
    )
    .unwrap();
    assert_eq!(inspect_schema(&target).unwrap().record_count, 1);
}
