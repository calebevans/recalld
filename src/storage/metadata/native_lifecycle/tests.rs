use super::*;
use crate::model::schema::LifecycleVersion;
use crate::storage::control::{Generation, GenerationRef};
use crate::storage::lifecycle_migration::{LifecycleMigrationOptions, migrate_lifecycle};
use crate::storage::offline_snapshot::OfflineSnapshot;
use crate::storage::schema_migration::{MigrationOptions, inspect_schema, migrate_schema};
use crate::storage::{RedbStorageEngine, StorageEngine};
use crate::test_support::{Fixture, MemorySpec};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

const DAY: i64 = 86_400_000;
struct TestClock {
    now: AtomicI64,
    calls: AtomicUsize,
}
impl TestClock {
    fn new(now: i64) -> Self {
        Self {
            now: AtomicI64::new(now),
            calls: AtomicUsize::new(0),
        }
    }
    fn set(&self, now: i64) {
        self.now.store(now, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_millis(&self) -> i64 {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.now.load(Ordering::SeqCst)
    }
}
struct Data {
    path: PathBuf,
    legacy: tempfile::TempDir,
    ns: NamespaceId,
    a: MemoryId,
    b: MemoryId,
    summary: MemoryId,
    ghost: MemoryId,
    terminal: MemoryId,
    absent: MemoryId,
}
async fn fixture(
    root: &Path,
    version: LifecycleVersion,
    perma: f32,
    multiplier: Option<f32>,
) -> Data {
    let f = Fixture::new();
    let mut ns = NamespaceConfig::default_namespace(0);
    ns.name = "native".into();
    ns.embedding_dim = 4;
    ns.permastore_threshold = perma;
    ns.decay_rate_multiplier = multiplier;
    let ns = f.storage.write().unwrap().create_namespace(&ns).unwrap();
    let a = f.insert_memory_with(MemorySpec::new(ns)).await;
    let b = f.insert_memory_with(MemorySpec::new(ns)).await;
    let summary = f
        .insert_memory_with(MemorySpec {
            phase: DecayPhase::Summary,
            ..MemorySpec::new(ns)
        })
        .await;
    let ghost = f
        .insert_memory_with(MemorySpec {
            phase: DecayPhase::Ghost,
            summary: "",
            ..MemorySpec::new(ns)
        })
        .await;
    let terminal = f.insert_memory_with(MemorySpec::new(ns)).await;
    f.tombstone(terminal);
    let absent = f.insert_memory_with(MemorySpec::new(ns)).await;
    f.storage.write().unwrap().delete_memory(absent).unwrap();
    f.storage.write().unwrap().sync().unwrap();
    let legacy = f.close();
    let canonical = root.join("schema");
    migrate_schema(
        legacy.path(),
        &canonical,
        &MigrationOptions {
            migration_now: 10,
            expected_source: inspect_schema(legacy.path())
                .unwrap()
                .control
                .generation_ref(),
            attestations: vec![],
        },
    )
    .unwrap();
    let path = if version == LifecycleVersion::ExplicitFeedbackV2 {
        let dest = root.join("v2");
        migrate_lifecycle(
            &canonical,
            &dest,
            &LifecycleMigrationOptions {
                expected_source: inspect_schema(&canonical).unwrap().control.generation_ref(),
                namespace_id: ns,
                expected_policy_revision: Generation::default(),
                target: version,
                idempotency_key: "select-v2".into(),
            },
        )
        .unwrap();
        dest
    } else {
        canonical
    };
    Data {
        path,
        legacy,
        ns,
        a,
        b,
        summary,
        ghost,
        terminal,
        absent,
    }
}
fn scope(d: &Data) -> LifecycleScope {
    LifecycleScope {
        namespace_id: d.ns,
        namespace_name: "native".into(),
        source: None,
    }
}
fn ctx(d: &Data, g: GenerationRef, key: &str) -> MutationContext {
    MutationContext {
        store_id: Some(g.store_id),
        scope: MutationScope::Namespace(d.ns),
        idempotency_key: Some(key.into()),
        expected_generation: None,
        contract_version: 2,
    }
}
fn record(session: &NativeLifecycleSession, id: MemoryId) -> DiskRecord {
    session.metadata.get(id).unwrap().unwrap()
}
fn open(d: &Data, clock: Arc<TestClock>) -> NativeLifecycleSession {
    NativeLifecycleSession::open(&d.path, NativePolicyInputs::default(), clock).unwrap()
}
fn outcome(receipt: &WriteReceipt) -> &NativeLifecycleWrite {
    match &receipt.outcome {
        MutationOutcome::LifecycleExposureRecorded { transition }
        | MutationOutcome::LifecycleFeedbackApplied { transition } => transition,
        _ => panic!("wrong outcome"),
    }
}
fn logical(session: &NativeLifecycleSession) -> Vec<Vec<u8>> {
    let read = session.metadata.db.begin_read().unwrap();
    let mut rows = Vec::new();
    for row in read.open_table(META_TABLE).unwrap().iter().unwrap() {
        rows.push(row.unwrap().1.value().to_vec());
    }
    rows.push(control::encode(&control::validate_existing(&read).unwrap().unwrap()).unwrap());
    for row in read.open_table(control::RECEIPTS).unwrap().iter().unwrap() {
        rows.push(row.unwrap().1.value().to_vec());
    }
    rows
}

#[tokio::test]
async fn real_native_exposure_and_feedback_obey_both_policies_and_one_clock() {
    for version in [
        LifecycleVersion::LegacyAccessV1,
        LifecycleVersion::ExplicitFeedbackV2,
    ] {
        let root = tempfile::tempdir().unwrap();
        let d = fixture(root.path(), version, 1500., Some(1.)).await;
        let clock = Arc::new(TestClock::new(7 * DAY));
        let session = open(&d, clock.clone());
        let before = record(&session, d.a);
        let state = logical(&session);
        let intent = session
            .capture_exposure(
                scope(&d),
                &[
                    (d.a, AccessKind::AssociativeRetrieval),
                    (d.a, AccessKind::DirectRetrieval),
                ],
            )
            .unwrap();
        assert_eq!(
            logical(&session),
            state,
            "capture cannot mutate canonical state"
        );
        assert_eq!(clock.calls.load(Ordering::SeqCst), 0);
        let key = ctx(&d, intent.capture_generation(), "expose");
        let receipt = session.record_exposure(&key, &intent).unwrap().unwrap();
        assert_eq!(
            outcome(&receipt).capture_generation,
            intent.capture_generation()
        );
        assert_eq!(outcome(&receipt).records.len(), 1);
        assert_eq!(
            outcome(&receipt).records[0].kind,
            AccessKind::DirectRetrieval
        );
        assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
        let after = record(&session, d.a);
        assert_eq!(after.stability, before.stability);
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.summary, before.summary);
        assert_eq!(after.schema.as_ref().unwrap().lifecycle.reinforced_at, None);
        assert_eq!(
            after.schema.as_ref().unwrap().lifecycle.decay_anchor_at,
            if version == LifecycleVersion::LegacyAccessV1 {
                7 * DAY
            } else {
                0
            }
        );
        assert_eq!(
            after.schema.as_ref().unwrap().lifecycle.retrieved_at,
            Some(7 * DAY)
        );
        assert_eq!(record(&session, d.b).access_history.len(), 0);
        clock.set(8 * DAY);
        let replay = session.record_exposure(&key, &intent).unwrap().unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
        let fresh = session
            .capture_exposure(scope(&d), &[(d.a, AccessKind::DirectRetrieval)])
            .unwrap();
        assert!(
            session
                .record_exposure(&key, &fresh)
                .unwrap()
                .unwrap()
                .replayed,
            "generated tokens/read generation do not change stable retry identity"
        );
        let a = session.capture_feedback(d.a, None, 3).unwrap();
        let b = session.capture_feedback(d.b, Some(d.ns), 3).unwrap();
        let ra = session
            .apply_feedback(&ctx(&d, a.capture_generation(), "feedback-a"), &a)
            .unwrap();
        let rb = session
            .apply_feedback(&ctx(&d, b.capture_generation(), "feedback-b"), &b)
            .unwrap();
        assert_eq!(clock.calls.load(Ordering::SeqCst), 3);
        let sa = outcome(&ra).records[0].after.stability;
        let sb = outcome(&rb).records[0].after.stability;
        if version == LifecycleVersion::ExplicitFeedbackV2 {
            assert_eq!(sa, sb, "exposure cannot change v2 feedback elapsed time");
        } else {
            assert_ne!(sa, sb);
        }
        let observed = record(&session, d.a);
        let life = &observed.schema.as_ref().unwrap().lifecycle;
        assert_eq!(life.observed_at, 0);
        assert_eq!(life.decay_anchor_at, 8 * DAY);
        assert_eq!(life.retrieved_at, Some(7 * DAY));
        assert_eq!(life.reinforced_at, Some(8 * DAY));
        drop(session);
        assert!(inspect_schema(&d.path).is_ok());
        assert!(RedbStorageEngine::open(&d.path).is_err());
        assert!(MetadataStore::open(&d.path.join("meta.db")).is_err());
    }
}

#[tokio::test]
async fn replay_precedes_stale_tokens_policy_and_clock_but_not_lineage_or_payload() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        None,
    )
    .await;
    let clock = Arc::new(TestClock::new(8 * DAY));
    let session = open(&d, clock.clone());
    let intent = session.capture_feedback(d.a, None, 3).unwrap();
    let mut key = ctx(&d, intent.capture_generation(), "feedback");
    key.expected_generation = Some(intent.capture_generation().generation);
    let first = session.apply_feedback(&key, &intent).unwrap();
    let committed = logical(&session);
    let wrong_policy = NativePolicyInputs {
        multiplier: 2.,
        ..Default::default()
    };
    clock.set(i64::MAX);
    let replay = session
        .metadata
        .apply_native_feedback(&key, &intent, &wrong_policy, clock.as_ref())
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.operation_id, first.operation_id);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(logical(&session), committed);
    let mut newkey = key.clone();
    newkey.idempotency_key = Some("different".into());
    newkey.expected_generation = None;
    assert!(matches!(
        session
            .metadata
            .apply_native_feedback(&newkey, &intent, &wrong_policy, clock.as_ref()),
        Err(StorageError::LifecycleConflict(_))
    ));
    assert!(matches!(
        session.apply_feedback(&newkey, &intent),
        Err(StorageError::LifecycleConflict(_))
    ));
    let changed = session.capture_feedback(d.a, None, 4).unwrap();
    assert!(matches!(
        session.apply_feedback(&key, &changed),
        Err(StorageError::IdempotencyConflict)
    ));
    let mut foreign = key;
    foreign.store_id = Some(uuid::Uuid::new_v4());
    assert!(matches!(
        session.apply_feedback(&foreign, &intent),
        Err(StorageError::StoreLineageMismatch)
    ));
    assert_eq!(logical(&session), committed);
}

#[tokio::test]
async fn abandoned_real_transaction_and_invalid_inputs_leave_records_receipts_and_history_unchanged()
 {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(8 * DAY));
    let session = open(&d, clock.clone());
    let before = logical(&session);
    let intent = session.capture_feedback(d.a, None, 3).unwrap();
    let key = ctx(&d, intent.capture_generation(), "abandoned");
    {
        let guard = session.metadata.native_guard().unwrap();
        let (tx, receipt) = session
            .metadata
            .prepare_native(
                &guard,
                &key,
                &intent.scope,
                &intent.capture,
                &[(d.a, AccessKind::ManualReinforcement)],
                Some(3),
                &NativePolicyInputs::default(),
                clock.as_ref(),
            )
            .unwrap();
        assert!(receipt.is_some());
        drop(tx);
    }
    assert_eq!(logical(&session), before);
    for quality in [0, 5, 255] {
        assert!(session.capture_feedback(d.a, None, quality).is_err());
    }
    for id in [d.ghost, d.terminal] {
        assert!(matches!(
            session.capture_feedback(id, None, 3),
            Err(StorageError::LifecycleContentUnavailable)
        ));
        assert!(
            session
                .capture_exposure(scope(&d), &[(id, AccessKind::DirectRetrieval)])
                .is_err()
        );
    }
    assert!(matches!(
        session.capture_feedback(d.absent, None, 3),
        Err(StorageError::NotFound(_))
    ));
    let mut wrong = scope(&d);
    wrong.namespace_name = "different".into();
    assert!(matches!(
        session.capture_exposure(wrong, &[(d.a, AccessKind::DirectRetrieval)]),
        Err(StorageError::MutationNamespaceMismatch)
    ));
    assert!(
        session
            .capture_feedback(d.a, Some(NamespaceId::new(1)), 3)
            .is_err()
    );
    assert!(
        session
            .capture_exposure(scope(&d), &[(d.a, AccessKind::ManualReinforcement)])
            .is_err()
    );
    let empty = session.capture_exposure(scope(&d), &[]).unwrap();
    assert!(
        session
            .record_exposure(&ctx(&d, empty.capture_generation(), "empty"), &empty)
            .unwrap()
            .is_none()
    );
    assert_eq!(logical(&session), before);
    let receipt = session.apply_feedback(&key, &intent).unwrap();
    drop(session);
    let session = open(&d, clock);
    assert!(session.apply_feedback(&key, &intent).unwrap().replayed);
    assert_eq!(outcome(&receipt).records.len(), 1);
}

#[tokio::test]
async fn native_entry_excludes_other_owners_legacy_stores_and_unparticipating_writers() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(DAY));
    let session = open(&d, clock.clone());
    assert!(
        NativeLifecycleSession::open(&d.path, NativePolicyInputs::default(), clock.clone())
            .is_err()
    );
    assert!(OfflineSnapshot::open(&d.path).is_err());
    let before = logical(&session);
    let r = record(&session, d.a);
    assert!(
        session
            .metadata
            .update_access(d.a, DAY, AccessKind::DirectRetrieval)
            .is_err()
    );
    assert!(session.metadata.tombstone(d.a).is_err());
    assert!(session.metadata.delete(d.a).is_err());
    assert!(session.metadata.strip_full_text(d.a).is_err());
    assert!(
        session
            .metadata
            .update_decay_state(d.a, DecayPhase::Summary, 0.1, 0.1, 1., false)
            .is_err()
    );
    assert!(session.metadata.insert(MemoryId::new(), &r).is_err());
    assert!(session.metadata.persist_phase_index().is_err());
    assert!(session.metadata.rebuild_phase_index().is_err());
    assert!(
        session
            .metadata
            .create_namespace(&NamespaceConfig::default_namespace(0))
            .is_err()
    );
    assert!(session.metadata.purge_namespace_records(d.ns, 1).is_err());
    assert_eq!(logical(&session), before);
    drop(session);
    assert!(
        NativeLifecycleSession::open(d.legacy.path(), NativePolicyInputs::default(), clock)
            .is_err()
    );
}

#[tokio::test]
async fn summary_feedback_stays_stripped_and_permastore_sticky_under_clock_regression() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        0.01,
        Some(2.),
    )
    .await;
    let clock = Arc::new(TestClock::new(8 * DAY));
    let session = open(&d, clock.clone());
    let before = record(&session, d.summary);
    let intent = session.capture_feedback(d.summary, None, 4).unwrap();
    session
        .apply_feedback(&ctx(&d, intent.capture_generation(), "promote"), &intent)
        .unwrap();
    clock.set(DAY);
    let lapse = session.capture_feedback(d.summary, None, 1).unwrap();
    session
        .apply_feedback(&ctx(&d, lapse.capture_generation(), "lapse"), &lapse)
        .unwrap();
    let after = record(&session, d.summary);
    assert_eq!(after.phase, DecayPhase::Summary);
    assert_eq!(after.text_length, 0);
    assert_eq!(after.summary, before.summary);
    assert_eq!(after.is_permastore, 1);
    assert_eq!(
        after.schema.as_ref().unwrap().lifecycle.reinforced_at,
        Some(8 * DAY)
    );
    assert_eq!(after.schema.as_ref().unwrap().lifecycle.observed_at, 0);
    assert_eq!(
        after.schema.as_ref().unwrap().lifecycle.phase_changed_at,
        before.schema.as_ref().unwrap().lifecycle.phase_changed_at
    );
    assert_eq!(clock.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_identical_key_commits_once_and_other_key_gets_record_conflict() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(8 * DAY));
    let session = Arc::new(open(&d, clock.clone()));
    let intent = session.capture_feedback(d.a, None, 3).unwrap();
    let key = ctx(&d, intent.capture_generation(), "same");
    let results = std::thread::scope(|s| {
        let a = s.spawn(|| session.apply_feedback(&key, &intent).unwrap());
        let b = s.spawn(|| session.apply_feedback(&key, &intent).unwrap());
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(results[0].operation_id, results[1].operation_id);
    assert_ne!(results[0].replayed, results[1].replayed);
    assert_eq!(record(&session, d.a).access_history.len(), 1);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        session.apply_feedback(&ctx(&d, intent.capture_generation(), "other"), &intent),
        Err(StorageError::LifecycleConflict(_))
    ));
}

#[tokio::test]
async fn mixed_format_rejected_before_either_writable_entry() {
    // Valid fixtures come only from real schema migration. Raw edits below are
    // intentional corruption, never a shortcut for constructing native state.
    for case in 0..7 {
        let root = tempfile::tempdir().unwrap();
        let d = fixture(
            root.path(),
            LifecycleVersion::LegacyAccessV1,
            1500.,
            Some(1.),
        )
        .await;
        let db = redb::Database::open(d.path.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        if case < 2 {
            let mut state = control::read_control(&tx).unwrap();
            state.control_schema_version = 1;
            tx.open_table(control::CONTROL)
                .unwrap()
                .insert("state", control::encode(&state).unwrap().as_slice())
                .unwrap();
            tx.delete_table(crate::storage::canonical_schema::SCHEMA)
                .unwrap();
        }
        if case == 1 || case == 2 || case == 4 {
            let mut table = tx.open_table(NAMESPACE_TABLE).unwrap();
            let rows = table
                .iter()
                .unwrap()
                .map(|row| {
                    let (k, v) = row.unwrap();
                    (k.value(), v.value().to_vec())
                })
                .collect::<Vec<_>>();
            for (id, bytes) in rows {
                if case != 1 && id != d.ns.get() {
                    continue;
                }
                let mut ns = NamespaceConfig::decode(&bytes).unwrap();
                if case == 4 {
                    ns.schema.as_mut().unwrap().namespace_schema_version = 3;
                } else {
                    ns.schema = None;
                }
                table
                    .insert(id, control::encode(&ns).unwrap().as_slice())
                    .unwrap();
            }
        }
        if case == 5 {
            tx.delete_table(crate::storage::canonical_schema::SCHEMA)
                .unwrap();
            tx.delete_table(control::CONTROL).unwrap();
            tx.delete_table(control::RECEIPTS).unwrap();
            tx.delete_table(control::KEYS).unwrap();
        }
        if case == 6 {
            tx.delete_table(crate::storage::metadata::text_generation::CATALOG)
                .unwrap();
        }
        if case == 3 {
            let mut table = tx.open_table(META_TABLE).unwrap();
            let bytes = table
                .get(d.a.as_bytes().as_slice())
                .unwrap()
                .unwrap()
                .value()
                .to_vec();
            let mut r = DiskRecord::from_bytes(&bytes).unwrap();
            r.schema = None;
            r.version = 1;
            table
                .insert(d.a.as_bytes().as_slice(), r.encode().unwrap().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let before = std::fs::read(d.path.join("meta.db")).unwrap();
        assert!(
            MetadataStore::open(&d.path.join("meta.db")).is_err(),
            "generic writable entry accepted mixed format case {case}"
        );
        assert!(MetadataStore::open_native_lifecycle(&d.path.join("meta.db")).is_err());
        assert_eq!(std::fs::read(d.path.join("meta.db")).unwrap(), before);
        assert!(
            NativeLifecycleSession::open(
                &d.path,
                NativePolicyInputs::default(),
                Arc::new(TestClock::new(DAY))
            )
            .is_err()
        );
        assert!(RedbStorageEngine::open(&d.path).is_err());
        assert_eq!(std::fs::read(d.path.join("meta.db")).unwrap(), before);
    }
}

#[tokio::test]
async fn mixed_native_policy_history_survives_rollback_copy_and_stale_policy_retry() {
    let root = tempfile::tempdir().unwrap();
    let mut d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let original_schema = inspect_schema(&d.path).unwrap().schema.unwrap();
    let clock = Arc::new(TestClock::new(DAY));
    let session = open(&d, clock.clone());
    let intent = session
        .capture_exposure(scope(&d), &[(d.a, AccessKind::DirectRetrieval)])
        .unwrap();
    let key = ctx(&d, intent.capture_generation(), "before-rollback");
    let receipt = session.record_exposure(&key, &intent).unwrap().unwrap();
    let before = record(&session, d.a);
    drop(session);
    let migrated = root.path().join("rollback");
    let inspection = inspect_schema(&d.path).unwrap();
    migrate_lifecycle(
        &d.path,
        &migrated,
        &LifecycleMigrationOptions {
            expected_source: inspection.control.generation_ref(),
            namespace_id: d.ns,
            expected_policy_revision: before.schema.as_ref().unwrap().lifecycle.policy_revision,
            target: LifecycleVersion::LegacyAccessV1,
            idempotency_key: "rollback".into(),
        },
    )
    .unwrap();
    d.path = migrated;
    let session = open(&d, clock.clone());
    assert_eq!(
        record(&session, d.a)
            .schema
            .as_ref()
            .unwrap()
            .lifecycle
            .decay_anchor_at,
        0
    );
    assert_eq!(
        record(&session, d.a)
            .schema
            .as_ref()
            .unwrap()
            .lifecycle
            .retrieved_at,
        Some(DAY)
    );
    assert_eq!(
        session
            .record_exposure(&key, &intent)
            .unwrap()
            .unwrap()
            .operation_id,
        receipt.operation_id
    );
    assert!(matches!(
        session.record_exposure(
            &ctx(&d, intent.capture_generation(), "stale-policy"),
            &intent
        ),
        Err(StorageError::LifecycleConflict(_))
    ));
    // Real native writes after rollback, including bounded history eviction.
    for i in 0..crate::model::constants::ACCESS_HISTORY_MAX + 1 {
        clock.set(2 * DAY + i as i64);
        let intent = session
            .capture_exposure(scope(&d), &[(d.a, AccessKind::DirectRetrieval)])
            .unwrap();
        let receipt = session
            .record_exposure(
                &ctx(&d, intent.capture_generation(), &format!("history-{i}")),
                &intent,
            )
            .unwrap()
            .unwrap();
        if i == crate::model::constants::ACCESS_HISTORY_MAX {
            assert!(outcome(&receipt).records[0].evicted_event.is_some());
        }
    }
    let feedback = session.capture_feedback(d.a, None, 3).unwrap();
    session
        .apply_feedback(
            &ctx(&d, feedback.capture_generation(), "after-rollback"),
            &feedback,
        )
        .unwrap();
    let expected = record(&session, d.a).encode().unwrap();
    drop(session);
    let source_bytes = std::fs::read(d.path.join("meta.db")).unwrap();
    let copy = root.path().join("format-copy");
    migrate_schema(
        &d.path,
        &copy,
        &MigrationOptions {
            migration_now: 123,
            expected_source: inspect_schema(&d.path).unwrap().control.generation_ref(),
            attestations: vec![],
        },
    )
    .unwrap();
    assert_eq!(std::fs::read(d.path.join("meta.db")).unwrap(), source_bytes);
    assert_eq!(
        inspect_schema(&copy).unwrap().schema.unwrap(),
        original_schema
    );
    d.path = copy;
    let session = open(&d, clock);
    assert_eq!(record(&session, d.a).encode().unwrap(), expected);
    assert_eq!(
        record(&session, d.a).access_history.len(),
        crate::model::constants::ACCESS_HISTORY_MAX
    );
}

#[tokio::test]
async fn native_backup_fork_preserves_receipts_applies_later_deletion_and_blocks_historical_writes()
{
    use crate::backup::{RestoreOptions, restore_from_backup, run_backup};
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(DAY));
    let schema = inspect_schema(&d.path).unwrap().schema.unwrap();
    let session = open(&d, clock.clone());
    let intent = session.capture_feedback(d.a, None, 3).unwrap();
    let key = ctx(&d, intent.capture_generation(), "old-feedback");
    let receipt = session.apply_feedback(&key, &intent).unwrap();
    drop(session);
    let backup = run_backup(
        &crate::config::RecalldConfig::default(),
        &root.path().join("native.zip"),
        Some(&d.path),
        false,
    )
    .await
    .unwrap();
    // This retained source is used only to create newer removal evidence, as in
    // restore bridge tests. It is not a second production serving copy.
    let legacy = RedbStorageEngine::open(d.legacy.path()).unwrap();
    legacy.tombstone_memory(d.a).unwrap();
    drop(legacy);
    restore_from_backup(RestoreOptions {
        backup_path: backup.clone(),
        data_dir: d.legacy.path().into(),
        force: true,
        no_stop_daemon: true,
        historical: false,
    })
    .await
    .unwrap();
    let inspection = inspect_schema(d.legacy.path()).unwrap();
    assert_eq!(inspection.schema.unwrap(), schema);
    assert_ne!(inspection.control.store_id, receipt.store_id);
    let session = NativeLifecycleSession::open(
        d.legacy.path(),
        NativePolicyInputs::default(),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(record(&session, d.a).phase, DecayPhase::Tombstone);
    assert!(session.capture_feedback(d.a, None, 3).is_err());
    assert!(matches!(
        session.apply_feedback(&key, &intent),
        Err(StorageError::StoreLineageMismatch)
    ));
    let read = session.metadata.db.begin_read().unwrap();
    let receipt_key = control::scoped_key(
        receipt.store_id,
        MutationScope::Namespace(d.ns),
        receipt.operation_id.as_bytes(),
    );
    let stored = control::validate_receipt(
        read.open_table(control::RECEIPTS)
            .unwrap()
            .get(receipt_key.as_slice())
            .unwrap()
            .unwrap()
            .value(),
    )
    .unwrap();
    assert_eq!(stored.receipt, receipt);
    drop(read);
    let fresh = session.capture_feedback(d.b, None, 3).unwrap();
    let new = session
        .apply_feedback(&ctx(&d, fresh.capture_generation(), "new-feedback"), &fresh)
        .unwrap();
    assert_eq!(new.store_id, inspection.control.store_id);
    drop(session);
    inspect_schema(d.legacy.path()).unwrap();
    let historical = root.path().join("historical");
    restore_from_backup(RestoreOptions {
        backup_path: backup,
        data_dir: historical.clone(),
        force: true,
        no_stop_daemon: true,
        historical: true,
    })
    .await
    .unwrap();
    inspect_schema(&historical).unwrap();
    let before = std::fs::read(historical.join("meta.db")).unwrap();
    assert!(
        NativeLifecycleSession::open(&historical, NativePolicyInputs::default(), clock).is_err()
    );
    assert_eq!(std::fs::read(historical.join("meta.db")).unwrap(), before);
}

#[tokio::test]
async fn malformed_native_proof_and_missing_suffix_fail_read_only_inspection() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let session = open(&d, Arc::new(TestClock::new(DAY)));
    let intent = session.capture_feedback(d.a, None, 3).unwrap();
    let mut context = ctx(&d, intent.capture_generation(), "unused");
    context.idempotency_key = None;
    let receipt = session.apply_feedback(&context, &intent).unwrap();
    let key = control::scoped_key(
        receipt.store_id,
        MutationScope::Namespace(d.ns),
        receipt.operation_id.as_bytes(),
    );
    let original = {
        let read = session.metadata.db.begin_read().unwrap();
        let table = read.open_table(control::RECEIPTS).unwrap();
        table.get(key.as_slice()).unwrap().unwrap().value().to_vec()
    };
    drop(session);
    for case in 0..6 {
        let db = redb::Database::open(d.path.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        let mut stored: control::StoredReceipt = control::decode(&original).unwrap();
        let MutationOutcome::LifecycleFeedbackApplied { transition } = &mut stored.receipt.outcome
        else {
            unreachable!()
        };
        match case {
            0 => transition.records[0].after.stability = 0.5_f32.try_into().unwrap(),
            1 => transition.records[0].history_before[0] ^= 1,
            2 => transition.capture_generation.store_id = uuid::Uuid::new_v4(),
            3 => transition.records[0].after.lifecycle.observed_at += 1,
            4 => transition.records[0].before.phase = DecayPhase::Ghost,
            5 => {}
            _ => unreachable!(),
        }
        {
            let mut table = tx.open_table(control::RECEIPTS).unwrap();
            if case == 5 {
                table.remove(key.as_slice()).unwrap();
            } else {
                table
                    .insert(key.as_slice(), control::encode(&stored).unwrap().as_slice())
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        drop(db);
        let before = std::fs::read(d.path.join("meta.db")).unwrap();
        assert!(
            inspect_schema(&d.path).is_err(),
            "accepted corrupted native proof case {case}"
        );
        assert!(
            NativeLifecycleSession::open(
                &d.path,
                NativePolicyInputs::default(),
                Arc::new(TestClock::new(DAY))
            )
            .is_err()
        );
        assert_eq!(std::fs::read(d.path.join("meta.db")).unwrap(), before);
        let db = redb::Database::open(d.path.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        tx.open_table(control::RECEIPTS)
            .unwrap()
            .insert(key.as_slice(), original.as_slice())
            .unwrap();
        tx.commit().unwrap();
        drop(db);
        inspect_schema(&d.path).unwrap();
    }
}

#[tokio::test]
async fn source_tokens_and_bad_clock_reject_before_batch_mutation() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(DAY));
    let session = open(&d, clock.clone());
    let mut scoped = scope(&d);
    scoped.source = Some(d.a);
    let original = [
        (d.b, AccessKind::AssociativeRetrieval),
        (d.summary, AccessKind::DirectRetrieval),
    ];
    let intent = session.capture_exposure(scoped.clone(), &original).unwrap();
    let source_feedback = session.capture_feedback(d.a, None, 3).unwrap();
    session
        .apply_feedback(
            &ctx(&d, source_feedback.capture_generation(), "source-change"),
            &source_feedback,
        )
        .unwrap();
    let before = logical(&session);
    assert!(matches!(
        session.record_exposure(
            &ctx(&d, intent.capture_generation(), "stale-source"),
            &intent
        ),
        Err(StorageError::LifecycleConflict(_))
    ));
    assert_eq!(logical(&session), before);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
    let fresh = session.capture_exposure(scoped, &original).unwrap();
    let key = ctx(&d, fresh.capture_generation(), "valid-batch");
    clock.set(i64::MAX);
    assert!(session.record_exposure(&key, &fresh).is_err());
    assert_eq!(logical(&session), before);
    clock.set(2 * DAY);
    let receipt = session.record_exposure(&key, &fresh).unwrap().unwrap();
    assert_eq!(outcome(&receipt).records.len(), 2);
    assert_eq!(
        record(&session, d.b)
            .schema
            .as_ref()
            .unwrap()
            .lifecycle
            .retrieved_at,
        Some(2 * DAY)
    );
    assert_eq!(
        record(&session, d.summary)
            .schema
            .as_ref()
            .unwrap()
            .lifecycle
            .retrieved_at,
        Some(2 * DAY)
    );
    assert_eq!(
        record(&session, d.a)
            .schema
            .as_ref()
            .unwrap()
            .lifecycle
            .retrieved_at,
        None
    );
    assert_eq!(clock.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn uncertain_real_commit_poisoning_requires_reopen_then_reconciles_without_reapplying() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(DAY));
    let session = open(&d, clock.clone());
    let intent = session.capture_feedback(d.a, None, 3).unwrap();
    let key = ctx(&d, intent.capture_generation(), "uncertain");
    FAIL_AFTER_NATIVE_COMMIT.with(|flag| flag.set(true));
    let error = session.apply_feedback(&key, &intent).unwrap_err();
    let StorageError::CommitOutcomeUnknown { operation_id, .. } = error else {
        panic!("wrong error: {error}")
    };
    assert!(matches!(
        session.capture_feedback(d.b, None, 3),
        Err(StorageError::RecoveryRequired(_))
    ));
    assert!(matches!(
        session.apply_feedback(&key, &intent),
        Err(StorageError::RecoveryRequired(_))
    ));
    drop(session);
    let session = open(&d, clock.clone());
    let reconciled = session
        .lookup_operation(intent.capture_generation().store_id, d.ns, operation_id)
        .unwrap()
        .unwrap();
    let committed = logical(&session);
    let retry = session.apply_feedback(&key, &intent).unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.operation_id, reconciled.operation_id);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(record(&session, d.a).access_history.len(), 1);
    assert_eq!(logical(&session), committed);
}

#[tokio::test]
async fn native_contract_one_rejects_feedback_and_exposure_without_bytes_or_time_change() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(DAY));
    let session = open(&d, clock.clone());
    let feedback = session.capture_feedback(d.a, None, 3).unwrap();
    let exposure = session
        .capture_exposure(scope(&d), &[(d.a, AccessKind::DirectRetrieval)])
        .unwrap();
    let mut context = ctx(&d, feedback.capture_generation(), "wrong-contract");
    context.contract_version = 1;
    let before = std::fs::read(d.path.join("meta.db")).unwrap();
    assert!(matches!(
        session.apply_feedback(&context, &feedback),
        Err(StorageError::InvalidMutationContext(_))
    ));
    assert!(matches!(
        session.record_exposure(&context, &exposure),
        Err(StorageError::InvalidMutationContext(_))
    ));
    assert_eq!(std::fs::read(d.path.join("meta.db")).unwrap(), before);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn capture_rechecks_poison_after_waiting_for_publication() {
    let root = tempfile::tempdir().unwrap();
    let d = fixture(
        root.path(),
        LifecycleVersion::ExplicitFeedbackV2,
        1500.,
        Some(1.),
    )
    .await;
    let clock = Arc::new(TestClock::new(DAY));
    for feedback in [false, true] {
        let session = open(&d, clock.clone());
        let before = logical(&session);
        let guard = session.metadata.publication.write().unwrap();
        std::thread::scope(|threads| {
            let (sender, checked) = std::sync::mpsc::channel();
            let pending = threads.spawn(|| {
                CAPTURE_CHECKED.with(|slot| slot.set(Some(sender)));
                if feedback {
                    session.capture_feedback(d.a, None, 3).map(|_| ())
                } else {
                    session
                        .capture_exposure(scope(&d), &[(d.a, AccessKind::DirectRetrieval)])
                        .map(|_| ())
                }
            });
            checked.recv().unwrap(); // Capture passed its initial admission check.
            session.metadata.mark_unavailable(); // Publication fails while it waits.
            drop(guard);
            assert!(matches!(
                pending.join().unwrap(),
                Err(StorageError::RecoveryRequired(_))
            ));
        });
        assert_eq!(logical(&session), before);
        assert_eq!(clock.calls.load(Ordering::SeqCst), 0);
    }
}
