//! Actual legacy transport regressions for canonical forget participation.
use super::*;
use crate::model::{DecayPhase, NamespaceId};
use crate::storage::control::{self, MutationScope};
use crate::storage::metadata::removal_ledger;
use crate::test_support::{DIM, Fixture};

fn assert_tombstone_receipt(path: &std::path::Path, id: crate::model::MemoryId) {
    let (_copy, db) = validation::inspect_database(&path.join("meta.db")).unwrap();
    let read = db.begin_read().unwrap();
    let state = control::validate_existing(&read).unwrap().unwrap();
    let ledger = removal_ledger::read_export(&read, Some(&state))
        .unwrap()
        .unwrap();
    let removal = ledger
        .entries
        .iter()
        .find(|e| e.memory_uuid == id.into_inner())
        .unwrap();
    assert_eq!(
        removal.terminal_kind,
        Some(removal_ledger::TerminalKind::Tombstone)
    );
    assert_eq!(removal.removed_fields_mask, removal_ledger::ALL);
    let receipt_key = control::scoped_key(
        state.store_id,
        MutationScope::Namespace(NamespaceId::new(removal.namespace_id)),
        removal.operation_id.as_bytes(),
    );
    let table = read.open_table(control::RECEIPTS).unwrap();
    let stored = table
        .get(receipt_key.as_slice())
        .unwrap()
        .expect("forget ledger must point to its same-transaction receipt");
    let receipt = control::validate_receipt(stored.value()).unwrap().receipt;
    assert_eq!(receipt.operation_id, removal.operation_id);
    assert_eq!(receipt.committed_generation, removal.generation);
}

#[tokio::test]
async fn legacy_mcp_forget_retains_same_commit_receipt_and_ledger() {
    use crate::mcp::bridge::StorageEngine as _;
    let fixture = Fixture::new();
    let id = fixture.insert_memory().await;
    let adapter = fixture.storage_adapter();
    assert!(adapter.delete_memory(id, None).await.unwrap());
    assert_eq!(fixture.record(id).unwrap().phase, DecayPhase::Tombstone);
    drop(adapter);
    let source = fixture.close();
    assert_tombstone_receipt(source.path(), id);
}

#[tokio::test]
async fn legacy_http_forget_retains_same_commit_receipt_and_ledger() {
    use crate::api::StorageEngine as _;
    let fixture = Fixture::new();
    let id = fixture.insert_memory().await;
    let adapter = crate::api::adapters::StorageEngineAdapter::new(
        fixture.storage.clone(),
        fixture.cache.clone(),
        fixture.vector_index.clone(),
        fixture.fts_index.clone(),
        fixture.entity_index.clone(),
        fixture.graph.clone(),
    );
    assert!(adapter.delete_memory(id, None).await.unwrap());
    drop(adapter);
    let source = fixture.close();
    assert_tombstone_receipt(source.path(), id);
}

#[tokio::test]
async fn legacy_batch_forget_keeps_order_and_uses_receipted_namespace_children() {
    use crate::mcp::bridge::StorageEngine as _;
    let fixture = Fixture::new();
    let other = fixture.create_namespace("other");
    let first = fixture.insert_memory().await;
    let second = fixture.insert_memory_in(other, &[]).await;
    let adapter = fixture.storage_adapter();
    let flags = adapter
        .delete_memories(&[first, crate::model::MemoryId::new(), first, second])
        .await
        .unwrap();
    assert_eq!(flags, [true, false, false, true]);
    drop(adapter);
    let source = fixture.close();
    assert_tombstone_receipt(source.path(), first);
    assert_tombstone_receipt(source.path(), second);
}

#[tokio::test]
async fn forget_reports_committed_not_ready_when_derived_publication_fails() {
    use crate::mcp::bridge::StorageEngine as _;
    let fixture = Fixture::new();
    let forgotten = fixture.insert_memory().await;
    fixture.insert_memory().await;
    // The remaining live vector cannot be rebuilt into this mismatched view.
    *fixture.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM + 1);
    let adapter = fixture.storage_adapter();
    assert!(
        adapter.delete_memory(forgotten, None).await.is_err(),
        "canonical commit must not return ready success when derived publication cannot complete"
    );
    let pending = fixture
        .storage
        .read()
        .unwrap()
        .meta_store()
        .pending_effects()
        .unwrap();
    assert!(
        !pending.is_empty(),
        "committed work must remain durably retryable"
    );
    assert!(pending.iter().all(|p| p.canonical_applied));
    assert_eq!(
        fixture.record(forgotten).unwrap().phase,
        DecayPhase::Tombstone
    );
}

fn context(engine: &RedbStorageEngine, key: &str) -> control::MutationContext {
    let state = engine.control_state().unwrap();
    control::MutationContext {
        store_id: Some(state.store_id),
        scope: MutationScope::Namespace(NamespaceId::new(1)),
        idempotency_key: Some(key.into()),
        expected_generation: Some(state.committed_generation),
        contract_version: 1,
    }
}

#[tokio::test]
async fn tombstone_replay_precedes_cas_and_existence_but_not_lineage() {
    use control::MutationOutcome;
    use tombstone::TombstoneStatus;
    let fixture = Fixture::new();
    let id = fixture.insert_memory().await;
    let (ctx, receipt) = {
        let mut engine = fixture.storage.write().unwrap();
        let ctx = context(&engine, "forget-once");
        let receipt = engine.tombstone_memory_with_receipt(&ctx, id).unwrap();
        assert_eq!(
            receipt.committed_generation.get(),
            ctx.expected_generation.unwrap().get() + 1
        );
        assert!(
            matches!(receipt.outcome, MutationOutcome::MemoryTombstoned { memory_id, status: TombstoneStatus::Tombstoned } if memory_id == id)
        );
        let replay = engine.tombstone_memory_with_receipt(&ctx, id).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        let conflict = control::MutationContext {
            idempotency_key: Some("different-key".into()),
            ..ctx.clone()
        };
        assert!(matches!(
            engine.tombstone_memory_with_receipt(&conflict, id),
            Err(StorageError::GenerationConflict { .. })
        ));
        assert!(matches!(
            engine.tombstone_memory_with_receipt(&ctx, crate::model::MemoryId::new()),
            Err(StorageError::IdempotencyConflict)
        ));
        let foreign = control::MutationContext {
            store_id: Some(uuid::Uuid::new_v4()),
            ..ctx.clone()
        };
        assert!(matches!(
            engine.tombstone_memory_with_receipt(&foreign, id),
            Err(StorageError::StoreLineageMismatch)
        ));
        let already_ctx = context(&engine, "already");
        let already = engine
            .tombstone_memory_with_receipt(&already_ctx, id)
            .unwrap();
        assert!(matches!(
            already.outcome,
            MutationOutcome::MemoryTombstoned {
                status: TombstoneStatus::AlreadyTombstoned,
                ..
            }
        ));
        let before = engine.control_state().unwrap();
        let missing = crate::model::MemoryId::new();
        let missing_ctx = context(&engine, "missing");
        assert!(matches!(
            engine.tombstone_memory_with_receipt(&missing_ctx, missing),
            Err(StorageError::NotFound(_))
        ));
        assert_eq!(engine.control_state().unwrap(), before);
        engine.delete_memory(id).unwrap();
        assert!(
            engine
                .tombstone_memory_with_receipt(&ctx, id)
                .unwrap()
                .replayed
        );
        (ctx, receipt)
    };
    let dir = fixture.close();
    let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
    let replay = engine.tombstone_memory_with_receipt(&ctx, id).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.operation_id, receipt.operation_id);
    assert!(engine.get_record(id).unwrap().is_none());
}

#[tokio::test]
async fn tombstone_unknown_commit_and_pending_application_recover_once() {
    for unknown in [false, true] {
        let fixture = Fixture::new();
        let id = fixture.insert_memory().await;
        let ctx = {
            let mut engine = fixture.storage.write().unwrap();
            let ctx = context(&engine, "uncertain-forget");
            if unknown {
                metadata::effects::FAIL_AFTER_RECORD_COMMIT.with(|flag| flag.set(true));
            } else {
                engine
                    .meta_store()
                    .fail_effect_application
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let error = engine.tombstone_memory_with_receipt(&ctx, id).unwrap_err();
            assert!(if unknown {
                matches!(error, StorageError::CommitOutcomeUnknown { .. })
            } else {
                matches!(error, StorageError::CommittedMutation { .. })
            });
            assert!(engine.get_record(id).is_err());
            ctx
        };
        let dir = fixture.close();
        for _ in 0..2 {
            let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
            let receipt = engine.tombstone_memory_with_receipt(&ctx, id).unwrap();
            assert!(receipt.replayed);
            assert_eq!(
                receipt.committed_generation.get(),
                ctx.expected_generation.unwrap().get() + 1
            );
            assert_eq!(
                engine.get_record(id).unwrap().unwrap().phase,
                DecayPhase::Tombstone
            );
        }
        assert_tombstone_receipt(dir.path(), id);
    }
}

#[tokio::test]
async fn applied_tombstone_release_does_not_free_reused_live_slot_on_reopen() {
    let fixture = Fixture::new();
    let id = fixture.insert_memory().await;
    let (new_id, slot, vector) = {
        let mut engine = fixture.storage.write().unwrap();
        let slot = engine.get_record(id).unwrap().unwrap().vector_slot;
        let old_ctx = context(&engine, "old");
        engine.tombstone_memory_with_receipt(&old_ctx, id).unwrap();
        let input = insertion::InsertMemory {
            id: None,
            summary: "new live owner".into(),
            full_text: None,
            tags: vec![],
            initial_stability: None,
            created_at: None,
            embedding: Some(vec![1., 0., 0., 0.]),
            parent_id: None,
            supersedes: None,
        };
        let new_ctx = context(&engine, "new");
        let receipt = engine
            .insert_memory_with_receipt(&new_ctx, &input, &[], 123)
            .unwrap();
        let new_id = insertion::inserted_id(&receipt).unwrap();
        assert_eq!(
            engine.get_record(new_id).unwrap().unwrap().vector_slot,
            slot
        );
        assert!(
            engine
                .meta_store()
                .pending_effects()
                .unwrap()
                .iter()
                .all(|e| e.canonical_applied)
        );
        (new_id, slot, input.embedding.unwrap())
    };
    let dir = fixture.close();
    for _ in 0..2 {
        let engine = RedbStorageEngine::open(dir.path()).unwrap();
        assert_eq!(
            engine.get_record(new_id).unwrap().unwrap().vector_slot,
            slot
        );
        assert_eq!(
            engine
                .get_vector(NamespaceId::new(1), slot)
                .unwrap()
                .unwrap(),
            vector
        );
    }
}

#[tokio::test]
async fn pending_tombstone_release_rejects_another_live_slot_owner() {
    let fixture = Fixture::new();
    let id = fixture.insert_memory().await;
    let other = fixture.insert_memory().await;
    let mut engine = fixture.storage.write().unwrap();
    let release_ctx = context(&engine, "release");
    engine
        .tombstone_memory_with_receipt(&release_ctx, id)
        .unwrap();
    let mut effect = engine
        .meta_store()
        .pending_effects()
        .unwrap()
        .into_iter()
        .find(|e| {
            matches!(
                e.receipt.outcome,
                control::MutationOutcome::MemoryTombstoned { .. }
            )
        })
        .unwrap();
    effect.canonical_applied = false;
    let mut records = engine.scan_all().unwrap();
    // Sanitized corrupt-plan fixture: a still-live record owns the delayed slot.
    records
        .iter_mut()
        .find(|(key, _)| *key == other)
        .unwrap()
        .1
        .vector_slot = effect.releases[0].slot;
    assert!(matches!(
        metadata::effects::validate_plan(&effect, &records),
        Err(StorageError::OwnershipMismatch(_))
    ));
    records.retain(|(key, _)| *key != other);
    metadata::effects::validate_plan(&effect, &records).unwrap();
    effect.releases[0].slot += 1;
    assert!(metadata::effects::validate_plan(&effect, &records).is_err());
}

#[test]
fn tombstone_process_child() {
    let Some(path) = std::env::var_os("RECALLD_TEST_TOMBSTONE_STORE") else {
        return;
    };
    let id = crate::model::MemoryId::from_bytes(
        uuid::Uuid::parse_str(&std::env::var("RECALLD_TEST_TOMBSTONE_ID").unwrap())
            .unwrap()
            .into_bytes(),
    );
    let mut engine = RedbStorageEngine::open(path).unwrap();
    let ctx = control::MutationContext {
        expected_generation: None,
        ..context(&engine, "crash-forget")
    };
    engine.tombstone_memory_with_receipt(&ctx, id).unwrap();
    panic!("tombstone crash boundary was not reached");
}

#[tokio::test]
async fn tombstone_process_publication_boundaries_recover_without_double_release() {
    for point in [
        "before_metadata_commit",
        "after_metadata_commit",
        "after_edge_commit",
        "after_vector_sync",
        "after_canonical_ack",
    ] {
        let fixture = Fixture::new();
        let id = fixture.insert_memory().await;
        let before = fixture
            .storage
            .read()
            .unwrap()
            .control_state()
            .unwrap()
            .committed_generation;
        let dir = fixture.close();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::tombstone_tests::tombstone_process_child",
                "--nocapture",
            ])
            .env("RECALLD_TEST_TOMBSTONE_STORE", dir.path())
            .env("RECALLD_TEST_TOMBSTONE_ID", id.to_string())
            .env("RECALLD_TEST_EFFECT_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(91),
            "{point}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let committed = point != "before_metadata_commit";
        let mut original_operation = None;
        for _ in 0..2 {
            let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
            let record = engine.get_record(id).unwrap().unwrap();
            assert_eq!(record.phase == DecayPhase::Tombstone, committed, "{point}");
            assert_eq!(
                engine.control_state().unwrap().committed_generation.get(),
                before.get() + u64::from(committed)
            );
            if committed {
                let ctx = control::MutationContext {
                    expected_generation: None,
                    ..context(&engine, "crash-forget")
                };
                let receipt = engine.tombstone_memory_with_receipt(&ctx, id).unwrap();
                assert!(receipt.replayed);
                if let Some(old) = original_operation {
                    assert_eq!(receipt.operation_id, old);
                }
                original_operation = Some(receipt.operation_id);
            } else {
                assert!(
                    engine
                        .get_vector(NamespaceId::new(1), record.vector_slot)
                        .unwrap()
                        .is_some()
                );
            }
        }
        if committed {
            assert_tombstone_receipt(dir.path(), id);
        }
    }
}

#[tokio::test]
async fn forget_retains_structural_edges_but_removes_live_search_payload() {
    use crate::mcp::bridge::StorageEngine as _;
    let fixture = Fixture::new();
    let parent = fixture.insert_memory().await;
    let id = fixture.insert_memory().await;
    let edge = edges::PersistedEdge {
        source: parent,
        target: id,
        edge_type: crate::model::EdgeType::ParentChild,
        weight: 1.0,
        auto_created: false,
        created_at: 1,
    };
    {
        let engine = fixture.storage.write().unwrap();
        engine
            .put_edges_with_effects(std::slice::from_ref(&edge))
            .unwrap();
    }
    let adapter = fixture.storage_adapter();
    assert!(adapter.delete_memory(id, None).await.unwrap());
    let engine = fixture.storage.read().unwrap();
    assert_eq!(engine.load_all_edges().unwrap(), vec![edge]);
    let record = engine.get_record(id).unwrap().unwrap();
    assert!(record.summary.is_empty() && record.tags.is_empty());
    assert_eq!((record.text_offset, record.text_length), (0, 0));
    assert!(
        engine
            .vector_manager()
            .get(NamespaceId::new(1))
            .unwrap()
            .is_free(record.vector_slot)
    );
    assert!(engine.meta_store().pending_effects().unwrap().is_empty());
}

#[tokio::test]
async fn http_forget_error_carries_committed_receipt_and_503() {
    use crate::api::StorageEngine as _;
    use axum::response::IntoResponse;
    let fixture = Fixture::new();
    let id = fixture.insert_memory().await;
    fixture.insert_memory().await;
    *fixture.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM + 1);
    let adapter = crate::api::adapters::StorageEngineAdapter::new(
        fixture.storage.clone(),
        fixture.cache.clone(),
        fixture.vector_index.clone(),
        fixture.fts_index.clone(),
        fixture.entity_index.clone(),
        fixture.graph.clone(),
    );
    let response =
        crate::api::AppError::from(adapter.delete_memory(id, None).await.unwrap_err()).into_response();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "INDEX_NOT_READY");
    assert_eq!(body["commitState"], "committed");
    assert_eq!(body["ready"], false);
    assert_eq!(body["receipt"]["outcome"]["memoryId"], id.to_string());
    assert_eq!(fixture.record(id).unwrap().phase, DecayPhase::Tombstone);
    // Retrying the legacy operation completes the existing work before false.
    *fixture.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM);
    assert!(!adapter.delete_memory(id, None).await.unwrap());
    assert!(
        fixture
            .storage
            .read()
            .unwrap()
            .meta_store()
            .pending_effects()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn actual_batch_tool_marks_committed_failure_as_error_and_stops_later_items() {
    let fixture = Fixture::new();
    let first = fixture.insert_memory().await;
    let later = fixture.insert_memory().await;
    *fixture.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM + 1);
    let mut bridge = crate::test_support::StubBridge::default().into_bridge();
    bridge.storage = std::sync::Arc::new(fixture.storage_adapter());
    let result = crate::mcp::tools::dispatch_tool(
        &bridge,
        "forget_memories",
        serde_json::json!({ "ids": [first.to_string(), later.to_string()] }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(encoded.contains("commitState=committed") && encoded.contains("receipt="));
    assert_eq!(fixture.record(first).unwrap().phase, DecayPhase::Tombstone);
    assert_eq!(fixture.record(later).unwrap().phase, DecayPhase::Full);
}
