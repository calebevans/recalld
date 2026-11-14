use super::*;
use crate::model::{DecayPhase, MemoryId, NamespaceId};
use crate::test_support::{DIM, Fixture, MemorySpec};
use control::{MutationContext, MutationOutcome, MutationScope, WriteReceipt};
use maintenance::{DecayMutationResult, DecaySkip};

fn context(engine: &RedbStorageEngine, key: &str) -> MutationContext {
    let state = engine.control_state().unwrap();
    MutationContext {
        store_id: Some(state.store_id),
        scope: MutationScope::Namespace(NamespaceId::new(1)),
        idempotency_key: Some(key.into()),
        expected_generation: Some(state.committed_generation),
        contract_version: 1,
    }
}
fn committed(result: DecayMutationResult) -> WriteReceipt {
    let DecayMutationResult::Committed(receipt) = result else {
        panic!("{result:?}")
    };
    receipt
}
fn ledger(path: &std::path::Path, id: MemoryId, receipt: &WriteReceipt, mask: u8) {
    let (_copy, db) = validation::inspect_database(&path.join("meta.db")).unwrap();
    let tx = db.begin_read().unwrap();
    let state = control::validate_existing(&tx).unwrap().unwrap();
    let export = metadata::removal_ledger::read_export(&tx, Some(&state))
        .unwrap()
        .unwrap();
    let entry = export
        .entries
        .iter()
        .find(|e| e.memory_uuid == id.into_inner())
        .unwrap();
    assert_eq!(entry.operation_id, receipt.operation_id);
    assert_eq!(entry.generation, receipt.committed_generation);
    assert_eq!(entry.removed_fields_mask, mask);
    assert_eq!(entry.terminal_kind, None);
}

#[test]
fn maintenance_loss_receipts_replay_and_reopen_without_losing_legacy_tags_or_vector() {
    let f = Fixture::new();
    let (id, receipt, ctx, expected, namespace, slot) = {
        let mut engine = f.storage.write().unwrap();
        let ctx = context(&engine, "insert");
        let input = insertion::InsertMemory {
            id: None,
            summary: "lost summary".into(),
            full_text: Some("lost text".into()),
            tags: vec![crate::model::Tag::new("entity/retained").unwrap()],
            initial_stability: None,
            created_at: None,
            embedding: Some(vec![1., 0., 0., 0.]),
            parent_id: None,
            supersedes: None,
        };
        let receipt = engine
            .insert_memory_with_receipt(&ctx, &input, &[], 0)
            .unwrap();
        let id = insertion::inserted_id(&receipt).unwrap();
        let namespace = engine.get_namespace(NamespaceId::new(1)).unwrap().unwrap();
        let full = engine.get_record(id).unwrap().unwrap();
        let ctx = context(&engine, "summary");
        let summary = committed(
            engine
                .apply_legacy_decay_with_receipt(
                    &ctx,
                    id,
                    &full,
                    &namespace,
                    DecayPhase::Summary,
                    0.5,
                    0.5,
                )
                .unwrap(),
        );
        assert!(matches!(
            summary.outcome,
            MutationOutcome::LifecycleMaterialized {
                removed_fields_mask: metadata::removal_ledger::FULL_TEXT,
                ..
            }
        ));
        let expected = engine.get_record(id).unwrap().unwrap();
        assert_eq!((expected.text_offset, expected.text_length), (0, 0));
        let ctx = context(&engine, "ghost");
        let receipt = committed(
            engine
                .apply_legacy_decay_with_receipt(
                    &ctx,
                    id,
                    &expected,
                    &namespace,
                    DecayPhase::Ghost,
                    0.2,
                    0.2,
                )
                .unwrap(),
        );
        assert!(matches!(
            receipt.outcome,
            MutationOutcome::LifecycleMaterialized {
                removed_fields_mask: metadata::removal_ledger::SUMMARY,
                ..
            }
        ));
        let ghost = engine.get_record(id).unwrap().unwrap();
        assert!(ghost.summary.is_empty());
        assert_eq!(ghost.tags, full.tags);
        assert_eq!(ghost.vector_slot, full.vector_slot);
        assert!(
            !engine
                .vector_manager()
                .get(namespace.id)
                .unwrap()
                .is_free(ghost.vector_slot)
        );
        assert!(
            engine
                .meta_store()
                .pending_effects()
                .unwrap()
                .iter()
                .all(|e| e.canonical_applied && e.releases.is_empty())
        );
        assert!(
            committed(
                engine
                    .apply_legacy_decay_with_receipt(
                        &ctx,
                        id,
                        &expected,
                        &namespace,
                        DecayPhase::Ghost,
                        0.2,
                        0.2
                    )
                    .unwrap()
            )
            .replayed
        );
        let serialized = serde_json::to_string(&receipt).unwrap();
        assert!(!serialized.contains("lost") && !serialized.contains("entity/"));
        (id, receipt, ctx, expected, namespace, ghost.vector_slot)
    };
    let dir = f.close();
    ledger(
        dir.path(),
        id,
        &receipt,
        metadata::removal_ledger::FULL_TEXT | metadata::removal_ledger::SUMMARY,
    );
    for _ in 0..2 {
        let engine = RedbStorageEngine::open(dir.path()).unwrap();
        assert_eq!(
            engine.get_record(id).unwrap().unwrap().phase,
            DecayPhase::Ghost
        );
        assert_eq!(
            engine.get_vector(namespace.id, slot).unwrap().unwrap(),
            vec![1., 0., 0., 0.]
        );
        let replay = committed(
            engine
                .apply_legacy_decay_with_receipt(
                    &ctx,
                    id,
                    &expected,
                    &namespace,
                    DecayPhase::Ghost,
                    0.2,
                    0.2,
                )
                .unwrap(),
        );
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
    }
}

#[tokio::test]
async fn maintenance_skip_reasons_leave_generation_and_effects_unchanged() {
    let f = Fixture::new();
    let id = f.insert_memory().await;
    let exempt = f
        .insert_memory_with(MemorySpec {
            is_permastore: true,
            ..MemorySpec::new(NamespaceId::new(1))
        })
        .await;
    let engine = f.storage.write().unwrap();
    let namespace = engine.get_namespace(NamespaceId::new(1)).unwrap().unwrap();
    let expected = engine.get_record(id).unwrap().unwrap();
    let ctx = context(&engine, "noop");
    let before = engine.control_state().unwrap();
    assert!(matches!(
        engine
            .apply_legacy_decay_with_receipt(
                &ctx,
                id,
                &expected,
                &namespace,
                expected.phase,
                expected.strength,
                expected.decay_strength
            )
            .unwrap(),
        DecayMutationResult::Skipped(DecaySkip::Unchanged)
    ));
    assert_eq!(engine.control_state().unwrap(), before);
    engine
        .update_access(id, 123, crate::model::AccessKind::DirectRetrieval)
        .unwrap();
    let before = engine.control_state().unwrap();
    assert!(matches!(
        engine
            .apply_legacy_decay_with_receipt(
                &context(&engine, "stale"),
                id,
                &expected,
                &namespace,
                DecayPhase::Summary,
                0.5,
                0.5
            )
            .unwrap(),
        DecayMutationResult::Skipped(DecaySkip::Stale)
    ));
    let expected = engine.get_record(exempt).unwrap().unwrap();
    assert!(matches!(
        engine
            .apply_legacy_decay_with_receipt(
                &context(&engine, "exempt"),
                exempt,
                &expected,
                &namespace,
                DecayPhase::Summary,
                0.5,
                0.5
            )
            .unwrap(),
        DecayMutationResult::Skipped(DecaySkip::Exempt)
    ));
    assert_eq!(engine.control_state().unwrap(), before);
    assert!(engine.meta_store().pending_effects().unwrap().is_empty());
}

#[tokio::test]
async fn maintenance_unknown_commit_recovers_same_receipt() {
    let f = Fixture::new();
    let id = f.insert_memory().await;
    let (ctx, expected, namespace) = {
        let engine = f.storage.write().unwrap();
        let expected = engine.get_record(id).unwrap().unwrap();
        let namespace = engine.get_namespace(NamespaceId::new(1)).unwrap().unwrap();
        let ctx = context(&engine, "unknown");
        metadata::effects::FAIL_AFTER_RECORD_COMMIT.with(|flag| flag.set(true));
        assert!(matches!(
            engine.apply_legacy_decay_with_receipt(
                &ctx,
                id,
                &expected,
                &namespace,
                DecayPhase::Summary,
                0.5,
                0.5
            ),
            Err(StorageError::CommitOutcomeUnknown { .. })
        ));
        assert!(engine.get_record(id).is_err());
        (ctx, expected, namespace)
    };
    let dir = f.close();
    let engine = RedbStorageEngine::open(dir.path()).unwrap();
    let receipt = committed(
        engine
            .apply_legacy_decay_with_receipt(
                &ctx,
                id,
                &expected,
                &namespace,
                DecayPhase::Summary,
                0.5,
                0.5,
            )
            .unwrap(),
    );
    assert!(receipt.replayed);
    assert_eq!(
        receipt.committed_generation.get(),
        ctx.expected_generation.unwrap().get() + 1
    );
    drop(engine);
    ledger(
        dir.path(),
        id,
        &receipt,
        metadata::removal_ledger::FULL_TEXT,
    );
}

#[tokio::test]
async fn actual_manual_sweep_awaits_publication_and_keeps_receipt_on_failure() {
    use crate::api::FsrsEngine as _;
    let f = Fixture::new();
    let id = f.insert_memory().await;
    *f.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM + 1);
    let adapter = crate::api::adapters::FsrsEngineAdapter::new(
        f.storage.clone(),
        f.cache.clone(),
        f.graph.clone(),
        std::sync::Arc::new(crate::config::RecalldConfig::default()),
        false,
    )
    .with_indexes(
        f.vector_index.clone(),
        f.fts_index.clone(),
        f.entity_index.clone(),
    )
    .with_clock(std::sync::Arc::new(crate::time::ManualClock::new(i64::MAX)));
    let error = adapter.trigger_sweep(None).await.unwrap_err();
    let error = error.downcast::<StorageError>().unwrap();
    let StorageError::CommittedMutation { receipt, .. } = *error else {
        panic!("{error}")
    };
    assert!(
        matches!(receipt.outcome, MutationOutcome::LifecycleMaterialized { memory_id, .. } if memory_id == id)
    );
    assert_eq!(f.record(id).unwrap().phase, DecayPhase::Summary);
    assert!(
        !f.storage
            .read()
            .unwrap()
            .meta_store()
            .pending_effects()
            .unwrap()
            .is_empty()
    );
    *f.vector_index.write().await = crate::search::FlatVectorIndex::new(DIM);
    let completion = crate::derived_completion::DerivedCompletion {
        storage: f.storage.clone(),
        cache: f.cache.clone(),
        graph: f.graph.clone(),
        vectors: f.vector_index.clone(),
        fts: f.fts_index.clone(),
        entities: f.entity_index.clone(),
    };
    completion.complete().await.unwrap();
    assert!(
        f.storage
            .read()
            .unwrap()
            .meta_store()
            .pending_effects()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn actual_sweep_reports_unchanged_without_receipt_or_generation() {
    use crate::api::FsrsEngine as _;
    let f = Fixture::new();
    let id = f.insert_memory().await;
    f.graph
        .write()
        .await
        .update_node_state(id, DecayPhase::Full, 0.0)
        .unwrap();
    let before = f.storage.read().unwrap().control_state().unwrap();
    let adapter = crate::api::adapters::FsrsEngineAdapter::new(
        f.storage.clone(),
        f.cache.clone(),
        f.graph.clone(),
        std::sync::Arc::new(crate::config::RecalldConfig::default()),
        false,
    )
    .with_indexes(
        f.vector_index.clone(),
        f.fts_index.clone(),
        f.entity_index.clone(),
    );
    let result = adapter.trigger_sweep(Some(0)).await.unwrap();
    assert_eq!(result.unchanged_skipped, 1);
    assert_eq!(result.stale_skipped, 0);
    assert_eq!(f.graph.read().await.get_node(&id).unwrap().strength, 1.0);
    assert_eq!(f.storage.read().unwrap().control_state().unwrap(), before);
    assert!(
        f.storage
            .read()
            .unwrap()
            .meta_store()
            .pending_effects()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn maintenance_process_child() {
    let Some(path) = std::env::var_os("RECALLD_TEST_MAINTENANCE_STORE") else {
        return;
    };
    let id = MemoryId::from_uuid(
        uuid::Uuid::parse_str(&std::env::var("RECALLD_TEST_MAINTENANCE_ID").unwrap()).unwrap(),
    );
    let engine = RedbStorageEngine::open(path).unwrap();
    let expected = engine.get_record(id).unwrap().unwrap();
    let namespace = engine.get_namespace(NamespaceId::new(1)).unwrap().unwrap();
    let ctx = MutationContext {
        expected_generation: None,
        ..context(&engine, "maintenance-crash")
    };
    engine
        .apply_legacy_decay_with_receipt(
            &ctx,
            id,
            &expected,
            &namespace,
            DecayPhase::Summary,
            0.5,
            0.5,
        )
        .unwrap();
    panic!("maintenance crash point not reached");
}

#[tokio::test]
async fn maintenance_interrupted_publication_recovers_without_repeating_phase_loss() {
    for point in [
        "before_metadata_commit",
        "after_metadata_commit",
        "after_edge_commit",
        "after_canonical_ack",
    ] {
        let f = Fixture::new();
        let id = f.insert_memory().await;
        let (expected, namespace, before) = {
            let engine = f.storage.read().unwrap();
            (
                engine.get_record(id).unwrap().unwrap(),
                engine.get_namespace(NamespaceId::new(1)).unwrap().unwrap(),
                engine.control_state().unwrap(),
            )
        };
        let dir = f.close();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::maintenance_tests::maintenance_process_child",
                "--nocapture",
            ])
            .env("RECALLD_TEST_MAINTENANCE_STORE", dir.path())
            .env("RECALLD_TEST_MAINTENANCE_ID", id.to_string())
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
        let mut original_receipt = None;
        for _ in 0..2 {
            let engine = RedbStorageEngine::open(dir.path()).unwrap();
            assert_eq!(
                engine.control_state().unwrap().committed_generation.get(),
                before.committed_generation.get() + u64::from(committed)
            );
            assert_eq!(
                engine.get_record(id).unwrap().unwrap().phase,
                if committed {
                    DecayPhase::Summary
                } else {
                    DecayPhase::Full
                }
            );
            if committed {
                let ctx = MutationContext {
                    expected_generation: None,
                    ..context(&engine, "maintenance-crash")
                };
                let replay = super::maintenance_tests::committed(
                    engine
                        .apply_legacy_decay_with_receipt(
                            &ctx,
                            id,
                            &expected,
                            &namespace,
                            DecayPhase::Summary,
                            0.5,
                            0.5,
                        )
                        .unwrap(),
                );
                assert!(replay.replayed);
                if let Some(old) = original_receipt {
                    assert_eq!(replay.operation_id, old);
                }
                original_receipt = Some(replay.operation_id);
            }
        }
    }
}

#[tokio::test]
async fn maintenance_receipt_and_pending_plan_reject_inconsistent_loss_or_release() {
    let f = Fixture::new();
    let id = f.insert_memory().await;
    let engine = f.storage.write().unwrap();
    let before = engine.get_record(id).unwrap().unwrap();
    let namespace = engine.get_namespace(NamespaceId::new(1)).unwrap().unwrap();
    let receipt = committed(
        engine
            .apply_legacy_decay_with_receipt(
                &context(&engine, "validate"),
                id,
                &before,
                &namespace,
                DecayPhase::Summary,
                0.5,
                0.5,
            )
            .unwrap(),
    );
    let mut stored = control::StoredReceipt {
        request_hash_version: 1,
        request_hash: [0; 32],
        receipt,
    };
    control::validate_receipt(&control::encode(&stored).unwrap()).unwrap();
    let MutationOutcome::LifecycleMaterialized {
        removed_fields_mask,
        ..
    } = &mut stored.receipt.outcome
    else {
        unreachable!()
    };
    *removed_fields_mask = 0;
    assert!(control::validate_receipt(&control::encode(&stored).unwrap()).is_err());
    let mut effect = engine
        .meta_store()
        .pending_effects()
        .unwrap()
        .pop()
        .unwrap();
    let mut rows = engine.scan_all().unwrap();
    metadata::effects::validate_plan(&effect, &rows).unwrap();
    rows[0].1.strength = 0.9;
    assert!(metadata::effects::validate_plan(&effect, &rows).is_err());
    rows[0].1.strength = 0.5;
    effect.releases.push(metadata::effects::VectorRelease {
        namespace_id: namespace.id,
        slot: before.vector_slot,
        memory_id: id,
    });
    assert!(
        metadata::effects::validate_plan(&effect, &rows).is_err(),
        "a retained live/ghost vector must never be released"
    );
}

#[tokio::test]
async fn actual_manual_sweep_preserves_canonical_application_failure_receipt() {
    use crate::api::FsrsEngine as _;
    for phase in [DecayPhase::Full, DecayPhase::Ghost] {
        let f = Fixture::new();
        let id = f
            .insert_memory_with(MemorySpec {
                phase,
                ..MemorySpec::new(NamespaceId::new(1))
            })
            .await;
        let slot = f.record(id).unwrap().vector_slot;
        f.storage
            .read()
            .unwrap()
            .meta_store()
            .fail_effect_application
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let adapter = crate::api::adapters::FsrsEngineAdapter::new(
            f.storage.clone(),
            f.cache.clone(),
            f.graph.clone(),
            std::sync::Arc::new(crate::config::RecalldConfig::default()),
            false,
        )
        .with_indexes(
            f.vector_index.clone(),
            f.fts_index.clone(),
            f.entity_index.clone(),
        );
        let error = adapter
            .trigger_sweep(Some(i64::MAX))
            .await
            .unwrap_err()
            .downcast::<StorageError>()
            .unwrap();
        let StorageError::CommittedMutation { receipt, .. } = *error else {
            panic!("{error}")
        };
        if phase == DecayPhase::Full {
            assert!(
                matches!(receipt.outcome, MutationOutcome::LifecycleMaterialized { memory_id, .. } if memory_id == id)
            );
        } else {
            assert!(matches!(
                receipt.outcome,
                MutationOutcome::GraphChanged {
                    memories_removed: 1,
                    ..
                }
            ));
        }
        drop(adapter);
        let dir = f.close();
        let engine = RedbStorageEngine::open(dir.path()).unwrap();
        if phase == DecayPhase::Full {
            assert_eq!(
                engine.get_record(id).unwrap().unwrap().phase,
                DecayPhase::Summary
            );
        } else {
            assert!(engine.get_record(id).unwrap().is_none());
            assert!(
                engine
                    .vector_manager()
                    .get(NamespaceId::new(1))
                    .unwrap()
                    .is_free(slot)
            );
        }
        assert_eq!(
            engine
                .lookup_operation(
                    receipt.store_id,
                    MutationScope::Namespace(NamespaceId::new(1)),
                    receipt.operation_id
                )
                .unwrap()
                .unwrap(),
            *receipt
        );
    }
}

#[tokio::test]
async fn sweep_counts_known_canonical_transition_when_application_fails() {
    let f = Fixture::new();
    f.insert_memory().await;
    f.storage
        .read()
        .unwrap()
        .meta_store()
        .fail_effect_application
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let result = crate::decay::sweep::DecaySweepRunner::execute_sweep_at(
        &crate::decay::sweep::SweepConfig::default(),
        &crate::decay::DecayConfig::default(),
        &crate::graph::ActivationConfig::default(),
        &f.storage,
        &f.graph,
        &f.cache,
        1.0,
        Some(i64::MAX),
    )
    .await;
    assert_eq!(result.full_to_summary, 1);
    assert_eq!(result.stale_skipped, 0);
    assert!(result.failed_commit.is_some());
    assert_eq!(result.errors.len(), 1);
    assert!(result.uncertain_operation.is_none());
}
