use super::*;
use crate::storage::control::{MutationOutcome, MutationScope};
use crate::test_support::{DIM, Fixture};
use redb::ReadableTable;

fn request() -> InsertMemory {
    InsertMemory {
        id: None,
        summary: "insertion secret".into(),
        full_text: Some("private full text".into()),
        tags: vec![crate::model::Tag::new("entity/insert").unwrap()],
        initial_stability: None,
        created_at: None,
        embedding: Some(vec![1., 0., 0., 0.]),
        parent_id: None,
        supersedes: None,
    }
}
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
fn payloads(engine: &RedbStorageEngine) -> (Vec<u8>, Vec<u8>) {
    (
        std::fs::read(engine.text_store.path()).unwrap(),
        std::fs::read(engine.db_path.join("default/vectors.dat")).unwrap(),
    )
}
fn generation(engine: &RedbStorageEngine) -> u64 {
    engine.control_state().unwrap().committed_generation.get()
}

#[test]
fn insertion_replay_precedes_clock_provider_allocation_and_mutable_defaults() {
    let f = Fixture::new();
    let mut input = request();
    input.embedding = None;
    let (ctx, receipt, id, bytes) = {
        let mut engine = f.storage.write().unwrap();
        let ctx = context(&engine, "insert");
        let before = generation(&engine);
        assert!(engine.lookup_insert_replay(&ctx, &input).unwrap().is_none());
        let receipt = engine
            .insert_memory_with_receipt(&ctx, &input, &[1., 0., 0., 0.], 123)
            .unwrap();
        let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
        assert_eq!(generation(&engine), before + 1);
        assert_eq!(engine.get_record(id).unwrap().unwrap().created_at, 123);
        assert_eq!(engine.get_record(id).unwrap().unwrap().edge_count, 0);
        let bytes = payloads(&engine);
        let replay = engine
            .insert_memory_with_receipt(&ctx, &input, &[f32::NAN], i64::MAX)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(payloads(&engine), bytes);
        let serialized = serde_json::to_string(&receipt).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("private"));
        (ctx, receipt, id, bytes)
    };
    let dir = f.close();
    // Controlled fixture change to mutable namespace defaults, without using
    // the production daemon or pretending that this raw test edit is an API.
    {
        let db = redb::Database::open(dir.path().join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        let table = redb::TableDefinition::<u32, &[u8]>::new("namespaces");
        let mut rows = tx.open_table(table).unwrap();
        let mut ns =
            crate::model::NamespaceConfig::decode(rows.get(1).unwrap().unwrap().value()).unwrap();
        ns.initial_stability = 99.;
        rows.insert(1, serde_json::to_vec(&ns).unwrap().as_slice())
            .unwrap();
        drop(rows);
        tx.commit().unwrap();
    }
    let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
    let replay = engine.lookup_insert_replay(&ctx, &input).unwrap().unwrap();
    assert_eq!(replay.operation_id, receipt.operation_id);
    assert!(replay.replayed);
    assert_eq!(payloads(&engine), bytes);
    // None and explicit equal resolved values remain distinct caller intent.
    let mut changed = input.clone();
    changed.initial_stability = Some(engine.get_record(id).unwrap().unwrap().stability);
    assert!(matches!(
        engine.lookup_insert_replay(&ctx, &changed),
        Err(StorageError::IdempotencyConflict)
    ));
    changed = input.clone();
    changed.created_at = Some(123);
    assert!(matches!(
        engine.lookup_insert_replay(&ctx, &changed),
        Err(StorageError::IdempotencyConflict)
    ));
    // Historical replay after content deletion cannot resurrect or allocate.
    engine.delete_memory(id).unwrap();
    let after_delete = payloads(&engine);
    assert_eq!(
        engine
            .insert_memory_with_receipt(&ctx, &input, &[], i64::MAX)
            .unwrap()
            .operation_id,
        receipt.operation_id
    );
    assert!(engine.get_record(id).unwrap().is_none());
    assert_eq!(payloads(&engine), after_delete);
    let mut reuse = request();
    reuse.id = Some(id);
    let fresh = context(&engine, "reuse");
    assert!(
        engine
            .insert_memory_with_receipt(&fresh, &reuse, &[], 200)
            .is_err()
    );
    assert_eq!(payloads(&engine), after_delete);
}

#[test]
fn caller_identity_bits_conflicts_and_preallocation_rejections() {
    let f = Fixture::new();
    let mut engine = f.storage.write().unwrap();
    let mut input = request();
    input.id = Some(MemoryId::new());
    input.created_at = Some(0);
    input.embedding = Some(vec![1., 0., 0., 0.]);
    let ctx = context(&engine, "caller");
    let receipt = engine
        .insert_memory_with_receipt(&ctx, &input, &[], 999)
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        MutationOutcome::MemoryInserted { created_at: 0, .. }
    ));
    let baseline = payloads(&engine);
    let before = generation(&engine);
    let mut changed = input.clone();
    changed.embedding.as_mut().unwrap()[1] = -0.0;
    assert!(matches!(
        engine.insert_memory_with_receipt(&ctx, &changed, &[], 999),
        Err(StorageError::IdempotencyConflict)
    ));
    changed = input.clone();
    changed.id = None;
    assert!(matches!(
        engine.lookup_insert_replay(&ctx, &changed),
        Err(StorageError::IdempotencyConflict)
    ));
    let foreign = MutationContext {
        store_id: Some(uuid::Uuid::new_v4()),
        ..ctx.clone()
    };
    assert!(matches!(
        engine.lookup_insert_replay(&foreign, &input),
        Err(StorageError::StoreLineageMismatch)
    ));
    for mutate in 0..6 {
        let mut invalid = request();
        match mutate {
            0 => invalid.embedding = Some(vec![1.]),
            1 => invalid.embedding = Some(vec![f32::NAN; DIM]),
            2 => invalid.initial_stability = Some(0.),
            3 => invalid.id = Some(MemoryId::nil()),
            4 => invalid.created_at = Some(i64::MAX),
            _ => invalid.full_text = Some(String::new()),
        }
        let ctx = context(&engine, &format!("invalid{mutate}"));
        assert!(
            engine
                .insert_memory_with_receipt(&ctx, &invalid, &[], 100)
                .is_err()
        );
        assert_eq!(generation(&engine), before);
        assert_eq!(payloads(&engine), baseline);
    }
    // Operation kinds share one key namespace with existing scalar access.
    let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
    assert!(matches!(
        engine.update_access_with_receipt(&ctx, id, 100, AccessKind::DirectRetrieval),
        Err(StorageError::IdempotencyConflict)
    ));
}

#[tokio::test]
async fn explicit_links_are_one_transaction_and_final_target_checks_precede_allocation() {
    let f = Fixture::new();
    let parent = f.insert_memory().await;
    let old = f.insert_memory().await;
    let other = f.create_namespace("other");
    let foreign = f.insert_memory_in(other, &[]).await;
    let mut input = request();
    input.parent_id = Some(parent);
    input.supersedes = Some(old);
    let mut engine = f.storage.write().unwrap();
    let before = generation(&engine);
    let ctx_linked = context(&engine, "linked");
    let receipt = engine
        .insert_memory_with_receipt(&ctx_linked, &input, &[], 100)
        .unwrap();
    let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
    assert_eq!(generation(&engine), before + 1);
    assert!(matches!(
        receipt.outcome,
        MutationOutcome::MemoryInserted { edges_put: 2, .. }
    ));
    assert_eq!(engine.get_record(parent).unwrap().unwrap().edge_count, 1);
    assert_eq!(engine.get_record(id).unwrap().unwrap().edge_count, 1);
    let edges = engine.load_all_edges().unwrap();
    assert_eq!(edges.len(), 2);
    let baseline = payloads(&engine);
    let before = generation(&engine);
    // A stale caller plan naming an already superseded target is rejected.
    let ctx_branch = context(&engine, "branch");
    assert!(
        engine
            .insert_memory_with_receipt(&ctx_branch, &input, &[], 100)
            .is_err()
    );
    input.supersedes = None;
    input.parent_id = Some(foreign);
    let ctx_foreign = context(&engine, "foreign");
    assert!(
        engine
            .insert_memory_with_receipt(&ctx_foreign, &input, &[], 100)
            .is_err()
    );
    input.parent_id = Some(MemoryId::new());
    let ctx_missing = context(&engine, "missing");
    assert!(
        engine
            .insert_memory_with_receipt(&ctx_missing, &input, &[], 100)
            .is_err()
    );
    assert_eq!(generation(&engine), before);
    assert_eq!(payloads(&engine), baseline);
    assert_eq!(engine.load_all_edges().unwrap(), edges);
}

#[test]
fn insertion_canonical_effect_failure_retains_receipt_and_reopens_without_slot_reuse() {
    let f = Fixture::new();
    let input = request();
    let (receipt, id) = {
        let mut engine = f.storage.write().unwrap();
        engine
            .meta_store
            .fail_effect_application
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = context(&engine, "failure");
        let error = engine
            .insert_memory_with_receipt(&ctx, &input, &[], 123)
            .unwrap_err();
        let StorageError::CommittedInsertion { receipt, .. } = error else {
            panic!("{error}")
        };
        let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
        assert!(engine.get_record(id).is_err());
        (receipt, id)
    };
    let dir = f.close();
    let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
    let record = engine.get_record(id).unwrap().unwrap();
    assert_eq!(record.summary, input.summary);
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
    let ctx_next = context(&engine, "next");
    let fresh = engine
        .insert_memory_with_receipt(&ctx_next, &request(), &[], 124)
        .unwrap();
    let next = engine
        .get_record(crate::storage::insertion::inserted_id(&fresh).unwrap())
        .unwrap()
        .unwrap();
    assert_ne!(record.vector_slot, next.vector_slot);
    assert_eq!(
        engine
            .get_vector(NamespaceId::new(1), record.vector_slot)
            .unwrap()
            .unwrap(),
        vec![1., 0., 0., 0.]
    );
}

#[test]
fn insertion_process_child() {
    let Some(path) = std::env::var_os("RECALLD_TEST_INSERT_STORE") else {
        return;
    };
    let mut engine = RedbStorageEngine::open(path).unwrap();
    let mut input = request();
    input.id = Some(MemoryId::from_bytes([7; 16]));
    let ctx = MutationContext {
        expected_generation: None,
        ..context(&engine, "crash")
    };
    engine
        .insert_memory_with_receipt(&ctx, &input, &[], 123)
        .unwrap();
    panic!("insertion crash boundary was not reached");
}

#[test]
fn insertion_process_crashes_replay_or_reclaim_without_double_allocation() {
    for point in [
        "after_insert_payload_sync",
        "before_metadata_commit",
        "after_metadata_commit",
        "after_edge_commit",
        "after_canonical_ack",
    ] {
        let f = Fixture::new();
        let generation_before = generation(&f.storage.read().unwrap());
        let dir = f.close();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::engine::insertion::tests::insertion_process_child",
                "--nocapture",
            ])
            .env("RECALLD_TEST_INSERT_STORE", dir.path())
            .env("RECALLD_TEST_EFFECT_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(91),
            "{point}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let committed = !matches!(
            point,
            "after_insert_payload_sync" | "before_metadata_commit"
        );
        let id = MemoryId::from_bytes([7; 16]);
        let mut input = request();
        input.id = Some(id);
        let mut original_operation = None;
        for _ in 0..2 {
            let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
            assert_eq!(
                engine.get_record(id).unwrap().is_some(),
                committed,
                "{point}"
            );
            assert_eq!(
                generation(&engine),
                generation_before + u64::from(committed)
            );
            let ctx = MutationContext {
                expected_generation: None,
                ..context(&engine, "crash")
            };
            if committed {
                let receipt = engine.lookup_insert_replay(&ctx, &input).unwrap().unwrap();
                assert!(receipt.replayed);
                if let Some(old) = original_operation {
                    assert_eq!(receipt.operation_id, old)
                }
                original_operation = Some(receipt.operation_id);
                let bytes = payloads(&engine);
                assert_eq!(
                    engine
                        .insert_memory_with_receipt(&ctx, &input, &[f32::NAN], i64::MAX)
                        .unwrap()
                        .operation_id,
                    receipt.operation_id
                );
                assert_eq!(payloads(&engine), bytes);
                assert_eq!(
                    engine.get_vector(NamespaceId::new(1), 0).unwrap().unwrap(),
                    vec![1., 0., 0., 0.]
                );
            } else {
                assert!(engine.lookup_insert_replay(&ctx, &input).unwrap().is_none());
                assert!(
                    engine
                        .vector_manager
                        .get(NamespaceId::new(1))
                        .unwrap()
                        .is_free(0)
                );
            }
        }
        let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
        let ctx = context(&engine, "fresh");
        let receipt = engine
            .insert_memory_with_receipt(&ctx, &request(), &[], 124)
            .unwrap();
        let fresh = engine
            .get_record(crate::storage::insertion::inserted_id(&receipt).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            fresh.vector_slot,
            u32::from(committed),
            "uncommitted staging must be reclaimable; committed slots must remain owned"
        );
    }
}

#[test]
fn insertion_pending_plan_preflight_rejects_before_original_store_changes() {
    let f = Fixture::new();
    {
        let mut engine = f.storage.write().unwrap();
        engine
            .meta_store
            .fail_effect_application
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = context(&engine, "malformed");
        assert!(matches!(
            engine.insert_memory_with_receipt(&ctx, &request(), &[], 123),
            Err(StorageError::CommittedInsertion { .. })
        ));
    }
    let dir = f.close();
    {
        let db = redb::Database::open(dir.path().join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        // Keep receipt/effect internally consistent, corrupt the canonical row
        // only. Pure pending-plan validation must detect this before replay.
        let mut rows = tx
            .open_table(redb::TableDefinition::<&[u8], &[u8]>::new("memories"))
            .unwrap();
        let (key, mut record) = {
            let (key, value) = rows.iter().unwrap().next().unwrap().unwrap();
            (
                key.value().to_vec(),
                crate::model::DiskRecord::from_bytes(value.value()).unwrap(),
            )
        };
        record.created_at += 1;
        rows.insert(key.as_slice(), record.encode().unwrap().as_slice())
            .unwrap();
        drop(rows);
        tx.commit().unwrap();
    }
    let before = std::fs::read(dir.path().join("meta.db")).unwrap();
    assert!(RedbStorageEngine::open(dir.path()).is_err());
    assert_eq!(std::fs::read(dir.path().join("meta.db")).unwrap(), before);
}

#[test]
fn insertion_unknown_commit_preserves_payload_until_receipt_reconciliation() {
    let f = Fixture::new();
    let input = request();
    let (ctx, operation) = {
        let mut engine = f.storage.write().unwrap();
        let ctx = context(&engine, "uncertain");
        crate::storage::metadata::effects::FAIL_AFTER_RECORD_COMMIT.with(|flag| flag.set(true));
        let error = engine
            .insert_memory_with_receipt(&ctx, &input, &[], 123)
            .unwrap_err();
        let StorageError::CommitOutcomeUnknown { operation_id, .. } = error else {
            panic!("{error}")
        };
        assert!(engine.scan_all().is_err());
        (ctx, operation_id)
    };
    let dir = f.close();
    let mut engine = RedbStorageEngine::open(dir.path()).unwrap();
    let receipt = engine
        .lookup_operation(ctx.store_id.unwrap(), ctx.scope, operation)
        .unwrap()
        .unwrap();
    let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
    let record = engine.get_record(id).unwrap().unwrap();
    assert_eq!(
        engine
            .get_vector(NamespaceId::new(1), record.vector_slot)
            .unwrap()
            .unwrap(),
        input.embedding.clone().unwrap()
    );
    assert_eq!(
        engine
            .get_text(TextRef {
                file_offset: record.text_offset,
                length: record.text_length
            })
            .unwrap(),
        input.full_text
    );
    let before = payloads(&engine);
    assert_eq!(
        engine
            .insert_memory_with_receipt(&ctx, &input, &[], i64::MAX)
            .unwrap()
            .operation_id,
        operation
    );
    assert_eq!(payloads(&engine), before);
}

#[test]
fn insertion_concurrent_same_key_commits_one_uuid_and_generation() {
    let f = Fixture::new();
    let ctx = context(&f.storage.read().unwrap(), "concurrent");
    let before = generation(&f.storage.read().unwrap());
    let start = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let storage = f.storage.clone();
            let ctx = ctx.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                storage
                    .write()
                    .unwrap()
                    .insert_memory_with_receipt(&ctx, &request(), &[], 123)
                    .unwrap()
            })
        })
        .collect();
    start.wait();
    let receipts: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(receipts[0].operation_id, receipts[1].operation_id);
    assert_eq!(receipts.iter().filter(|r| r.replayed).count(), 1);
    let engine = f.storage.read().unwrap();
    assert_eq!(generation(&engine), before + 1);
    assert_eq!(engine.scan_all().unwrap().len(), 1);
}

#[test]
fn insertion_rejects_schema_policy_and_invalid_receipt_numerics() {
    use crate::storage::control::{self, StoredReceipt};
    let f = Fixture::new();
    let mut engine = f.storage.write().unwrap();
    let ctx = context(&engine, "numeric");
    let receipt = engine
        .insert_memory_with_receipt(&ctx, &request(), &[], 123)
        .unwrap();
    for invalid in 0..6 {
        let mut bad = receipt.clone();
        let MutationOutcome::MemoryInserted {
            memory_id,
            created_at,
            initial_stability_bits,
            embedding_dim,
            edges_put,
        } = &mut bad.outcome
        else {
            panic!()
        };
        match invalid {
            0 => *memory_id = MemoryId::nil(),
            1 => *created_at = i64::MAX,
            2 => *initial_stability_bits = f32::NAN.to_bits(),
            3 => *initial_stability_bits = 0,
            4 => *embedding_dim = 8193,
            _ => *edges_put = 3,
        }
        let stored = StoredReceipt {
            request_hash_version: 1,
            request_hash: [0; 32],
            receipt: bad,
        };
        assert!(control::validate_receipt(&control::encode(&stored).unwrap()).is_err());
    }
    // Catalog admission now rejects schema-bearing legacy creation before any
    // payload preparation. The metadata test independently probes insertion.
    let mut config = crate::model::NamespaceConfig::default_namespace(0);
    config.name = "schema".into();
    config.embedding_dim = DIM as u32;
    config.schema = Some(crate::model::schema::NamespaceSchemaV2::from_legacy());
    let before = generation(&engine);
    let text = std::fs::read(engine.text_store.path()).unwrap();
    assert!(matches!(
        engine.create_namespace(&config),
        Err(StorageError::InvalidMutation(_))
    ));
    assert_eq!(generation(&engine), before);
    assert_eq!(std::fs::read(engine.text_store.path()).unwrap(), text);
    assert!(!engine.db_path.join("schema").exists());
}

#[tokio::test]
async fn insertion_uses_canonical_target_after_preflight_and_retains_structural_tombstones() {
    let f = Fixture::new();
    let old = f.insert_memory().await;
    let lost = f.insert_memory().await;
    // Cached graph still reports live endpoints after these canonical changes.
    f.tombstone(old);
    let mut engine = f.storage.write().unwrap();
    engine.delete_memory(lost).unwrap();
    let mut input = request();
    input.parent_id = Some(lost);
    let ctx = context(&engine, "stale");
    let before = generation(&engine);
    let bytes = payloads(&engine);
    assert!(matches!(
        engine.insert_memory_with_receipt(&ctx, &input, &[], 123),
        Err(StorageError::NotFound(_))
    ));
    assert_eq!(generation(&engine), before);
    assert_eq!(payloads(&engine), bytes);
    input.parent_id = None;
    input.supersedes = Some(old);
    let ctx = context(&engine, "retained");
    let receipt = engine
        .insert_memory_with_receipt(&ctx, &input, &[], 123)
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        MutationOutcome::MemoryInserted { edges_put: 1, .. }
    ));
    assert_eq!(
        engine.get_record(old).unwrap().unwrap().phase,
        crate::model::DecayPhase::Tombstone
    );
}

fn reject_zero_insertion(supplied: bool) {
    let f = Fixture::new();
    let mut engine = f.storage.write().unwrap();
    let mut input = request();
    input.embedding = supplied.then(|| vec![0.; DIM]);
    let ctx = context(&engine, "zero");
    let bytes = payloads(&engine);
    let before = generation(&engine);
    let error = engine.insert_memory_with_receipt(&ctx, &input, &[0.; DIM], 123);
    let error = error.expect_err("zero-norm insertion must fail");
    assert!(
        if supplied {
            matches!(error, StorageError::InvalidMutation(_))
        } else {
            matches!(error, StorageError::ProviderVectorUnavailable(_))
        },
        "{error}"
    );
    assert_eq!(generation(&engine), before);
    assert_eq!(payloads(&engine), bytes);
    assert!(engine.scan_all().unwrap().is_empty());
    assert!(engine.lookup_insert_replay(&ctx, &input).unwrap().is_none());
}

#[test]
fn insertion_rejects_zero_supplied_without_mutation() {
    reject_zero_insertion(true);
}

#[test]
fn insertion_rejects_zero_generated_without_mutation() {
    reject_zero_insertion(false);
}

#[test]
fn insertion_vector_boundary_normalizes_both_origins_and_preserves_request_identity() {
    for supplied in [false, true] {
        let f = Fixture::new();
        let mut engine = f.storage.write().unwrap();
        for (index, raw) in [
            vec![3., 4., 0., 0.],
            vec![f32::MAX, f32::MAX, 0., 0.],
            vec![f32::from_bits(1), 0., 0., 0.],
            vec![1., 0., 0., 0.],
        ]
        .into_iter()
        .enumerate()
        {
            let mut input = request();
            input.embedding = supplied.then(|| raw.clone());
            let ctx = context(&engine, &format!("normalized{index}"));
            let receipt = engine
                .insert_memory_with_receipt(&ctx, &input, &raw, 123)
                .unwrap();
            let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
            let slot = engine.get_record(id).unwrap().unwrap().vector_slot;
            let persisted = engine
                .get_vector(NamespaceId::new(1), slot)
                .unwrap()
                .unwrap();
            let norm: f64 = persisted.iter().map(|&v| f64::from(v).powi(2)).sum();
            assert!((norm - 1.).abs() < 1e-6, "{persisted:?}");
            if index == 0 {
                assert_eq!(persisted, vec![0.6, 0.8, 0., 0.]);
            }
            let bytes = payloads(&engine);
            assert_eq!(
                engine
                    .insert_memory_with_receipt(&ctx, &input, &[0.; DIM], i64::MAX)
                    .unwrap()
                    .operation_id,
                receipt.operation_id
            );
            assert_eq!(payloads(&engine), bytes);
            if supplied && (index == 0 || index == 3) {
                let mut changed = input.clone();
                changed.embedding = Some(if index == 0 {
                    vec![6., 8., 0., 0.]
                } else {
                    vec![2., 0., 0., 0.]
                });
                assert!(
                    matches!(
                        engine.insert_memory_with_receipt(&ctx, &changed, &[], 123),
                        Err(StorageError::IdempotencyConflict)
                    ),
                    "normalized-equivalent caller bytes must not alias"
                );
            }
        }
        let bytes = payloads(&engine);
        let before = generation(&engine);
        for (index, invalid) in [
            vec![0.; DIM],
            vec![-0.; DIM],
            vec![f32::NAN; DIM],
            vec![f32::INFINITY; DIM],
            vec![1.; DIM - 1],
        ]
        .into_iter()
        .enumerate()
        {
            let mut input = request();
            input.embedding = supplied.then(|| invalid.clone());
            let ctx = context(&engine, &format!("invalid-vector{index}"));
            let error = engine
                .insert_memory_with_receipt(&ctx, &input, &invalid, 123)
                .unwrap_err();
            assert!(
                if supplied {
                    matches!(error, StorageError::InvalidMutation(_))
                } else {
                    matches!(error, StorageError::ProviderVectorUnavailable(_))
                },
                "{error}"
            );
            assert_eq!(generation(&engine), before);
            assert_eq!(payloads(&engine), bytes);
            assert!(engine.lookup_insert_replay(&ctx, &input).unwrap().is_none());
            assert!(engine.ensure_available().is_ok());
        }
    }
}
