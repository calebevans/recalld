use super::*;
use crate::storage::schema_migration::{MigrationOptions, inspect_schema, migrate_schema};
use crate::storage::{RedbStorageEngine, StorageEngine};
use crate::test_support::{Fixture, MemorySpec};
use crate::time::ManualClock;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn fixture(root: &Path) -> PathBuf {
    fixture_attested(root, false).await
}
async fn fixture_attested(root: &Path, attested: bool) -> PathBuf {
    let f = Fixture::new();
    f.insert_memory_with(MemorySpec {
        embedding: Some(&[1., 0., 0., 0.]),
        ..MemorySpec::new(NamespaceId::new(1))
    })
    .await;
    f.insert_memory_with(MemorySpec {
        phase: DecayPhase::Summary,
        embedding: Some(&[1., 0., 0., 0.]),
        ..MemorySpec::new(NamespaceId::new(1))
    })
    .await;
    f.insert_memory_with(MemorySpec {
        phase: DecayPhase::Ghost,
        embedding: Some(&[1., 0., 0., 0.]),
        summary: "",
        ..MemorySpec::new(NamespaceId::new(1))
    })
    .await;
    let terminal = f.insert_memory().await;
    f.tombstone(terminal);
    f.storage.write().unwrap().sync().unwrap();
    let source = f.close();
    let dest = root.join("canonical");
    migrate_schema(
        source.path(),
        &dest,
        &MigrationOptions {
            migration_now: 1234,
            expected_source: inspect_schema(source.path())
                .unwrap()
                .control
                .generation_ref(),
            attestations: if attested {
                use crate::model::schema::{
                    EmbeddingProfile, Normalization, PreprocessingIdentity, ProviderKind,
                };
                vec![crate::storage::schema_migration::ProfileAttestationInput {
                    namespace_id: NamespaceId::new(1),
                    namespace_name: "default".into(),
                    descriptor: EmbeddingProfile {
                        identity_version: 1,
                        provider_kind: ProviderKind::Passthrough,
                        model_id: "synthetic-unit-model".into(),
                        model_revision: Some("fixture-1".into()),
                        dimension: 4,
                        document_preprocessing: PreprocessingIdentity {
                            version: "recalld-document-v1".into(),
                            prefix: "".into(),
                        },
                        query_preprocessing: PreprocessingIdentity {
                            version: "recalld-query-v1".into(),
                            prefix: "".into(),
                        },
                        normalization: Normalization::L2UnitV1,
                    },
                }]
            } else {
                vec![]
            },
        },
    )
    .unwrap();
    dest
}
fn options(source: &Path, target: LifecycleVersion, key: &str) -> LifecycleMigrationOptions {
    let state = inspect_schema(source).unwrap();
    LifecycleMigrationOptions {
        expected_source: state.control.generation_ref(),
        namespace_id: NamespaceId::new(1),
        expected_policy_revision: state
            .namespaces
            .iter()
            .find(|ns| ns.id.get() == 1)
            .unwrap()
            .schema
            .as_ref()
            .unwrap()
            .policy_revision,
        target,
        idempotency_key: key.into(),
    }
}
fn files(source: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    super::super::validation::canonical_files(source)
        .unwrap()
        .into_iter()
        .map(|p| {
            (
                p.strip_prefix(source).unwrap().to_path_buf(),
                std::fs::read(p).unwrap(),
            )
        })
        .collect()
}
fn rows(source: &Path) -> Vec<DiskRecord> {
    let snapshot = OfflineSnapshot::open(source).unwrap();
    let tx = snapshot.metadata.begin_read().unwrap();
    tx.open_table(META_TABLE)
        .unwrap()
        .iter()
        .unwrap()
        .map(|r| DiskRecord::from_bytes(r.unwrap().1.value()).unwrap())
        .collect()
}
fn edit(source: &Path, f: impl FnOnce(&redb::WriteTransaction)) {
    let db = redb::Database::open(source.join("meta.db")).unwrap();
    let tx = db.begin_write().unwrap();
    f(&tx);
    tx.commit().unwrap();
}
fn edit_receipt(source: &Path, id: uuid::Uuid, f: impl FnOnce(&mut control::StoredReceipt)) {
    edit(source, |tx| {
        let mut table = tx.open_table(control::RECEIPTS).unwrap();
        let (key, mut value) = table
            .iter()
            .unwrap()
            .map(|r| {
                let (k, v) = r.unwrap();
                (
                    k.value().to_vec(),
                    control::validate_receipt(v.value()).unwrap(),
                )
            })
            .find(|(_, v)| v.receipt.operation_id == id)
            .unwrap();
        f(&mut value);
        table
            .insert(key.as_slice(), control::encode(&value).unwrap().as_slice())
            .unwrap();
    });
}
struct OnceClock(AtomicUsize);
impl Clock for OnceClock {
    fn now_millis(&self) -> i64 {
        assert_eq!(self.0.fetch_add(1, Ordering::SeqCst), 0);
        40_000 * 86_400_000
    }
}

#[tokio::test]
async fn preview_and_both_directions_preserve_payload_times_and_publication() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    let original = files(&source);
    let original_rows = rows(&source);
    let opts = options(&source, LifecycleVersion::ExplicitFeedbackV2, "forward");
    let clock = OnceClock(AtomicUsize::new(0));
    let preview =
        preview_lifecycle(&source, &opts, &clock, &DecayConfig::default(), 1., true).unwrap();
    assert_eq!(clock.0.load(Ordering::SeqCst), 1);
    assert_eq!(preview.record_count, 4);
    assert_eq!(preview.full_to_summary, 1);
    assert_eq!(preview.summary_to_ghost, 1);
    assert_eq!(preview.ghost_to_delete, 1);
    assert_eq!(files(&source), original);
    let v2 = root.path().join("v2");
    let receipt = migrate_lifecycle(&source, &v2, &opts).unwrap();
    assert_eq!(
        receipt.committed_generation,
        opts.expected_source.generation.next().unwrap()
    );
    let state = inspect_schema(&v2).unwrap();
    assert_eq!(state.schema, inspect_schema(&source).unwrap().schema);
    let changed = rows(&v2);
    for (old, new) in original_rows.iter().zip(&changed) {
        let mut expected = old.clone();
        let fields = &mut expected.schema.as_mut().unwrap().lifecycle;
        fields.lifecycle_version = LifecycleVersion::ExplicitFeedbackV2;
        fields.policy_revision = fields.policy_revision.next().unwrap();
        fields.record_generation = fields.record_generation.next().unwrap();
        assert_eq!(expected.encode().unwrap(), new.encode().unwrap());
    }
    for (name, bytes) in &original {
        if name != Path::new("meta.db") {
            assert_eq!(std::fs::read(v2.join(name)).unwrap(), *bytes);
        }
    }
    assert!(
        RedbStorageEngine::open(&v2).is_err(),
        "ordinary v2 serving remains closed"
    );
    let rollback = root.path().join("rollback");
    migrate_lifecycle(
        &v2,
        &rollback,
        &options(&v2, LifecycleVersion::LegacyAccessV1, "back"),
    )
    .unwrap();
    for (old, new) in original_rows.iter().zip(rows(&rollback)) {
        let mut expected = old.clone();
        let fields = &mut expected.schema.as_mut().unwrap().lifecycle;
        fields.policy_revision = Generation::default().next().unwrap().next().unwrap();
        fields.record_generation = fields.policy_revision;
        assert_eq!(expected.encode().unwrap(), new.encode().unwrap());
    }
    assert_eq!(files(&source), original);
}

#[tokio::test]
async fn rollback_preserves_distinct_native_anchor_and_feedback_uses_it() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    let v2 = root.path().join("v2");
    migrate_lifecycle(
        &source,
        &v2,
        &options(&source, LifecycleVersion::ExplicitFeedbackV2, "fwd"),
    )
    .unwrap();
    // Synthetic native timestamps, not evidence of an enabled native writer.
    edit(&v2, |tx| {
        let mut table = tx.open_table(META_TABLE).unwrap();
        let mut r = table
            .iter()
            .unwrap()
            .map(|r| DiskRecord::from_bytes(r.unwrap().1.value()).unwrap())
            .find(|r| r.phase == DecayPhase::Full)
            .unwrap();
        r.last_accessed_at = 9 * 86_400_000;
        let t = &mut r.schema.as_mut().unwrap().lifecycle;
        t.retrieved_at = Some(r.last_accessed_at);
        t.decay_anchor_at = 86_400_000;
        t.lifecycle_updated_at = r.last_accessed_at;
        table
            .insert(r.id.as_slice(), r.encode().unwrap().as_slice())
            .unwrap();
    });
    let r = rows(&v2)
        .into_iter()
        .find(|r| r.phase == DecayPhase::Full)
        .unwrap();
    let policy = resolve_policy(
        &inspect_schema(&v2).unwrap().namespaces[0],
        &DecayConfig::default(),
        1.,
    )
    .unwrap();
    let plan = crate::decay::lifecycle::plan_feedback(&r, &policy, 10 * 86_400_000, 3).unwrap();
    let mut changed = r.clone();
    changed.last_accessed_at += 100;
    changed.schema.as_mut().unwrap().lifecycle.retrieved_at = Some(changed.last_accessed_at);
    assert_eq!(
        plan,
        crate::decay::lifecycle::plan_feedback(&changed, &policy, 10 * 86_400_000, 3).unwrap()
    );
    changed.schema.as_mut().unwrap().lifecycle.decay_anchor_at = 8 * 86_400_000;
    assert_ne!(
        plan.stability,
        crate::decay::lifecycle::plan_feedback(&changed, &policy, 10 * 86_400_000, 3)
            .unwrap()
            .stability
    );
    let dest = root.path().join("back");
    migrate_lifecycle(
        &v2,
        &dest,
        &options(&v2, LifecycleVersion::LegacyAccessV1, "back"),
    )
    .unwrap();
    let after = rows(&dest).into_iter().find(|v| v.id == r.id).unwrap();
    assert_eq!(after.last_accessed_at, r.last_accessed_at);
    assert_eq!(
        after.schema.as_ref().unwrap().lifecycle.decay_anchor_at,
        86_400_000
    );
    assert!(inspect_schema(&dest).is_ok());
    edit(&dest, |tx| {
        tx.open_table(control::RECEIPTS)
            .unwrap()
            .retain(|_, bytes| {
                !matches!(
                    control::validate_receipt(bytes).unwrap().receipt.outcome,
                    MutationOutcome::LifecyclePolicyMigrated { .. }
                )
            })
            .unwrap();
    });
    assert!(
        inspect_schema(&dest).is_err(),
        "revision>0 codec alone cannot authorize divergence"
    );
}

#[tokio::test]
async fn failures_abandon_whole_transaction_and_postpublish_retry_reconciles_exact_destination() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    let before = files(&source);
    let opts = options(&source, LifecycleVersion::ExplicitFeedbackV2, "cutover");
    for failure in [
        Stage::Copied,
        Stage::BeforeCommit,
        Stage::Committed,
        Stage::Validated,
        Stage::Published,
    ] {
        let target = root.path().join(format!("{failure:?}"));
        assert!(
            migrate_at(&source, &target, &opts, |stage| if stage == failure {
                Err(StorageError::RecoveryRequired(
                    "injected interruption".into(),
                ))
            } else {
                Ok(())
            })
            .is_err()
        );
        assert_eq!(files(&source), before);
        if failure == Stage::Published {
            let first = rows(&target);
            let receipt = migrate_lifecycle(&source, &target, &opts).unwrap();
            assert!(receipt.replayed);
            let again = migrate_lifecycle(&source, &target, &opts).unwrap();
            assert_eq!(receipt, again);
            assert_eq!(
                first
                    .iter()
                    .map(|r| r.encode().unwrap())
                    .collect::<Vec<_>>(),
                rows(&target)
                    .iter()
                    .map(|r| r.encode().unwrap())
                    .collect::<Vec<_>>()
            );
            let mut wrong = opts.clone();
            wrong.target = LifecycleVersion::LegacyAccessV1;
            assert!(matches!(
                migrate_lifecycle(&source, &target, &wrong),
                Err(StorageError::IdempotencyConflict)
            ));
            assert!(migrate_lifecycle(&target, &root.path().join("copied-again"), &opts).is_err());
        } else {
            assert!(!target.exists());
        }
    }
}

#[tokio::test]
async fn expected_state_ownership_and_token_exhaustion_leave_source_untouched() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    let before = files(&source);
    let opts = options(&source, LifecycleVersion::ExplicitFeedbackV2, "cutover");
    let target = root.path().join("dest");
    {
        let _view = OfflineSnapshot::open(&source).unwrap();
        assert!(migrate_lifecycle(&source, &target, &opts).is_err());
    }
    let mut wrong = opts.clone();
    wrong.expected_policy_revision = Generation::default().next().unwrap();
    assert!(migrate_lifecycle(&source, &target, &wrong).is_err());
    wrong = opts.clone();
    wrong.expected_source.generation = wrong.expected_source.generation.next().unwrap();
    assert!(migrate_lifecycle(&source, &target, &wrong).is_err());
    wrong = opts.clone();
    wrong.expected_source.store_id = uuid::Uuid::new_v4();
    assert!(matches!(
        migrate_lifecycle(&source, &target, &wrong),
        Err(StorageError::StoreLineageMismatch)
    ));
    assert_eq!(files(&source), before);
    edit(&source, |tx| {
        let mut table = tx.open_table(META_TABLE).unwrap();
        let mut r = {
            let row = table.iter().unwrap().next().unwrap().unwrap();
            DiskRecord::from_bytes(row.1.value()).unwrap()
        };
        r.schema.as_mut().unwrap().lifecycle.record_generation = Generation::MAX;
        table
            .insert(r.id.as_slice(), r.encode().unwrap().as_slice())
            .unwrap();
    });
    let before = files(&source);
    assert!(matches!(
        migrate_lifecycle(&source, &target, &opts),
        Err(StorageError::GenerationExhausted)
    ));
    assert!(!target.exists());
    assert_eq!(files(&source), before);
}

#[tokio::test]
async fn malformed_suffix_and_namespace_proofs_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    for case in [
        "unknown",
        "gap",
        "previous",
        "revision",
        "count",
        "digest",
        "duplicate",
        "record",
        "namespace",
    ] {
        let dest = root.path().join(case);
        let receipt = migrate_lifecycle(
            &source,
            &dest,
            &options(&source, LifecycleVersion::ExplicitFeedbackV2, case),
        )
        .unwrap();
        match case {
            "unknown" => edit_receipt(&dest, receipt.operation_id, |r| {
                r.receipt.outcome = MutationOutcome::GraphChanged {
                    edges_put: 0,
                    edges_removed: 0,
                    memories_removed: 0,
                }
            }),
            "gap" => edit(&dest, |tx| {
                control::advance_generation(tx).unwrap();
            }),
            "duplicate" => edit(&dest, |tx| {
                let mut table = tx.open_table(control::RECEIPTS).unwrap();
                let key = control::scoped_key(
                    receipt.store_id,
                    MutationScope::Namespace(NamespaceId::new(1)),
                    receipt.operation_id.as_bytes(),
                );
                let mut stored =
                    control::validate_receipt(table.get(key.as_slice()).unwrap().unwrap().value())
                        .unwrap();
                stored.receipt.operation_id = uuid::Uuid::new_v4();
                let key = control::scoped_key(
                    receipt.store_id,
                    MutationScope::Namespace(NamespaceId::new(1)),
                    stored.receipt.operation_id.as_bytes(),
                );
                table
                    .insert(key.as_slice(), control::encode(&stored).unwrap().as_slice())
                    .unwrap();
            }),
            "record" => edit(&dest, |tx| {
                let mut table = tx.open_table(META_TABLE).unwrap();
                let mut r = {
                    let row = table.iter().unwrap().next().unwrap().unwrap();
                    DiskRecord::from_bytes(row.1.value()).unwrap()
                };
                r.schema.as_mut().unwrap().lifecycle.record_generation = r
                    .schema
                    .as_ref()
                    .unwrap()
                    .lifecycle
                    .record_generation
                    .next()
                    .unwrap();
                table
                    .insert(r.id.as_slice(), r.encode().unwrap().as_slice())
                    .unwrap();
            }),
            "namespace" => edit(&dest, |tx| {
                let mut table = tx.open_table(NAMESPACE_TABLE).unwrap();
                let mut ns =
                    NamespaceConfig::decode(table.get(1).unwrap().unwrap().value()).unwrap();
                ns.schema.as_mut().unwrap().lifecycle_version = LifecycleVersion::LegacyAccessV1;
                table
                    .insert(1, control::encode(&ns).unwrap().as_slice())
                    .unwrap();
            }),
            _ => edit_receipt(&dest, receipt.operation_id, |r| {
                let MutationOutcome::LifecyclePolicyMigrated { transition: t } =
                    &mut r.receipt.outcome
                else {
                    panic!()
                };
                match case {
                    "previous" => t.previous_generation = t.previous_generation.next().unwrap(),
                    "revision" => t.next_revision = t.next_revision.next().unwrap(),
                    "count" => t.record_count += 1,
                    "digest" => t.record_token_digest = "0".repeat(64),
                    _ => unreachable!(),
                }
            }),
        }
        assert!(inspect_schema(&dest).is_err(), "{case}");
    }
}

#[tokio::test]
async fn migrated_backup_forks_preserving_policy_proof_and_uses_active_baseline_one() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    let v2 = root.path().join("v2");
    let receipt = migrate_lifecycle(
        &source,
        &v2,
        &options(&source, LifecycleVersion::ExplicitFeedbackV2, "forward"),
    )
    .unwrap();
    let archive = crate::backup::run_backup(
        &crate::config::RecalldConfig::default(),
        &root.path().join("v2.zip"),
        Some(&v2),
        false,
    )
    .await
    .unwrap();
    let fork = root.path().join("fork");
    crate::backup::restore_from_backup(crate::backup::RestoreOptions {
        backup_path: archive,
        data_dir: fork.clone(),
        force: true,
        no_stop_daemon: true,
        historical: true,
    })
    .await
    .unwrap();
    let control = inspect_schema(&fork).unwrap().control;
    assert_ne!(control.store_id, receipt.store_id);
    assert_eq!(control.committed_generation.get(), 1);
    let back = root.path().join("fork-back");
    let new = migrate_lifecycle(
        &fork,
        &back,
        &options(&fork, LifecycleVersion::LegacyAccessV1, "back"),
    )
    .unwrap();
    assert_eq!(new.store_id, control.store_id);
    assert_eq!(new.committed_generation.get(), 2);
    assert!(RedbStorageEngine::open(&back).is_err());
    let _ = preview_lifecycle(
        &back,
        &options(&back, LifecycleVersion::ExplicitFeedbackV2, "again"),
        &ManualClock::new(0),
        &DecayConfig::default(),
        1.,
        true,
    )
    .unwrap();
    edit_receipt(&back, receipt.operation_id, |r| {
        let MutationOutcome::LifecyclePolicyMigrated { transition: t } = &mut r.receipt.outcome
        else {
            panic!()
        };
        t.to = LifecycleVersion::LegacyAccessV1;
    });
    assert!(
        inspect_schema(&back).is_err(),
        "historical policy evidence remains required"
    );
}

#[tokio::test]
async fn attested_policy_restore_keeps_original_publication_and_rejects_stripped_pointer() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture_attested(root.path(), true).await;
    let original = inspect_schema(&source).unwrap().schema.unwrap();
    assert!(original.receipt_operation_id.is_some());
    let v2 = root.path().join("v2");
    migrate_lifecycle(
        &source,
        &v2,
        &options(&source, LifecycleVersion::ExplicitFeedbackV2, "forward"),
    )
    .unwrap();
    assert_eq!(
        inspect_schema(&v2).unwrap().schema.as_ref(),
        Some(&original)
    );
    let archive = crate::backup::run_backup(
        &crate::config::RecalldConfig::default(),
        &root.path().join("attested.zip"),
        Some(&v2),
        false,
    )
    .await
    .unwrap();
    // Default retained-state restore into its existing same-lineage target.
    crate::backup::restore_from_backup(crate::backup::RestoreOptions {
        backup_path: archive,
        data_dir: v2.clone(),
        force: true,
        no_stop_daemon: true,
        historical: false,
    })
    .await
    .unwrap();
    let restored = inspect_schema(&v2).unwrap();
    assert_ne!(restored.control.store_id, original.source.store_id);
    assert_eq!(restored.schema.as_ref(), Some(&original));
    edit(&v2, |tx| {
        let mut table = tx
            .open_table(super::super::canonical_schema::SCHEMA)
            .unwrap();
        let mut state: CanonicalSchemaState =
            control::decode(table.get("state").unwrap().unwrap().value()).unwrap();
        state.receipt_operation_id = None;
        table
            .insert("state", control::encode(&state).unwrap().as_slice())
            .unwrap();
    });
    assert!(
        inspect_schema(&v2).is_err(),
        "later policy receipts cannot replace historical attestation proof"
    );
}

#[tokio::test]
async fn preview_projects_canonical_same_scope_live_donors_and_honors_disabled_policy() {
    for donor in ["live", "ghost", "other-namespace"] {
        let root = tempfile::tempdir().unwrap();
        let f = Fixture::new();
        let a = f.insert_memory().await;
        let ns = if donor == "other-namespace" {
            let mut ns = NamespaceConfig::default_namespace(0);
            ns.name = "other".into();
            ns.embedding_dim = 4;
            f.storage.write().unwrap().create_namespace(&ns).unwrap()
        } else {
            NamespaceId::new(1)
        };
        let phase = if donor == "ghost" {
            DecayPhase::Ghost
        } else {
            DecayPhase::Full
        };
        let b = f
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Full,
                summary: if donor == "ghost" { "" } else { "donor" },
                ..MemorySpec::new(ns)
            })
            .await;
        {
            let mut storage = f.storage.write().unwrap();
            // Canonical edge creation requires live same-namespace endpoints.
            // A later decay may leave an existing edge attached to a Ghost.
            let edge = storage.put_edges_with_effects(&[crate::storage::PersistedEdge {
                source: a,
                target: b,
                edge_type: crate::model::EdgeType::Associative,
                weight: 1.,
                auto_created: false,
                created_at: 0,
            }]);
            if donor == "other-namespace" {
                assert!(edge.is_err());
            } else {
                edge.unwrap();
            }
            for (id, phase) in [(a, DecayPhase::Full), (b, phase)] {
                storage
                    .update_decay_state(id, phase, 0., 0., 1., false)
                    .unwrap();
            }
            storage.sync().unwrap();
        }
        let source = f.close();
        let canonical = root.path().join("canonical");
        migrate_schema(
            source.path(),
            &canonical,
            &MigrationOptions {
                migration_now: 0,
                expected_source: inspect_schema(source.path())
                    .unwrap()
                    .control
                    .generation_ref(),
                attestations: vec![],
            },
        )
        .unwrap();
        let opts = options(&canonical, LifecycleVersion::ExplicitFeedbackV2, "preview");
        let before = files(&canonical);
        let clock = ManualClock::new(9 * 86_400_000 / 2);
        let p = preview_lifecycle(&canonical, &opts, &clock, &DecayConfig::default(), 1., true)
            .unwrap();
        assert_eq!(
            p.full_to_summary,
            usize::from(donor != "live"),
            "{donor}: cached strength zero must not replace canonical projection"
        );
        for (multiplier, enabled) in [(0., true), (1., false), (2., true)] {
            assert_eq!(
                preview_lifecycle(
                    &canonical,
                    &opts,
                    &clock,
                    &DecayConfig::default(),
                    multiplier,
                    enabled
                )
                .unwrap()
                .full_to_summary,
                0
            );
        }
        assert_eq!(files(&canonical), before);
    }
}

#[tokio::test]
async fn active_policy_receipt_without_its_metadata_changes_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    let v2 = root.path().join("v2");
    migrate_lifecycle(
        &source,
        &v2,
        &options(&source, LifecycleVersion::ExplicitFeedbackV2, "forward"),
    )
    .unwrap();
    let opts = options(&v2, LifecycleVersion::LegacyAccessV1, "orphan");
    // A syntactically valid, correctly keyed receipt is not evidence that the
    // namespace/records actually reached the claimed next policy revision.
    edit(&v2, |tx| {
        control::record_receipt(
            tx,
            &opts.context(),
            [7; 32],
            MutationOutcome::LifecyclePolicyMigrated {
                transition: PolicyTransition {
                    namespace_id: opts.namespace_id,
                    from: LifecycleVersion::ExplicitFeedbackV2,
                    to: LifecycleVersion::LegacyAccessV1,
                    previous_revision: opts.expected_policy_revision,
                    next_revision: opts.expected_policy_revision.next().unwrap(),
                    previous_generation: opts.expected_source.generation,
                    record_count: 4,
                    record_token_digest: "0".repeat(64),
                },
            },
        )
        .unwrap();
    });
    assert!(
        inspect_schema(&v2).is_err(),
        "every active suffix receipt must explain final namespace/record state"
    );
}

#[tokio::test]
async fn policy_revision_order_must_match_commits_within_each_origin() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path()).await;
    let first = root.path().join("first");
    let a = migrate_lifecycle(
        &source,
        &first,
        &options(&source, LifecycleVersion::ExplicitFeedbackV2, "forward"),
    )
    .unwrap();
    let second = root.path().join("second");
    let b = migrate_lifecycle(
        &first,
        &second,
        &options(&first, LifecycleVersion::LegacyAccessV1, "back"),
    )
    .unwrap();
    for (id, generation) in [
        (a.operation_id, b.committed_generation),
        (b.operation_id, a.committed_generation),
    ] {
        edit_receipt(&second, id, |r| {
            r.receipt.committed_generation = generation;
            let MutationOutcome::LifecyclePolicyMigrated { transition: t } = &mut r.receipt.outcome
            else {
                panic!()
            };
            t.previous_generation =
                Generation::try_from((generation.get() - 1).to_string()).unwrap();
        });
    }
    assert!(
        inspect_schema(&second).is_err(),
        "a contiguous suffix and matching token digests cannot justify reversed commits"
    );
}

#[tokio::test]
async fn schema_v2_copy_cannot_move_publication_past_policy_receipts() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture_attested(root.path(), true).await;
    let v2 = root.path().join("policy");
    migrate_lifecycle(
        &source,
        &v2,
        &options(&source, LifecycleVersion::ExplicitFeedbackV2, "forward"),
    )
    .unwrap();
    let inspection = inspect_schema(&v2).unwrap();
    let before = files(&v2);
    let dest = root.path().join("copy");
    let mut request = MigrationOptions {
        migration_now: 9999,
        expected_source: inspection.control.generation_ref(),
        attestations: vec![],
    };
    let report = migrate_schema(&v2, &dest, &request).unwrap();
    assert_eq!(
        Some(report.state),
        inspection.schema,
        "format-only copying must preserve original publication evidence"
    );
    assert!(report.receipt.is_none());
    assert_eq!(files(&dest), before);
    let ns = &inspection.namespaces[0];
    let crate::model::schema::EmbeddingBinding::Bound { descriptor, .. } =
        &ns.schema.as_ref().unwrap().embedding_binding
    else {
        panic!()
    };
    request
        .attestations
        .push(crate::storage::schema_migration::ProfileAttestationInput {
            namespace_id: ns.id,
            namespace_name: ns.name.clone(),
            descriptor: descriptor.clone(),
        });
    let rebound = root.path().join("rebound");
    assert!(
        migrate_schema(&v2, &rebound, &request).is_err(),
        "schema2 attestation needs a dedicated receipt/proof extension"
    );
    assert!(!rebound.exists());
    assert_eq!(files(&v2), before);
    edit(&dest, |tx| {
        let mut table = tx.open_table(META_TABLE).unwrap();
        let mut r = {
            let row = table.iter().unwrap().next().unwrap().unwrap();
            DiskRecord::from_bytes(row.1.value()).unwrap()
        };
        r.schema.as_mut().unwrap().lifecycle.record_generation = r
            .schema
            .as_ref()
            .unwrap()
            .lifecycle
            .record_generation
            .next()
            .unwrap();
        table
            .insert(r.id.as_slice(), r.encode().unwrap().as_slice())
            .unwrap();
    });
    assert!(
        inspect_schema(&dest).is_err(),
        "copy cannot hide a policy record-token mismatch behind a new baseline"
    );
}
