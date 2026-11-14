use super::*;
use crate::storage::control::Generation;

fn fixture() -> (tempfile::TempDir, MetadataStore, NamespaceId, [MemoryId; 2]) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
    let ns = store
        .create_namespace(&NamespaceConfig::default_namespace(0))
        .unwrap();
    let ids = [MemoryId::new(), MemoryId::new()];
    for (slot, id) in ids.iter().enumerate() {
        store
            .insert(
                *id,
                &DiskRecord {
                    version: DiskRecord::CURRENT_VERSION,
                    id: *id.as_bytes(),
                    namespace_id: ns.get(),
                    created_at: 10,
                    last_accessed_at: 20,
                    phase: DecayPhase::Full,
                    strength: 0.5,
                    decay_strength: 0.6,
                    stability: 1.0,
                    difficulty: 5.0,
                    is_permastore: 0,
                    vector_slot: slot as u32,
                    edge_count: 7,
                    summary: "secret content excluded from receipts".into(),
                    tags: vec![crate::model::Tag::new("secret/tag").unwrap()],
                    access_history: Vec::new(),
                    text_offset: 0,
                    text_length: 0,
                },
            )
            .unwrap();
    }
    (dir, store, ns, ids)
}

fn context(store: &MetadataStore, ns: NamespaceId, key: &str) -> MutationContext {
    let state = store.control_state().unwrap();
    MutationContext {
        store_id: Some(state.store_id),
        scope: MutationScope::Namespace(ns),
        idempotency_key: Some(key.into()),
        expected_generation: Some(state.committed_generation),
        contract_version: 1,
    }
}

fn counts(store: &MetadataStore) -> (u64, u64) {
    let tx = store.db.begin_read().unwrap();
    (
        tx.open_table(control::RECEIPTS).unwrap().len().unwrap(),
        tx.open_table(control::KEYS).unwrap().len().unwrap(),
    )
}

fn snapshot(
    store: &MetadataStore,
    ids: &[MemoryId],
) -> (ControlState, Vec<Option<Vec<u8>>>, (u64, u64)) {
    (
        store.control_state().unwrap(),
        ids.iter()
            .map(|id| store.get(*id).unwrap().map(|r| r.to_bytes()))
            .collect(),
        counts(store),
    )
}

#[test]
fn exposure_batch_is_distinct_ordered_monotonic_and_one_generation() {
    let (_dir, store, ns, [a, b]) = fixture();
    let ctx = context(&store, ns, "batch");
    let before = snapshot(&store, &[a, b]);
    assert!(
        store
            .record_exposure_batch(&ctx, &[], 100)
            .unwrap()
            .is_none()
    );
    assert_eq!(snapshot(&store, &[a, b]), before);
    let receipt = store
        .record_exposure_batch(
            &ctx,
            &[
                (a, AccessKind::DirectRetrieval),
                (b, AccessKind::AssociativeRetrieval),
                (a, AccessKind::DirectRetrieval),
            ],
            5,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.committed_generation.get(),
        before.0.committed_generation.get() + 1
    );
    let MutationOutcome::ExposuresRecorded { accesses } = &receipt.outcome else {
        panic!()
    };
    assert_eq!(
        accesses.iter().map(|a| a.memory_id).collect::<Vec<_>>(),
        vec![a, b]
    );
    for id in [a, b] {
        let r = store.get(id).unwrap().unwrap();
        assert_eq!(r.last_accessed_at, 20);
        assert_eq!(r.access_history.len(), 1);
        assert_eq!(r.stability, 1.0);
        assert_eq!(r.phase, DecayPhase::Full);
        assert_eq!(r.edge_count, 7);
    }
    let bytes = serde_json::to_string(&receipt).unwrap();
    assert!(!bytes.contains("secret"));
    let after = snapshot(&store, &[a, b]);
    // Duplicate normalization preserves identity; sampled time does not.
    let replay = store
        .record_exposure_batch(
            &ctx,
            &[
                (a, AccessKind::DirectRetrieval),
                (b, AccessKind::AssociativeRetrieval),
            ],
            999,
        )
        .unwrap()
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.operation_id, receipt.operation_id);
    assert_eq!(snapshot(&store, &[a, b]), after);
    assert!(matches!(
        store.record_exposure_batch(
            &ctx,
            &[
                (b, AccessKind::AssociativeRetrieval),
                (a, AccessKind::DirectRetrieval)
            ],
            999
        ),
        Err(StorageError::IdempotencyConflict)
    ));
}

#[test]
fn invalid_last_record_rolls_back_every_event_receipt_and_generation() {
    for mode in ["missing", "foreign", "tombstone", "key_mismatch"] {
        let (_dir, store, ns, [a, b]) = fixture();
        let bad = if mode == "missing" {
            MemoryId::new()
        } else {
            b
        };
        if mode != "missing" {
            let tx = store.db.begin_write().unwrap();
            let mut r = store.get(b).unwrap().unwrap();
            match mode {
                "foreign" => r.namespace_id += 1,
                "tombstone" => r.phase = DecayPhase::Tombstone,
                "key_mismatch" => r.id = *MemoryId::new().as_bytes(),
                _ => unreachable!(),
            }
            tx.open_table(META_TABLE)
                .unwrap()
                .insert(b.as_bytes().as_slice(), r.to_bytes().as_slice())
                .unwrap();
            tx.commit().unwrap();
        }
        let ctx = context(&store, ns, "reject");
        let before = snapshot(&store, &[a, b]);
        assert!(
            store
                .record_exposure_batch(
                    &ctx,
                    &[
                        (a, AccessKind::DirectRetrieval),
                        (bad, AccessKind::DirectRetrieval)
                    ],
                    100
                )
                .is_err(),
            "{mode}"
        );
        assert_eq!(snapshot(&store, &[a, b]), before, "{mode}");
    }
}

#[test]
fn feedback_matches_legacy_planner_preserves_other_fields_and_replays_before_cas() {
    let (_dir, store, ns, [a, b]) = fixture();
    let ctx = context(&store, ns, "feedback");
    let old = store.get(a).unwrap().unwrap();
    let policy = crate::decay::lifecycle::resolve_policy(
        &store.get_namespace(ns).unwrap().unwrap(),
        &crate::decay::DecayConfig::default(),
        1.0,
    )
    .unwrap();
    let expected = crate::decay::lifecycle::plan_feedback(&old, &policy, 86_400_000, 3).unwrap();
    let receipt = store
        .record_legacy_feedback(&ctx, a, 3, 86_400_000, 1.0)
        .unwrap();
    let current = store.get(a).unwrap().unwrap();
    assert_eq!(current.stability, expected.stability);
    assert_eq!(current.strength, expected.strength);
    assert_eq!(current.phase, old.phase);
    assert_eq!(current.tags, old.tags);
    assert_eq!(current.edge_count, old.edge_count);
    assert_eq!(
        current.access_history,
        vec![AccessEvent {
            timestamp: expected.timestamp,
            kind: AccessKind::ManualReinforcement
        }]
    );
    assert_eq!(
        receipt.committed_generation.get(),
        ctx.expected_generation.unwrap().get() + 1
    );
    let before = snapshot(&store, &[a, b]);
    let replay = store
        .record_legacy_feedback(&ctx, a, 3, 1, f64::NAN)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.outcome, receipt.outcome);
    assert_eq!(snapshot(&store, &[a, b]), before);
    assert!(matches!(
        store.record_legacy_feedback(&ctx, a, 4, 100, 1.0),
        Err(StorageError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.record_exposure_batch(&ctx, &[(a, AccessKind::DirectRetrieval)], 100),
        Err(StorageError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.update_access_with_receipt(&ctx, a, 100, AccessKind::DirectRetrieval),
        Err(StorageError::IdempotencyConflict)
    ));
    let mut new_key = ctx.clone();
    new_key.idempotency_key = Some("new".into());
    assert!(matches!(
        store.record_legacy_feedback(&new_key, a, 3, 100, 1.0),
        Err(StorageError::GenerationConflict { .. })
    ));
    let mut foreign = ctx.clone();
    foreign.store_id = Some(uuid::Uuid::new_v4());
    assert!(matches!(
        store.record_legacy_feedback(&foreign, a, 3, 100, 1.0),
        Err(StorageError::StoreLineageMismatch)
    ));
}

#[test]
fn dropped_real_transactions_and_generation_exhaustion_leave_no_partial_writes() {
    let (_dir, store, ns, [a, b]) = fixture();
    let ctx = context(&store, ns, "abort");
    let before = snapshot(&store, &[a, b]);
    {
        let guard = store.mutation_guard().unwrap();
        let (tx, _) = store
            .prepare_exposures(
                &guard,
                &ctx,
                &[
                    (a, AccessKind::DirectRetrieval),
                    (b, AccessKind::DirectRetrieval),
                ],
                100,
            )
            .unwrap();
        drop(tx);
        let (tx, _) = store
            .prepare_feedback(&guard, &ctx, a, 3, 100, 1.0)
            .unwrap();
        drop(tx);
    }
    assert_eq!(snapshot(&store, &[a, b]), before);
    let tx = store.db.begin_write().unwrap();
    let mut state = store.control_state().unwrap();
    state.committed_generation = Generation::MAX;
    tx.open_table(control::CONTROL)
        .unwrap()
        .insert("state", control::encode(&state).unwrap().as_slice())
        .unwrap();
    tx.commit().unwrap();
    let ctx = context(&store, ns, "exhausted");
    let before = snapshot(&store, &[a, b]);
    assert!(matches!(
        store.record_exposure_batch(
            &ctx,
            &[
                (a, AccessKind::DirectRetrieval),
                (b, AccessKind::DirectRetrieval)
            ],
            100
        ),
        Err(StorageError::GenerationExhausted)
    ));
    assert!(matches!(
        store.record_legacy_feedback(&ctx, a, 3, 100, 1.0),
        Err(StorageError::GenerationExhausted)
    ));
    assert_eq!(snapshot(&store, &[a, b]), before);
}

#[test]
fn namespace_keys_are_independent_and_shared_across_operation_kinds() {
    let (_dir, store, ns, [a, b]) = fixture();
    let mut config = NamespaceConfig::default_namespace(0);
    config.name = "other".into();
    let other = store.create_namespace(&config).unwrap();
    let mut r = store.get(b).unwrap().unwrap();
    let c = MemoryId::new();
    r.id = *c.as_bytes();
    r.namespace_id = other.get();
    store.insert(c, &r).unwrap();
    let first = context(&store, ns, "shared");
    store
        .update_access_with_receipt(&first, a, 100, AccessKind::DirectRetrieval)
        .unwrap();
    assert!(matches!(
        store.record_exposure_batch(&first, &[(a, AccessKind::DirectRetrieval)], 100),
        Err(StorageError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.record_legacy_feedback(&first, a, 3, 100, 1.0),
        Err(StorageError::IdempotencyConflict)
    ));
    let second = context(&store, other, "shared");
    store
        .record_legacy_feedback(&second, c, 3, 100, 1.0)
        .unwrap();
    assert_eq!(counts(&store), (2, 2));
}

#[test]
fn concurrent_identical_batches_append_once() {
    let (_dir, store, ns, [a, b]) = fixture();
    let ctx = context(&store, ns, "concurrent");
    let before = store.control_state().unwrap();
    let store = std::sync::Arc::new(store);
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                store
                    .record_exposure_batch(
                        &ctx,
                        &[
                            (a, AccessKind::DirectRetrieval),
                            (b, AccessKind::DirectRetrieval),
                        ],
                        100,
                    )
                    .unwrap()
                    .unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(receipts.iter().filter(|r| !r.replayed).count(), 1);
    assert!(
        receipts
            .iter()
            .all(|r| r.operation_id == receipts[0].operation_id)
    );
    assert_eq!(
        store.control_state().unwrap().committed_generation.get(),
        before.committed_generation.get() + 1
    );
    for id in [a, b] {
        assert_eq!(store.get(id).unwrap().unwrap().access_history.len(), 1);
    }
}

#[test]
fn invalid_feedback_kinds_and_v2_contexts_do_not_mutate() {
    let (_dir, store, ns, [a, b]) = fixture();
    let ctx = context(&store, ns, "invalid");
    let before = snapshot(&store, &[a, b]);
    for quality in [0, 5, 255] {
        assert!(
            store
                .record_legacy_feedback(&ctx, a, quality, 100, 1.0)
                .is_err()
        );
    }
    assert!(
        store
            .record_legacy_feedback(&ctx, a, 3, 100, f64::NAN)
            .is_err()
    );
    for kind in [AccessKind::DecaySweep, AccessKind::ManualReinforcement] {
        assert!(
            store
                .record_exposure_batch(&ctx, &[(a, kind)], 100)
                .is_err()
        );
    }
    assert!(
        store
            .record_exposure_batch(
                &ctx,
                &[
                    (a, AccessKind::DirectRetrieval),
                    (a, AccessKind::AssociativeRetrieval)
                ],
                100
            )
            .is_err()
    );
    let mut v2 = ctx.clone();
    v2.contract_version = 2;
    assert!(
        store
            .record_exposure_batch(&v2, &[(a, AccessKind::DirectRetrieval)], 100)
            .is_err()
    );
    assert!(store.record_legacy_feedback(&v2, a, 3, 100, 1.0).is_err());
    assert_eq!(snapshot(&store, &[a, b]), before);
}

#[test]
fn committed_observations_survive_process_exit_and_replay_after_purge() {
    const CHILD: &str = "RECALLD_OBSERVATION_CHILD";
    if let Ok(path) = std::env::var(CHILD) {
        let (ctx, ids, feedback): (MutationContextWire, [MemoryId; 2], bool) =
            serde_json::from_slice(
                &std::fs::read(std::path::Path::new(&path).join("request.json")).unwrap(),
            )
            .unwrap();
        let store = MetadataStore::open(&std::path::Path::new(&path).join("meta.db")).unwrap();
        let ctx = ctx.context();
        if feedback {
            store
                .record_legacy_feedback(&ctx, ids[0], 3, 100, 1.0)
                .unwrap();
        } else {
            store
                .record_exposure_batch(
                    &ctx,
                    &[
                        (ids[0], AccessKind::DirectRetrieval),
                        (ids[1], AccessKind::DirectRetrieval),
                    ],
                    100,
                )
                .unwrap();
        }
        std::process::exit(91);
    }
    for feedback in [false, true] {
        let (dir, store, ns, ids) = fixture();
        let ctx = context(&store, ns, "crash");
        let initial = store.control_state().unwrap();
        let wire = MutationContextWire {
            store_id: initial.store_id,
            namespace: ns,
            generation: initial.committed_generation,
        };
        std::fs::write(
            dir.path().join("request.json"),
            serde_json::to_vec(&(wire, ids, feedback)).unwrap(),
        )
        .unwrap();
        drop(store);
        let output=std::process::Command::new(std::env::current_exe().unwrap()).arg("storage::metadata::observation::tests::committed_observations_survive_process_exit_and_replay_after_purge").arg("--exact").arg("--nocapture").env(CHILD,dir.path()).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(91),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_eq!(store.get(ids[0]).unwrap().unwrap().access_history.len(), 1);
        assert_eq!(
            store.control_state().unwrap().committed_generation.get(),
            initial.committed_generation.get() + 1
        );
        for id in ids {
            store.delete(id).unwrap();
        }
        store.delete_namespace(ns).unwrap();
        let after = snapshot(&store, &ids);
        let receipt = if feedback {
            store
                .record_legacy_feedback(&ctx, ids[0], 3, 999, 1.0)
                .unwrap()
        } else {
            store
                .record_exposure_batch(
                    &ctx,
                    &[
                        (ids[0], AccessKind::DirectRetrieval),
                        (ids[1], AccessKind::DirectRetrieval),
                    ],
                    999,
                )
                .unwrap()
                .unwrap()
        };
        assert!(receipt.replayed);
        assert_eq!(snapshot(&store, &ids), after);
        assert_eq!(
            receipt.committed_generation.get(),
            initial.committed_generation.get() + 1
        );
        assert!(
            store
                .lookup_operation(initial.store_id, ctx.scope, receipt.operation_id)
                .unwrap()
                .is_some()
        );
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MutationContextWire {
    store_id: uuid::Uuid,
    namespace: NamespaceId,
    generation: Generation,
}
impl MutationContextWire {
    fn context(self) -> MutationContext {
        MutationContext {
            store_id: Some(self.store_id),
            scope: MutationScope::Namespace(self.namespace),
            idempotency_key: Some("crash".into()),
            expected_generation: Some(self.generation),
            contract_version: 1,
        }
    }
}

#[test]
fn malformed_new_receipt_outcomes_and_catalog_scope_fail_reopen() {
    for mode in [
        "empty",
        "duplicate",
        "bad_kind",
        "bad_quality",
        "catalog",
        "payload",
    ] {
        let (dir, store, ns, [a, _]) = fixture();
        let ctx = MutationContext::unkeyed(ns);
        let receipt = if mode == "bad_quality" {
            store.record_legacy_feedback(&ctx, a, 3, 100, 1.0).unwrap()
        } else {
            store
                .record_exposure_batch(&ctx, &[(a, AccessKind::DirectRetrieval)], 100)
                .unwrap()
                .unwrap()
        };
        let key = control::scoped_key(receipt.store_id, ctx.scope, receipt.operation_id.as_bytes());
        let tx = store.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(control::RECEIPTS).unwrap();
            let mut stored: control::StoredReceipt =
                control::decode(table.get(key.as_slice()).unwrap().unwrap().value()).unwrap();
            match &mut stored.receipt.outcome {
                MutationOutcome::ExposuresRecorded { accesses } => match mode {
                    "empty" => accesses.clear(),
                    "duplicate" => accesses.push(accesses[0].clone()),
                    "bad_kind" => accesses[0].kind = AccessKind::ManualReinforcement,
                    _ => {}
                },
                MutationOutcome::LegacyFeedbackRecorded { quality, .. } => *quality = 0,
                _ => unreachable!(),
            }
            let mut bytes = control::encode(&stored).unwrap();
            if mode == "payload" {
                let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                json["receipt"]["outcome"]["summary"] = serde_json::json!("unrecognized content");
                bytes = serde_json::to_vec(&json).unwrap();
            }
            if mode == "catalog" {
                table.remove(key.as_slice()).unwrap();
                let catalog = control::scoped_key(
                    receipt.store_id,
                    MutationScope::Catalog,
                    receipt.operation_id.as_bytes(),
                );
                table.insert(catalog.as_slice(), bytes.as_slice()).unwrap();
            } else {
                table.insert(key.as_slice(), bytes.as_slice()).unwrap();
            }
        }
        tx.commit().unwrap();
        assert!(
            control::validate_existing(&store.db.begin_read().unwrap()).is_err(),
            "{mode}"
        );
        drop(store);
        assert!(
            MetadataStore::open(&dir.path().join("meta.db")).is_err(),
            "{mode}"
        );
    }
}

#[test]
fn feedback_rejects_canonically_ineligible_content_without_receipt() {
    for phase in [
        DecayPhase::Ghost,
        DecayPhase::Tombstone,
        DecayPhase::Summary,
    ] {
        let (_dir, store, ns, [a, b]) = fixture();
        let mut record = store.get(a).unwrap().unwrap();
        record.phase = phase;
        record.summary.clear();
        let tx = store.db.begin_write().unwrap();
        tx.open_table(META_TABLE)
            .unwrap()
            .insert(a.as_bytes().as_slice(), record.to_bytes().as_slice())
            .unwrap();
        tx.commit().unwrap();
        let ctx = context(&store, ns, "ineligible");
        let before = snapshot(&store, &[a, b]);
        assert!(store.record_legacy_feedback(&ctx, a, 3, 100, 1.0).is_err());
        assert_eq!(snapshot(&store, &[a, b]), before);
    }
}

#[test]
fn observation_timestamps_fail_without_mutation_but_replay_ignores_current_clock() {
    let (_dir, store, ns, [a, b]) = fixture();
    for invalid in [i64::MAX, i64::MIN] {
        let ctx = context(&store, ns, "bad-clock");
        let before = snapshot(&store, &[a, b]);
        assert!(
            store
                .record_exposure_batch(&ctx, &[(a, AccessKind::DirectRetrieval)], invalid)
                .is_err()
        );
        assert_eq!(snapshot(&store, &[a, b]), before);
        assert!(
            store
                .record_legacy_feedback(&ctx, b, 3, invalid, 1.0)
                .is_err()
        );
        assert_eq!(snapshot(&store, &[a, b]), before);
    }
    let exposure_ctx = context(&store, ns, "good-exposure");
    let exposure = store
        .record_exposure_batch(&exposure_ctx, &[(a, AccessKind::DirectRetrieval)], 100)
        .unwrap()
        .unwrap();
    let feedback_ctx = context(&store, ns, "good-feedback");
    let feedback = store
        .record_legacy_feedback(&feedback_ctx, b, 3, 100, 1.0)
        .unwrap();
    let before = snapshot(&store, &[a, b]);
    let replay = store
        .record_exposure_batch(&exposure_ctx, &[(a, AccessKind::DirectRetrieval)], i64::MAX)
        .unwrap()
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.operation_id, exposure.operation_id);
    let replay = store
        .record_legacy_feedback(&feedback_ctx, b, 3, i64::MAX, f64::NAN)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.operation_id, feedback.operation_id);
    assert_eq!(snapshot(&store, &[a, b]), before);

    // A legacy record can already have an invalid anchor. Do not copy it into
    // a new observation/receipt even when the supplied clock is valid.
    let mut record = store.get(a).unwrap().unwrap();
    record.last_accessed_at = i64::MAX;
    let tx = store.db.begin_write().unwrap();
    tx.open_table(META_TABLE)
        .unwrap()
        .insert(a.as_bytes().as_slice(), record.to_bytes().as_slice())
        .unwrap();
    tx.commit().unwrap();
    let ctx = context(&store, ns, "bad-anchor");
    let before = snapshot(&store, &[a, b]);
    assert!(
        store
            .record_exposure_batch(
                &ctx,
                &[
                    (b, AccessKind::DirectRetrieval),
                    (a, AccessKind::DirectRetrieval)
                ],
                100
            )
            .is_err()
    );
    assert!(store.record_legacy_feedback(&ctx, a, 3, 100, 1.0).is_err());
    assert_eq!(snapshot(&store, &[a, b]), before);

    for receipt in [exposure, feedback] {
        let mut stored = control::StoredReceipt {
            receipt,
            request_hash_version: 1,
            request_hash: [0; 32],
        };
        match &mut stored.receipt.outcome {
            MutationOutcome::ExposuresRecorded { accesses } => accesses[0].accessed_at = i64::MAX,
            MutationOutcome::LegacyFeedbackRecorded { applied_at, .. } => *applied_at = i64::MAX,
            _ => unreachable!(),
        }
        assert!(control::validate_receipt(&control::encode(&stored).unwrap()).is_err());
    }
}
