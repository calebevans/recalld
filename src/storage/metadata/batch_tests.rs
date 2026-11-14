use super::*;
use crate::storage::StorageEngine as _;
use crate::test_support::Fixture;

fn process_input(engine: &crate::storage::RedbStorageEngine) -> crate::storage::batch::BatchInput {
    crate::storage::batch::BatchInput {
        result_version: 1,
        kind: BatchKind::Insert,
        items: vec![item()],
        default_namespace: "default".into(),
        store_id: Some(engine.control_state().unwrap().store_id),
        idempotency_key: Some("process-batch".into()),
        batch_operation_id: None,
    }
}

#[test]
fn batch_process_child() {
    let Some(path) = std::env::var_os("RECALLD_TEST_BATCH_STORE") else {
        return;
    };
    let mut engine = crate::storage::RedbStorageEngine::open(path).unwrap();
    let input = process_input(&engine);
    let plans: Vec<_> = crate::mcp::batches::prepare(&input)
        .unwrap()
        .into_iter()
        .map(|p| p.plan)
        .collect();
    let p = engine
        .meta_store()
        .reserve_batch(&input.context(), BatchKind::Insert, &plans)
        .unwrap();
    let child = p.child(0).unwrap();
    let BatchItemPlan::Insert { input, .. } = &plans[0] else {
        panic!()
    };
    engine
        .insert_memory_reserved(&child.context(), input, &[], 123, Some(&child))
        .unwrap();
    panic!("batch crash boundary was not reached");
}

#[test]
fn batch_process_crashes_reconcile_parent_and_child_without_duplicate_mutation() {
    for point in [
        "before_batch_commit",
        "after_batch_commit",
        "after_insert_payload_sync",
        "before_metadata_commit",
        "after_metadata_commit",
        "after_edge_commit",
        "after_canonical_ack",
    ] {
        let f = Fixture::new();
        let before = f
            .storage
            .read()
            .unwrap()
            .control_state()
            .unwrap()
            .committed_generation
            .get();
        let dir = f.close();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::metadata::batch::tests::batch_process_child",
                "--nocapture",
            ])
            .env("RECALLD_TEST_BATCH_STORE", dir.path())
            .env("RECALLD_TEST_EFFECT_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(91),
            "{point}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let mut engine = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
        let parent_committed = point != "before_batch_commit";
        let child_committed = matches!(
            point,
            "after_metadata_commit" | "after_edge_commit" | "after_canonical_ack"
        );
        assert_eq!(
            engine.control_state().unwrap().committed_generation.get(),
            before + u64::from(parent_committed) + u64::from(child_committed),
            "{point}"
        );
        assert_eq!(
            engine.scan_all().unwrap().len(),
            usize::from(child_committed)
        );
        let input = process_input(&engine);
        let plans: Vec<_> = crate::mcp::batches::prepare(&input)
            .unwrap()
            .into_iter()
            .map(|p| p.plan)
            .collect();
        let p = engine
            .meta_store()
            .reserve_batch(&input.context(), BatchKind::Insert, &plans)
            .unwrap();
        assert_eq!(p.reservation_receipt.replayed, parent_committed);
        let child = p.child(0).unwrap();
        let BatchItemPlan::Insert { input: record, .. } = &plans[0] else {
            panic!()
        };
        let receipt = engine
            .insert_memory_reserved(&child.context(), record, &[], 123, Some(&child))
            .unwrap();
        assert_eq!(receipt.replayed, child_committed);
        assert_eq!(receipt.operation_id, p.items[0].operation_id);
        assert_eq!(engine.scan_all().unwrap().len(), 1);
        assert_eq!(engine.meta_store().pending_batch_items().unwrap(), 0);
        let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
        drop(engine);
        let mut engine = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
        let replay = engine
            .insert_memory_reserved(
                &child.context(),
                record,
                &[f32::NAN],
                i64::MAX,
                Some(&child),
            )
            .unwrap();
        assert_eq!(crate::storage::insertion::inserted_id(&replay).unwrap(), id);
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(
            engine.control_state().unwrap().committed_generation.get(),
            before + 2
        );
    }
}

fn request(
    f: &Fixture,
    key: &str,
    items: Vec<serde_json::Value>,
) -> (MutationContext, Vec<BatchItemPlan>) {
    let state = f.storage.read().unwrap().control_state().unwrap();
    let input = crate::storage::batch::BatchInput {
        result_version: 1,
        kind: BatchKind::Insert,
        items,
        default_namespace: "default".into(),
        store_id: Some(state.store_id),
        idempotency_key: Some(key.into()),
        batch_operation_id: None,
    };
    let mut ctx = input.context();
    ctx.expected_generation = Some(state.committed_generation);
    let plans = crate::mcp::batches::prepare(&input)
        .unwrap()
        .into_iter()
        .map(|p| p.plan)
        .collect();
    (ctx, plans)
}
fn item() -> serde_json::Value {
    serde_json::json!({"summary":"batch secret","fullText":"batch payload","embedding":[1.,0.,0.,0.],"checkDuplicates":false})
}

#[test]
fn reservation_is_a_real_write_even_when_all_items_invalid_and_replay_precedes_cas() {
    let f = Fixture::new();
    let (ctx, plans) = request(
        &f,
        "invalid",
        vec![serde_json::json!(17), serde_json::json!({"summary":false})],
    );
    let engine = f.storage.read().unwrap();
    let store = engine.meta_store();
    let before = store.control_state().unwrap().committed_generation.get();
    let p = store
        .reserve_batch(&ctx, BatchKind::Insert, &plans)
        .unwrap();
    assert_eq!(p.unresolved(), 0);
    assert!(
        p.items
            .iter()
            .all(|i| matches!(i.state, BatchItemState::Error { .. }))
    );
    assert_eq!(
        store.control_state().unwrap().committed_generation.get(),
        before + 1
    );
    let replay = store
        .reserve_batch(&ctx, BatchKind::Insert, &plans)
        .unwrap();
    assert!(replay.reservation_receipt.replayed);
    assert_eq!(
        replay.reservation_receipt.operation_id,
        p.reservation_receipt.operation_id
    );
    assert_eq!(replay.items, p.items);
    assert!(matches!(
        store.reserve_batch(&ctx, BatchKind::Forget, &plans),
        Err(StorageError::IdempotencyConflict)
    ));
    assert_eq!(
        store.control_state().unwrap().committed_generation.get(),
        before + 1
    );
    assert!(store.scan_all().unwrap().is_empty());
    let tx = store.db.begin_read().unwrap();
    assert_eq!(
        validate_existing(&tx, Some(&store.control_state().unwrap())).unwrap(),
        vec![p]
    );
}

#[test]
fn child_outcome_commits_with_record_and_lost_ack_reopens_without_reallocation() {
    let f = Fixture::new();
    let path = f.path().to_owned();
    let (ctx, plans) = request(&f, "child", vec![item(), item()]);
    let mut engine = f.storage.write().unwrap();
    let p = engine
        .meta_store()
        .reserve_batch(&ctx, BatchKind::Insert, &plans)
        .unwrap();
    assert_eq!(engine.meta_store().pending_batch_items().unwrap(), 2);
    let child = p.child(0).unwrap();
    let BatchItemPlan::Insert { input, .. } = &plans[0] else {
        panic!()
    };
    let later = p.child(1).unwrap();
    let before = engine.control_state().unwrap();
    assert!(matches!(
        engine.insert_memory_reserved(&later.context(), input, &[], 123, Some(&later)),
        Err(StorageError::InvalidMutation(_))
    ));
    assert_eq!(engine.control_state().unwrap(), before);
    assert!(engine.scan_all().unwrap().is_empty());
    engine
        .meta_store()
        .fail_batch_commit
        .store(3, Ordering::SeqCst);
    let error = engine
        .insert_memory_reserved(&child.context(), input, &[], 123, Some(&child))
        .unwrap_err();
    assert!(
        matches!(error,StorageError::CommitOutcomeUnknown{operation_id,..} if operation_id==p.items[0].operation_id)
    );
    assert!(
        engine
            .meta_store()
            .lookup_batch(
                p.reservation_receipt.store_id,
                p.reservation_receipt.operation_id
            )
            .is_err()
    );
    drop(engine);
    let dir = f.close();
    let mut engine = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
    let after = engine.control_state().unwrap().committed_generation;
    let text_path = path.join(
        engine
            .meta_store()
            .text_selection_for_recovery()
            .unwrap()
            .unwrap()
            .filename,
    );
    let before_text = std::fs::read(&text_path).unwrap();
    let before_vector = std::fs::read(path.join("default/vectors.dat")).unwrap();
    let replay = engine
        .meta_store()
        .reserve_batch(&ctx, BatchKind::Insert, &plans)
        .unwrap();
    assert!(matches!(replay.items[0].state, BatchItemState::Ok { .. }));
    assert_eq!(engine.meta_store().pending_batch_items().unwrap(), 1);
    let r = engine
        .insert_memory_reserved(&child.context(), input, &[f32::NAN], i64::MAX, Some(&child))
        .unwrap();
    assert!(r.replayed);
    assert_eq!(r.operation_id, p.items[0].operation_id);
    assert_eq!(engine.control_state().unwrap().committed_generation, after);
    assert_eq!(std::fs::read(&text_path).unwrap(), before_text);
    assert_eq!(
        std::fs::read(path.join("default/vectors.dat")).unwrap(),
        before_vector
    );
    assert_eq!(engine.scan_all().unwrap().len(), 1);
    let receipt_json = serde_json::to_string(&replay).unwrap();
    assert!(!receipt_json.contains("batch secret"));
    assert!(!receipt_json.contains("batch payload"));
    // No operation may reinterpret another slot's input under its capability.
    let mut changed = input.clone();
    changed.summary = "changed".into();
    assert!(matches!(
        engine.insert_memory_reserved(&child.context(), &changed, &[], 1, Some(&child)),
        Err(StorageError::IdempotencyConflict)
    ));
}

#[test]
fn pending_child_policy_is_rechecked_and_final_error_has_separate_durable_receipt() {
    let f = Fixture::new();
    let (ctx, plans) = request(&f, "policy", vec![item()]);
    let mut engine = f.storage.write().unwrap();
    let p = engine
        .meta_store()
        .reserve_batch(&ctx, BatchKind::Insert, &plans)
        .unwrap();
    let child = p.child(0).unwrap();
    let mut policy = crate::storage::catalog::NamespacePolicy::from(
        &engine.get_namespace(NamespaceId::new(1)).unwrap().unwrap(),
    );
    policy.initial_stability = 42.;
    engine
        .meta_store()
        .update_namespace_policy_with_receipt(
            &MutationContext::unkeyed(NamespaceId::new(1)),
            &policy,
        )
        .unwrap();
    let BatchItemPlan::Insert { input, .. } = &plans[0] else {
        panic!()
    };
    assert!(
        engine
            .insert_memory_reserved(&child.context(), input, &[], 1, Some(&child))
            .is_err()
    );
    assert!(engine.scan_all().unwrap().is_empty());
    let final_error = BatchError::new("POLICY_CHANGED");
    let complete = engine
        .meta_store()
        .finalize_batch_error(&child, final_error.clone())
        .unwrap();
    assert_eq!(complete.bookkeeping_receipts.len(), 1);
    assert_ne!(
        complete.bookkeeping_receipts[0].operation_id,
        child.operation_id
    );
    assert_eq!(complete.reservation_receipt, p.reservation_receipt);
    assert!(matches!(&complete.items[0].state,BatchItemState::Error{error} if *error==final_error));
    let generation = engine.control_state().unwrap().committed_generation;
    assert_eq!(
        engine
            .meta_store()
            .finalize_batch_error(&child, final_error)
            .unwrap(),
        complete
    );
    assert_eq!(
        engine.control_state().unwrap().committed_generation,
        generation
    );
    let tx = engine.meta_store().db.begin_read().unwrap();
    assert_eq!(
        validate_existing(&tx, Some(&engine.control_state().unwrap())).unwrap(),
        vec![complete]
    );
}

#[test]
fn corrupt_batch_manifest_is_rejected_before_writable_startup_without_source_changes() {
    for corruption in 0..5 {
        let f = Fixture::new();
        let (ctx, plans) = request(&f, "corrupt", vec![item()]);
        {
            let engine = f.storage.read().unwrap();
            let store = engine.meta_store();
            let mut p = store
                .reserve_batch(&ctx, BatchKind::Insert, &plans)
                .unwrap();
            let tx = store.db.begin_write().unwrap();
            match corruption {
                0 => {
                    tx.delete_table(SCHEMA).unwrap();
                }
                1 => {
                    tx.open_table(SCHEMA).unwrap().insert("version", 2).unwrap();
                }
                2 => {
                    p.items[0].operation_id = p.reservation_receipt.operation_id;
                    save(&tx, &p).unwrap();
                }
                3 => {
                    p.items[0].state = BatchItemState::Error {
                        error: BatchError::new("INVALID_INPUT"),
                    };
                    save(&tx, &p).unwrap();
                }
                _ => {
                    let mut row = serde_json::to_value(&p).unwrap();
                    row["items"][0]["state"]["futureStep"] = serde_json::json!(true);
                    tx.open_table(BATCHES)
                        .unwrap()
                        .insert(
                            p.reservation_receipt.operation_id.as_bytes().as_slice(),
                            serde_json::to_vec(&row).unwrap().as_slice(),
                        )
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let dir = f.close();
        let path = dir.path().join("meta.db");
        let before = std::fs::read(&path).unwrap();
        assert!(crate::storage::RedbStorageEngine::open(dir.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

#[test]
fn pending_batch_scope_never_rebinds_a_recreated_namespace_name() {
    let f = Fixture::new();
    let original = f.create_namespace("incarnation");
    let (ctx, plans) = request(
        &f,
        "scope",
        vec![
            serde_json::json!({"summary":"s","namespace":"incarnation","embedding":[1.,0.,0.,0.]}),
        ],
    );
    let p = f
        .storage
        .read()
        .unwrap()
        .meta_store()
        .reserve_batch(&ctx, BatchKind::Insert, &plans)
        .unwrap();
    f.storage
        .write()
        .unwrap()
        .purge_namespace_checked(original, true)
        .unwrap();
    let replacement = f.create_namespace("incarnation");
    assert_ne!(original, replacement);
    let mut engine = f.storage.write().unwrap();
    let replay = engine
        .meta_store()
        .reserve_batch(&ctx, BatchKind::Insert, &plans)
        .unwrap();
    assert_eq!(replay.items, p.items);
    let child = replay.child(0).unwrap();
    assert_eq!(child.namespace(), original);
    let BatchItemPlan::Insert { input, .. } = &plans[0] else {
        panic!()
    };
    let before = engine.control_state().unwrap();
    assert!(matches!(
        engine.insert_memory_reserved(&child.context(), input, &[], 123, Some(&child)),
        Err(StorageError::NamespaceNotFound(_))
    ));
    assert_eq!(engine.control_state().unwrap(), before);
    assert!(engine.scan_all().unwrap().is_empty());
}
