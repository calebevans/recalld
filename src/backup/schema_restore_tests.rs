//! Actual migration/backup/fork/inspection regressions; all stores are synthetic.
use super::tests::{fixture_with_embedding, retained_options};
use super::*;
use crate::model::schema::{EmbeddingProfile, Normalization, PreprocessingIdentity, ProviderKind};
use crate::model::{DecayPhase, EdgeType, MemoryId, NamespaceId, Tag};
use crate::storage::control::{self, Generation, MutationOutcome, MutationScope};
use crate::storage::metadata::{effects, restoration};
use crate::storage::offline_snapshot::OfflineSnapshot;
use crate::storage::schema_migration::{
    MigrationOptions, ProfileAttestationInput, inspect_schema, migrate_schema,
};
use crate::storage::{MetadataStore, PersistedEdge, RedbStorageEngine, StorageEngine};
use redb::{ReadableTable, ReadableTableMetadata};
use uuid::Uuid;

struct Fixture {
    target: PathBuf,
    migrated: PathBuf,
    backup: PathBuf,
    live: MemoryId,
    ghost: MemoryId,
    forgotten: MemoryId,
    deleted: MemoryId,
    namespace: NamespaceId,
    graph_operation: Uuid,
}

async fn fixture(root: &Path, attested: bool) -> Fixture {
    let (target, _) = fixture_with_embedding(root, &[1., 0., 0., 0.]);
    let mut engine = RedbStorageEngine::open(&target).unwrap();
    let (live, original) = engine.scan_all().unwrap().remove(0);
    let namespace = NamespaceId::new(original.namespace_id);
    let mut add = |summary: &str| {
        let id = MemoryId::new();
        let mut record = original.clone();
        record.id = *id.as_bytes();
        record.summary = summary.into();
        record.tags = vec![
            Tag::new("entity/secretlabel").unwrap(),
            Tag::new("topic/private").unwrap(),
            Tag::new("emotion/fear").unwrap(),
        ];
        record.text_offset = 0;
        record.text_length = 0;
        engine
            .insert_memory(
                id,
                namespace,
                &mut record,
                &[1., 0., 0., 0.],
                Some("secretpayload"),
            )
            .unwrap();
        id
    };
    let ghost = add("ghostsecret");
    let forgotten = add("forgottensecret");
    let deleted = add("deletedsecret");
    engine.sync().unwrap();
    // Real canonical effects: edge DB applied, derived acknowledgement withheld.
    let outcome = engine
        .put_edges_with_effects(&[PersistedEdge {
            source: live,
            target: deleted,
            edge_type: EdgeType::Associative,
            weight: 0.5,
            auto_created: false,
            created_at: 7,
        }])
        .unwrap()
        .unwrap();
    let pending = engine.meta_store().pending_effects().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].canonical_applied);
    let graph_operation = outcome.receipt.operation_id;
    drop(engine);
    let source = inspect_schema(&target).unwrap().control.generation_ref();
    let migrated = root.join("migrated");
    let attestations = if attested {
        vec![ProfileAttestationInput {
            namespace_id: namespace,
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
    };
    migrate_schema(
        &target,
        &migrated,
        &MigrationOptions {
            migration_now: 123_456,
            expected_source: source,
            attestations,
        },
    )
    .unwrap();
    let backup = crate::backup::run_backup(
        &crate::config::RecalldConfig::default(),
        &root.join("schema.zip"),
        Some(&migrated),
        false,
    )
    .await
    .unwrap();
    Fixture {
        target,
        migrated,
        backup,
        live,
        ghost,
        forgotten,
        deleted,
        namespace,
        graph_operation,
    }
}

fn newer_removals(f: &Fixture) {
    // Ordinary v2 writes remain blocked. Newer evidence is produced by the real
    // legacy writers on the retained source, after the offline snapshot was made.
    let mut engine = RedbStorageEngine::open(&f.target).unwrap();
    let ns = engine.get_namespace(f.namespace).unwrap().unwrap();
    for (phase, strength) in [(DecayPhase::Summary, 0.6), (DecayPhase::Ghost, 0.1)] {
        let old = engine.get_record(f.ghost).unwrap().unwrap();
        assert!(
            engine
                .apply_legacy_decay(f.ghost, &old, &ns, phase, strength, strength)
                .unwrap()
        );
    }
    engine.tombstone_memory(f.forgotten).unwrap();
    assert!(
        engine
            .delete_memory_with_edges(f.deleted, &[])
            .unwrap()
            .is_some()
    );
    engine.sync().unwrap();
}

fn schema_bytes(path: &Path) -> Vec<u8> {
    let (_copy, db) = crate::storage::validation::inspect_database(&path.join("meta.db")).unwrap();
    db.begin_read()
        .unwrap()
        .open_table(crate::storage::canonical_schema::SCHEMA)
        .unwrap()
        .get("state")
        .unwrap()
        .unwrap()
        .value()
        .to_vec()
}

fn assert_receipts_retained(
    before: &restoration::CapturedSnapshot,
    after: &restoration::CapturedSnapshot,
) {
    for row in &before.receipts {
        assert!(after.receipts.contains(row));
    }
    for row in &before.keys {
        assert!(after.keys.contains(row));
    }
}

#[tokio::test]
async fn migrated_format_and_attested_backups_fork_with_masks_and_consume_applied_effects() {
    for attested in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let f = fixture(root.path(), attested).await;
        let marker_before = schema_bytes(&f.migrated);
        let snapshot_before = restoration::capture(&f.migrated).unwrap();
        let view = OfflineSnapshot::open(&f.migrated).unwrap();
        let ghost_before = view.record(f.ghost).unwrap().unwrap().record;
        assert!(!ghost_before.schema.as_ref().unwrap().labels.is_empty());
        assert_eq!(
            effects::validate_existing(&view.metadata.begin_read().unwrap())
                .unwrap()
                .len(),
            1
        );
        drop(view);
        newer_removals(&f);
        let retained_before = restoration::capture(&f.target).unwrap();
        restore_from_backup(retained_options(&f.backup, &f.target))
            .await
            .unwrap();
        let inspected = inspect_schema(&f.target).unwrap();
        assert_ne!(inspected.control.store_id, snapshot_before.control.store_id);
        assert_eq!(inspected.control.committed_generation.get(), 1);
        let schema = inspected.schema.unwrap();
        assert_eq!(schema.receipt_operation_id.is_some(), attested);
        assert_eq!(
            schema_bytes(&f.target),
            marker_before,
            "immutable schema publication proof"
        );
        let captured = restoration::capture(&f.target).unwrap();
        assert_receipts_retained(&snapshot_before, &captured);
        assert_receipts_retained(&retained_before, &captured);
        assert!(captured.receipts.iter().any(|(_, bytes)| {
            control::validate_receipt(bytes)
                .unwrap()
                .receipt
                .operation_id
                == f.graph_operation
        }));
        let view = OfflineSnapshot::open(&f.target).unwrap();
        let read = view.metadata.begin_read().unwrap();
        assert!(effects::validate_existing(&read).unwrap().is_empty());
        assert_eq!(read.open_table(effects::EFFECTS).unwrap().len().unwrap(), 0);
        let ghost = view.record(f.ghost).unwrap().unwrap();
        assert_eq!(ghost.record.phase, DecayPhase::Ghost);
        assert!(ghost.record.summary.is_empty() && ghost.full_text.is_none());
        assert!(ghost.record.schema.as_ref().unwrap().labels.is_empty());
        assert_eq!(
            ghost.record.schema.as_ref().unwrap().lifecycle,
            ghost_before.schema.as_ref().unwrap().lifecycle,
            "ledger has no phase event timestamp; preserve existing inferred fields"
        );
        assert_eq!(
            ghost.record.strength, ghost_before.strength,
            "phase describes content, not an invented projection"
        );
        assert_eq!(
            ghost.record.tags, ghost_before.tags,
            "legacy mask did not remove tags"
        );
        assert_eq!(ghost.embedding, Some(vec![1., 0., 0., 0.]));
        let forgotten = view.record(f.forgotten).unwrap().unwrap();
        assert_eq!(forgotten.record.phase, DecayPhase::Tombstone);
        assert!(forgotten.record.schema.as_ref().unwrap().vector.is_none());
        assert!(forgotten.record.schema.as_ref().unwrap().labels.is_empty());
        assert!(forgotten.record.tags.is_empty() && forgotten.record.summary.is_empty());
        assert!(forgotten.full_text.is_none() && forgotten.embedding.is_none());
        assert!(view.record(f.deleted).unwrap().is_none());
        assert_eq!(view.record(f.live).unwrap().unwrap().record.edge_count, 0);
        view.for_each_edge(|_| panic!("deleted endpoint cannot survive in canonical graph"))
            .unwrap();
        drop(read);
        drop(view);
        let fts = crate::search::FtsIndex::new(&f.target).unwrap();
        for text in [
            "ghostsecret",
            "forgottensecret",
            "deletedsecret",
            "secretpayload",
        ] {
            assert!(
                fts.search(f.namespace, text, 10).unwrap().is_empty(),
                "retained {text}"
            );
        }
        assert!(!fts.search(f.namespace, "fixture", 10).unwrap().is_empty());
        drop(fts);
        let before = fs::read(f.target.join("meta.db")).unwrap();
        assert!(RedbStorageEngine::open(&f.target).is_err());
        assert!(MetadataStore::open(&f.target.join("meta.db")).is_err());
        assert_eq!(fs::read(f.target.join("meta.db")).unwrap(), before);
        // A second ancestor restore must retain the original schema origin/proof.
        restore_from_backup(retained_options(&f.backup, &f.target))
            .await
            .unwrap();
        assert_eq!(schema_bytes(&f.target), marker_before);
        assert!(
            OfflineSnapshot::open(&f.target)
                .unwrap()
                .record(f.deleted)
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn historical_schema_restore_is_inspectable_but_never_ordinary_serving() {
    for attested in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let f = fixture(root.path(), attested).await;
        let destination = root.path().join("historical");
        let mut opts = retained_options(&f.backup, &destination);
        opts.historical = true;
        restore_from_backup(opts).await.unwrap();
        let state = inspect_schema(&destination).unwrap();
        assert_eq!(
            state.schema.unwrap().receipt_operation_id.is_some(),
            attested
        );
        let before = fs::read(destination.join("meta.db")).unwrap();
        let snapshot = OfflineSnapshot::open(&destination).unwrap();
        assert!(
            snapshot.record(f.deleted).unwrap().is_some(),
            "historical mode cannot infer later deletions"
        );
        drop(snapshot);
        assert!(RedbStorageEngine::open(&destination).is_err());
        assert!(MetadataStore::open(&destination.join("meta.db")).is_err());
        assert_eq!(fs::read(destination.join("meta.db")).unwrap(), before);
    }
}

#[tokio::test]
async fn forked_attestation_rejects_removed_pointer_and_invalid_historical_proofs() {
    for bad in [
        "pointer",
        "source_gap",
        "zero_count",
        "unknown_origin",
        "bound",
        "wrong_receipt_origin",
    ] {
        let root = tempfile::tempdir().unwrap();
        let f = fixture(root.path(), true).await;
        restore_from_backup(retained_options(&f.backup, &f.target))
            .await
            .unwrap();
        let db = redb::Database::open(f.target.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        let mut table = tx
            .open_table(crate::storage::canonical_schema::SCHEMA)
            .unwrap();
        let mut state: crate::storage::canonical_schema::CanonicalSchemaState =
            control::decode(table.get("state").unwrap().unwrap().value()).unwrap();
        let operation = state.receipt_operation_id.unwrap();
        match bad {
            "pointer" => state.receipt_operation_id = None,
            "source_gap" => {
                state.source.generation =
                    Generation::try_from((state.source.generation.get() - 1).to_string()).unwrap()
            }
            "unknown_origin" => state.source.store_id = Uuid::new_v4(),
            "bound" => {
                let mut lineage_table = tx.open_table(crate::storage::lineage::LINEAGE).unwrap();
                let mut lineage: crate::storage::lineage::LineageState =
                    control::decode(lineage_table.get("state").unwrap().unwrap().value()).unwrap();
                for origin in &mut lineage.ancestors {
                    if origin.store_id == state.source.store_id {
                        origin.generation = state.source.generation;
                    }
                }
                lineage_table
                    .insert("state", control::encode(&lineage).unwrap().as_slice())
                    .unwrap();
            }
            "zero_count" | "wrong_receipt_origin" => {
                let key = control::scoped_key(
                    state.source.store_id,
                    MutationScope::Catalog,
                    operation.as_bytes(),
                );
                let mut receipts = tx.open_table(control::RECEIPTS).unwrap();
                let mut stored = control::validate_receipt(
                    receipts.get(key.as_slice()).unwrap().unwrap().value(),
                )
                .unwrap();
                if bad == "zero_count" {
                    if let MutationOutcome::SchemaMigrated {
                        attested_namespaces,
                        ..
                    } = &mut stored.receipt.outcome
                    {
                        *attested_namespaces = 0;
                    } else {
                        panic!("migration receipt expected");
                    }
                } else {
                    stored.receipt.store_id = Uuid::new_v4();
                }
                receipts
                    .insert(key.as_slice(), control::encode(&stored).unwrap().as_slice())
                    .unwrap();
            }
            _ => unreachable!(),
        }
        table
            .insert("state", control::encode(&state).unwrap().as_slice())
            .unwrap();
        drop(table);
        tx.commit().unwrap();
        drop(db);
        let before = fs::read(f.target.join("meta.db")).unwrap();
        assert!(inspect_schema(&f.target).is_err(), "accepted {bad}");
        assert!(OfflineSnapshot::open(&f.target).is_err(), "accepted {bad}");
        assert!(RedbStorageEngine::open(&f.target).is_err());
        assert_eq!(
            fs::read(f.target.join("meta.db")).unwrap(),
            before,
            "inspection wrote after {bad}"
        );
    }
}

#[tokio::test]
async fn private_rebuild_failure_does_not_consume_applied_effects_or_publish_fork() {
    let root = tempfile::tempdir().unwrap();
    let f = fixture(root.path(), true).await;
    let manifest = read_manifest(&f.backup).unwrap();
    let stage = root.path().join("private-stage");
    extract_backup(&f.backup, &stage, &manifest).unwrap();
    let source = restoration::capture(&stage).unwrap();
    let retained = restoration::capture(&f.target).unwrap();
    let target_before = fs::read(f.target.join("meta.db")).unwrap();
    fs::create_dir(stage.join("fts.db")).unwrap(); // real SQLite open failure
    assert!(
        restoration::reconcile(
            &stage,
            &source,
            Some(&retained),
            crate::storage::lineage::RestoreMode::Serving,
            Uuid::new_v4(),
            Uuid::new_v4(),
            [0; 32]
        )
        .is_err()
    );
    let (_copy, db) = crate::storage::validation::inspect_database(&stage.join("meta.db")).unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(
        control::validate_existing(&read).unwrap().unwrap(),
        source.control
    );
    let pending = effects::validate_existing(&read).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].canonical_applied);
    assert_eq!(pending[0].receipt.operation_id, f.graph_operation);
    assert_eq!(fs::read(f.target.join("meta.db")).unwrap(), target_before);
}

#[tokio::test]
async fn backup_and_restore_reject_unapplied_effects_without_rewriting_evidence() {
    let root = tempfile::tempdir().unwrap();
    let f = fixture(root.path(), true).await;
    let db = redb::Database::open(f.migrated.join("meta.db")).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(effects::EFFECTS).unwrap();
        let key = f.graph_operation.as_bytes().as_slice();
        let mut pending: effects::PendingEffects =
            control::decode(table.get(key).unwrap().unwrap().value()).unwrap();
        pending.canonical_applied = false;
        table
            .insert(key, control::encode(&pending).unwrap().as_slice())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let before = fs::read(f.migrated.join("meta.db")).unwrap();
    let bad = root.path().join("pending.zip");
    assert!(
        crate::backup::run_backup(
            &crate::config::RecalldConfig::default(),
            &bad,
            Some(&f.migrated),
            false
        )
        .await
        .is_err()
    );
    assert!(!bad.exists());
    // A CRC-consistent forged archive must still fail semantic validation.
    let manifest = read_manifest(&f.backup).unwrap();
    let files: Vec<_> = manifest
        .files
        .iter()
        .map(|e| f.migrated.join(&e.name))
        .collect();
    let forged = super::tests::checksum_valid_manifest(&f.migrated, manifest);
    crate::backup::archive::create_archive(&f.migrated, &files, &forged, &bad).unwrap();
    let retained_before = fs::read(f.target.join("meta.db")).unwrap();
    assert!(
        restore_from_backup(retained_options(&bad, &f.target))
            .await
            .is_err()
    );
    assert_eq!(fs::read(f.migrated.join("meta.db")).unwrap(), before);
    assert_eq!(fs::read(f.target.join("meta.db")).unwrap(), retained_before);
}

#[tokio::test]
async fn independent_v2_field_masks_preserve_unremoved_labels_and_payloads() {
    for mask in ["text", "summary", "tags"] {
        let root = tempfile::tempdir().unwrap();
        let f = fixture(root.path(), false).await;
        let engine = RedbStorageEngine::open(&f.target).unwrap();
        match mask {
            "text" => engine.meta_store().strip_full_text(f.ghost).unwrap(),
            "summary" => engine.meta_store().strip_summary(f.ghost).unwrap(),
            "tags" => {
                let mut record = engine.get_record(f.ghost).unwrap().unwrap();
                record.tags.clear();
                engine
                    .meta_store()
                    .batch_update_records(&[(f.ghost, record)])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        drop(engine);
        restore_from_backup(retained_options(&f.backup, &f.target))
            .await
            .unwrap();
        let view = OfflineSnapshot::open(&f.target).unwrap();
        let item = view.record(f.ghost).unwrap().unwrap();
        assert_eq!(item.full_text.is_none(), mask == "text");
        assert_eq!(item.record.summary.is_empty(), mask == "summary");
        assert_eq!(item.record.tags.is_empty(), mask == "tags");
        assert_eq!(
            item.record.schema.as_ref().unwrap().labels.is_empty(),
            mask == "tags"
        );
        assert_eq!(
            item.record.phase,
            if mask == "text" {
                DecayPhase::Summary
            } else {
                DecayPhase::Full
            }
        );
        assert!(item.embedding.is_some());
    }
}

#[tokio::test]
async fn schema_journal_restart_finishes_fork_then_refuses_ordinary_v2_serving() {
    let root = tempfile::tempdir().unwrap();
    let f = fixture(root.path(), true).await;
    let marker = schema_bytes(&f.migrated);
    newer_removals(&f);
    assert!(
        restore_at(retained_options(&f.backup, &f.target), |stage| {
            if matches!(stage, crate::storage::restore_journal::Stage::OldMoved) {
                Err(StorageError::RecoveryRequired(
                    "injected stop after old rename".into(),
                ))
            } else {
                Ok(())
            }
        })
        .await
        .is_err()
    );
    let pending = root.path().join("source.recalld-restore-pending");
    assert!(pending.exists());
    assert!(inspect_schema(&f.target).is_err());
    assert!(pending.exists(), "inspection must not finish the restore");
    assert!(
        RedbStorageEngine::open(&f.target).is_err(),
        "recovery is not permission to serve v2"
    );
    assert!(!pending.exists());
    assert_eq!(schema_bytes(&f.target), marker);
    for _ in 0..2 {
        inspect_schema(&f.target).unwrap();
        let view = OfflineSnapshot::open(&f.target).unwrap();
        assert!(view.record(f.deleted).unwrap().is_none());
        assert!(
            effects::validate_existing(&view.metadata.begin_read().unwrap())
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn unknown_record_extensions_roundtrip_only_when_no_removal_is_required() {
    for masked in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let f = fixture(root.path(), false).await;
        let id = if masked { f.ghost } else { f.live };
        // Explicit offline codec fixture; this does not enable a v2 writer API.
        let db = redb::Database::open(f.migrated.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::storage::metadata::META_TABLE).unwrap();
            let mut record = crate::model::DiskRecord::from_bytes(
                table
                    .get(id.as_bytes().as_slice())
                    .unwrap()
                    .unwrap()
                    .value(),
            )
            .unwrap();
            record.schema.as_mut().unwrap().extensions.insert(
                "secret_text".into(),
                serde_json::json!("opaque retained payload"),
            );
            table
                .insert(
                    id.as_bytes().as_slice(),
                    record.encode().unwrap().as_slice(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let backup = crate::backup::run_backup(
            &crate::config::RecalldConfig::default(),
            &root.path().join("extensions.zip"),
            Some(&f.migrated),
            false,
        )
        .await
        .unwrap();
        newer_removals(&f);
        let before = fs::read(f.target.join("meta.db")).unwrap();
        let outcome = restore_from_backup(retained_options(&backup, &f.target)).await;
        if masked {
            assert!(
                outcome
                    .unwrap_err()
                    .to_string()
                    .contains("unknown record extensions")
            );
            assert_eq!(fs::read(f.target.join("meta.db")).unwrap(), before);
            assert!(!root.path().join("source.recalld-restore-pending").exists());
        } else {
            outcome.unwrap();
            let view = OfflineSnapshot::open(&f.target).unwrap();
            assert_eq!(
                view.record(id)
                    .unwrap()
                    .unwrap()
                    .record
                    .schema
                    .unwrap()
                    .extensions["secret_text"],
                serde_json::json!("opaque retained payload")
            );
        }
    }
}
