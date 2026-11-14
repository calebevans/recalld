//! Offline canonical-schema conversion into a fresh, non-serving destination.
//! Canonical files are copied whole: receipts, effects and removal history are
//! never reconstructed from live records or omitted from migration.
use std::collections::{HashMap, HashSet};
use std::path::Path;

use redb::{ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::canonical_schema::{CanonicalSchemaState, ServingMode};
use super::control::{
    self, ControlState, Generation, GenerationRef, MutationContext, MutationOutcome, MutationScope,
    WriteReceipt,
};
use super::fsync::fsync_dir;
use super::metadata::{META_TABLE, NAMESPACE_TABLE};
use super::ownership::{StoreOwnership, lock_directory};
use super::{StorageError, VectorStore};
use crate::embedding::profile::prepare_attestation;
use crate::model::schema::{
    BindingProvenance, EmbeddingBinding, EmbeddingProfile, NamespaceSchemaV2, RecordSchemaV2,
    VectorReference,
};
use crate::model::{DecayPhase, DiskRecord, NamespaceConfig, NamespaceId};

/// Supplying this value explicitly asserts that ALL retained vectors in the
/// named namespace were produced with this exact descriptor. Storage can check
/// ownership/shape/norm, but cannot infer the producing model from vector bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileAttestationInput {
    pub namespace_id: NamespaceId,
    pub namespace_name: String,
    pub descriptor: EmbeddingProfile,
}

#[derive(Debug, Clone)]
pub struct MigrationOptions {
    pub migration_now: i64,
    pub expected_source: GenerationRef,
    pub attestations: Vec<ProfileAttestationInput>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    pub state: CanonicalSchemaState,
    pub receipt: Option<WriteReceipt>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaInspection {
    pub control: ControlState,
    pub schema: Option<CanonicalSchemaState>,
    pub namespaces: Vec<NamespaceConfig>,
    pub record_count: u64,
}

/// Read-only canonical inspection; neither effects replay nor daemon startup.
pub fn inspect_schema(source: &Path) -> Result<SchemaInspection, StorageError> {
    if !source.is_dir() {
        return Err(StorageError::InvalidPath);
    }
    let owner = StoreOwnership::acquire(source)?;
    let _lock = lock_directory(&owner.path)?;
    super::validation::canonical_files(&owner.path)?;
    inspect_owned(&owner.path)
}

fn inspect_owned(source: &Path) -> Result<SchemaInspection, StorageError> {
    let (_copy, db) = super::validation::inspect_database(&source.join("meta.db"))?;
    let tx = db.begin_read()?;
    let control = control::validate_existing(&tx)?.ok_or_else(|| {
        StorageError::RecoveryRequired(
            "source has no durable lineage; use offline text migration first".into(),
        )
    })?;
    let schema = super::canonical_schema::read_state(&tx, &control)?;
    let namespaces = tx
        .open_table(NAMESPACE_TABLE)?
        .iter()?
        .map(|row| {
            let (_, value) = row?;
            NamespaceConfig::decode(value.value()).map_err(StorageError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let record_count = tx.open_table(META_TABLE)?.len()?;
    Ok(SchemaInspection {
        control,
        schema,
        namespaces,
        record_count,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Copied,
    Converted,
    Validated,
    Published,
}

pub fn migrate_schema(
    source: &Path,
    destination: &Path,
    options: &MigrationOptions,
) -> Result<MigrationReport, StorageError> {
    migrate_at(source, destination, options, |_| Ok(()))
}

fn migrate_at(
    source: &Path,
    destination: &Path,
    options: &MigrationOptions,
    mut boundary: impl FnMut(Stage) -> Result<(), StorageError>,
) -> Result<MigrationReport, StorageError> {
    if chrono::DateTime::from_timestamp_millis(options.migration_now).is_none() {
        return Err(StorageError::InvalidMutation(
            "unrepresentable migration timestamp".into(),
        ));
    }
    if !source.is_dir() {
        return Err(StorageError::InvalidPath);
    }
    let source_owner = StoreOwnership::acquire(source)?;
    let source = &source_owner.path;
    let _source_lock = lock_directory(source)?;
    let absolute = std::path::absolute(destination)?;
    let parent = absolute
        .parent()
        .ok_or(StorageError::InvalidPath)?
        .canonicalize()?;
    let target = parent.join(absolute.file_name().ok_or(StorageError::InvalidPath)?);
    if target.starts_with(source) || source.starts_with(&target) {
        return Err(StorageError::InvalidPath);
    }
    let target_owner = StoreOwnership::acquire(&target)?;
    let target = &target_owner.path;
    if target.try_exists()? {
        return Err(StorageError::InvalidMutation(
            "migration destination must be fresh; inspect an existing destination before retrying"
                .into(),
        ));
    }
    // This shared validation must reject unresolved canonical effects and invalid
    // ledger/control state. It never applies effects during offline inspection.
    let files = super::validation::canonical_files(source)?;
    let inspection = inspect_owned(source)?;
    if inspection.control.store_id != options.expected_source.store_id {
        return Err(StorageError::StoreLineageMismatch);
    }
    if inspection.control.committed_generation != options.expected_source.generation {
        return Err(StorageError::GenerationConflict {
            expected: options.expected_source.generation.into(),
            actual: inspection.control.committed_generation.into(),
        });
    }
    {
        let (_copy, db) = super::validation::inspect_database(&source.join("meta.db"))?;
        if super::metadata::text_generation::read_selection(&db.begin_read()?)?.is_none() {
            return Err(StorageError::RecoveryRequired("schema migration requires a fenced selected-text store; use offline text migration into a fresh destination first".into()));
        }
    }
    let stage = tempfile::Builder::new()
        .prefix(".recalld-schema-migration-")
        .tempdir_in(&parent)?;
    for file in files {
        let copied = stage.path().join(
            file.strip_prefix(source)
                .map_err(|_| StorageError::InvalidPath)?,
        );
        let directory = copied.parent().ok_or(StorageError::InvalidPath)?;
        std::fs::create_dir_all(directory)?;
        std::fs::copy(file, &copied)?;
        std::fs::File::open(&copied)?.sync_all()?;
        fsync_dir(directory)?;
    }
    boundary(Stage::Copied)?;
    let report = convert_staging(stage.path(), options)?;
    boundary(Stage::Converted)?;
    for file in super::validation::canonical_files(stage.path())? {
        std::fs::File::open(file)?.sync_all()?;
    }
    fsync_dir(stage.path())?;
    boundary(Stage::Validated)?;
    if target.try_exists()? {
        return Err(StorageError::InvalidMutation(
            "migration destination appeared during staging".into(),
        ));
    }
    std::fs::rename(stage.path(), target)?;
    fsync_dir(&parent).map_err(|e| {
        StorageError::RecoveryRequired(format!(
            "migration {} published at {}; inspect before retrying: {e}",
            report.state.migration_id,
            target.display()
        ))
    })?;
    boundary(Stage::Published)?;
    Ok(report)
}

fn convert_staging(
    path: &Path,
    options: &MigrationOptions,
) -> Result<MigrationReport, StorageError> {
    // Only this inaccessible copy is writable. Complete original canonical
    // tables, including opaque retained receipts, stay in place.
    let db = redb::Database::open(path.join("meta.db"))?;
    let mut namespaces = HashMap::new();
    let mut vectors = HashMap::new();
    let mut records = Vec::new();
    {
        let read = db.begin_read()?;
        for row in read.open_table(NAMESPACE_TABLE)?.iter()? {
            let (_, bytes) = row?;
            let mut ns = NamespaceConfig::decode(bytes.value())?;
            if ns.schema.is_none() {
                ns.schema = Some(NamespaceSchemaV2::from_legacy());
            }
            ns.schema
                .as_ref()
                .unwrap()
                .validate(&ns)
                .map_err(StorageError::InvalidMutation)?;
            vectors.insert(
                ns.id.get(),
                VectorStore::open(&path.join(&ns.name), ns.embedding_dim as usize)?,
            );
            namespaces.insert(ns.id.get(), ns);
        }
        let mut seen = HashSet::new();
        for input in &options.attestations {
            if !seen.insert(input.namespace_id.get()) {
                return Err(StorageError::InvalidMutation(
                    "duplicate namespace attestation".into(),
                ));
            }
            let ns = namespaces
                .get_mut(&input.namespace_id.get())
                .ok_or(StorageError::NamespaceNotFound(input.namespace_id.get()))?;
            if ns.name != input.namespace_name {
                return Err(StorageError::InvalidMutation(
                    "attestation namespace name/ID mismatch".into(),
                ));
            }
            let prepared = prepare_attestation(ns, input.namespace_id, input.descriptor.clone())
                .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
            ns.schema.as_mut().unwrap().embedding_binding = EmbeddingBinding::Bound {
                profile_id: prepared.profile_id().to_owned(),
                descriptor: prepared.descriptor().clone(),
                provenance: BindingProvenance::OperatorAttestation,
            };
        }
        for row in read.open_table(META_TABLE)?.iter()? {
            let (_, bytes) = row?;
            let mut record = DiskRecord::from_bytes(bytes.value())?;
            let ns = namespaces
                .get(&record.namespace_id)
                .ok_or(StorageError::NamespaceNotFound(record.namespace_id))?;
            let vector = if record.phase == DecayPhase::Tombstone {
                None
            } else {
                let values = vectors
                    .get(&record.namespace_id)
                    .unwrap()
                    .get_vector(record.vector_slot)
                    .ok_or_else(|| {
                        StorageError::OwnershipMismatch("record vector missing".into())
                    })?;
                if matches!(
                    ns.schema.as_ref().unwrap().embedding_binding,
                    EmbeddingBinding::Bound { .. }
                ) {
                    super::canonical_schema::validate_bound_vector(values)?;
                }
                let mut hash = Sha256::new();
                for value in values {
                    hash.update(value.to_le_bytes());
                }
                Some(VectorReference {
                    slot: record.vector_slot,
                    allocation_epoch: Generation::default(),
                    sha256: hash.finalize().into(),
                })
            };
            if record.schema.is_none() {
                record.schema = Some(RecordSchemaV2::from_legacy(
                    &record,
                    options.migration_now,
                    vector,
                ));
                record.version = 2;
            }
            // Existing v2 records, labels and optional extensions are not rebuilt.
            records.push((record.id, record.encode()?));
        }
    }
    let tx = db.begin_write()?;
    let ctx = MutationContext {
        store_id: Some(options.expected_source.store_id),
        scope: MutationScope::Catalog,
        idempotency_key: None,
        expected_generation: Some(options.expected_source.generation),
        contract_version: 1,
    };
    let inputs = control::encode(&options.attestations)?;
    let hash = control::request_hash(
        &ctx,
        "migrateCanonicalSchema",
        &[&options.migration_now.to_be_bytes(), &inputs],
    );
    control::replay_or_check(&tx, &ctx, hash)?;
    {
        let mut table = tx.open_table(NAMESPACE_TABLE)?;
        for ns in namespaces.values() {
            table.insert(ns.id.get(), control::encode(ns)?.as_slice())?;
        }
    }
    {
        let mut table = tx.open_table(META_TABLE)?;
        for (id, bytes) in &records {
            table.insert(id.as_slice(), bytes.as_slice())?;
        }
    }
    let mut control = control::read_control(&tx)?;
    control.control_schema_version = 2;
    tx.open_table(control::CONTROL)?
        .insert("state", control::encode(&control)?.as_slice())?;
    let migration_id = uuid::Uuid::new_v4();
    let receipt = if options.attestations.is_empty() {
        None
    } else {
        Some(control::record_receipt(
            &tx,
            &ctx,
            hash,
            MutationOutcome::SchemaMigrated {
                migration_id,
                record_count: records.len() as u64,
                attested_namespaces: options.attestations.len() as u64,
            },
        )?)
    };
    let state = CanonicalSchemaState {
        canonical_schema_version: 2,
        migration_id,
        receipt_operation_id: receipt.as_ref().map(|r| r.operation_id),
        migration_now: options.migration_now,
        source: options.expected_source,
        serving_mode: ServingMode::Offline,
    };
    tx.open_table(super::canonical_schema::SCHEMA)?
        .insert("state", control::encode(&state)?.as_slice())?;
    tx.commit()?;
    Ok(MigrationReport { state, receipt })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::schema::{LifecycleVersion, TimestampOrigin};
    use crate::storage::{MetadataStore, RedbStorageEngine, StorageEngine, TextStore};
    use crate::test_support::{Fixture, MemorySpec};

    async fn fixture() -> tempfile::TempDir {
        let fixture = Fixture::new();
        for phase in [
            DecayPhase::Full,
            DecayPhase::Summary,
            DecayPhase::Ghost,
            DecayPhase::Tombstone,
        ] {
            let content = matches!(phase, DecayPhase::Full | DecayPhase::Summary);
            let id = fixture
                .insert_memory_with(MemorySpec {
                    phase: if phase == DecayPhase::Tombstone {
                        DecayPhase::Full
                    } else {
                        phase
                    },
                    summary: if content { "retained résumé" } else { "" },
                    tags: if content {
                        &["entity/john-smith", "topic/storage"]
                    } else {
                        &[]
                    },
                    embedding: Some(&[1., -0., 0., 0.]),
                    ..MemorySpec::new(NamespaceId::new(1))
                })
                .await;
            if phase == DecayPhase::Tombstone {
                fixture
                    .storage
                    .read()
                    .unwrap()
                    .tombstone_memory(id)
                    .unwrap();
            }
        }
        fixture.storage.write().unwrap().sync().unwrap();
        fixture.close()
    }

    fn options(source: &Path) -> MigrationOptions {
        MigrationOptions {
            migration_now: 123_456,
            expected_source: inspect_schema(source).unwrap().control.generation_ref(),
            attestations: vec![],
        }
    }

    fn snapshot(path: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        super::super::validation::canonical_files(path)
            .unwrap()
            .into_iter()
            .map(|file| {
                (
                    file.strip_prefix(path).unwrap().to_path_buf(),
                    std::fs::read(file).unwrap(),
                )
            })
            .collect()
    }

    fn records(path: &Path) -> Vec<DiskRecord> {
        let (_copy, db) =
            super::super::validation::inspect_database(&path.join("meta.db")).unwrap();
        let read = db.begin_read().unwrap();
        read.open_table(META_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .map(|row| DiskRecord::from_bytes(row.unwrap().1.value()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn migration_preserves_legacy_bytes_defaults_and_offline_restart() {
        let source = fixture().await;
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("v2");
        let options = options(source.path());
        let before = snapshot(source.path());
        let old_records = records(source.path());
        let report = migrate_schema(source.path(), &target, &options).unwrap();
        assert!(report.receipt.is_none());
        assert_eq!(
            inspect_schema(&target).unwrap().control.generation_ref(),
            options.expected_source
        );
        for (old, new) in old_records.iter().zip(records(&target)) {
            let mut base = new.clone();
            base.schema = None;
            base.version = 1;
            assert_eq!(base.encode().unwrap(), old.encode().unwrap());
            let fields = new.schema.unwrap().lifecycle;
            assert_eq!(fields.timestamp_origin, TimestampOrigin::LegacyInferred);
            assert_eq!(fields.observed_at, old.created_at);
            assert_eq!(fields.decay_anchor_at, old.last_accessed_at);
            assert_eq!(fields.phase_changed_at, options.migration_now);
            assert_eq!(fields.retrieved_at, None);
            assert_eq!(fields.reinforced_at, None);
        }
        let durable = snapshot(&target);
        for _ in 0..2 {
            let inspection = inspect_schema(&target).unwrap();
            assert_eq!(inspection.schema.unwrap(), report.state);
            let schema = inspection.namespaces[0].schema.as_ref().unwrap();
            assert_eq!(schema.lifecycle_version, LifecycleVersion::LegacyAccessV1);
            assert_eq!(schema.embedding_binding, EmbeddingBinding::UnknownLegacy);
            assert!(RedbStorageEngine::open(&target).is_err());
            assert!(MetadataStore::open(&target.join("meta.db")).is_err());
            assert!(TextStore::open(&target.join("fulltext.dat")).is_err());
            assert_eq!(snapshot(&target), durable);
        }
        assert_eq!(snapshot(source.path()), before);
        assert!(migrate_schema(source.path(), &target, &options).is_err());
        assert_eq!(snapshot(&target), durable);
        let next = parent.path().join("copy");
        let mut again = options.clone();
        again.expected_source = inspect_schema(&target).unwrap().control.generation_ref();
        again.migration_now += 10;
        migrate_schema(&target, &next, &again).unwrap();
        assert_eq!(
            records(&next)
                .iter()
                .map(|r| r.encode().unwrap())
                .collect::<Vec<_>>(),
            records(&target)
                .iter()
                .map(|r| r.encode().unwrap())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn schema_interruption_child() {
        let Ok(source) = std::env::var("RECALLD_SCHEMA_TEST_SOURCE") else {
            return;
        };
        let target = std::env::var("RECALLD_SCHEMA_TEST_TARGET").unwrap();
        let stop = std::env::var("RECALLD_SCHEMA_TEST_STOP").unwrap();
        migrate_at(
            Path::new(&source),
            Path::new(&target),
            &options(Path::new(&source)),
            |stage| {
                if format!("{stage:?}") == stop {
                    std::process::exit(75);
                }
                Ok(())
            },
        )
        .unwrap();
        panic!("boundary not reached");
    }

    #[tokio::test]
    async fn invalid_schema_state_fails_before_canonical_writable_open() {
        for missing in [false, true] {
            let source = fixture().await;
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            migrate_schema(source.path(), &target, &options(source.path())).unwrap();
            {
                let db = redb::Database::open(target.join("meta.db")).unwrap();
                let tx = db.begin_write().unwrap();
                if missing {
                    tx.delete_table(super::super::canonical_schema::SCHEMA)
                        .unwrap();
                } else {
                    let mut table = tx
                        .open_table(super::super::canonical_schema::SCHEMA)
                        .unwrap();
                    let mut state: CanonicalSchemaState =
                        control::decode(table.get("state").unwrap().unwrap().value()).unwrap();
                    state.canonical_schema_version = 999;
                    table
                        .insert("state", control::encode(&state).unwrap().as_slice())
                        .unwrap();
                }
                tx.commit().unwrap();
            }
            let before = std::fs::read(target.join("meta.db")).unwrap();
            assert!(inspect_schema(&target).is_err());
            assert!(RedbStorageEngine::open(&target).is_err());
            assert!(MetadataStore::open(&target.join("meta.db")).is_err());
            assert_eq!(std::fs::read(target.join("meta.db")).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn malformed_attestation_evidence_is_rejected_without_changing_canonical_files() {
        use crate::model::schema::{Normalization, PreprocessingIdentity, ProviderKind};
        for fault in [
            "missing_operation",
            "source_gap",
            "zero_attestations",
            "unverified_origin",
        ] {
            let source = fixture().await;
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("attested");
            let mut input = options(source.path());
            let namespace = inspect_schema(source.path()).unwrap().namespaces.remove(0);
            input.attestations.push(ProfileAttestationInput {
                namespace_id: namespace.id,
                namespace_name: namespace.name,
                descriptor: EmbeddingProfile {
                    identity_version: 1,
                    provider_kind: ProviderKind::Ollama,
                    model_id: "explicit-known-model".into(),
                    model_revision: None,
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
            });
            let report = migrate_schema(source.path(), &target, &input).unwrap();
            let receipt = report.receipt.unwrap();
            assert_eq!(
                receipt.committed_generation,
                input.expected_source.generation.next().unwrap()
            );
            assert!(inspect_schema(&target).is_ok());
            {
                let db = redb::Database::open(target.join("meta.db")).unwrap();
                let tx = db.begin_write().unwrap();
                if fault == "zero_attestations" {
                    let key = control::scoped_key(
                        receipt.store_id,
                        MutationScope::Catalog,
                        receipt.operation_id.as_bytes(),
                    );
                    let mut table = tx.open_table(control::RECEIPTS).unwrap();
                    let mut stored: serde_json::Value =
                        control::decode(table.get(key.as_slice()).unwrap().unwrap().value())
                            .unwrap();
                    stored["receipt"]["outcome"]["attestedNamespaces"] = serde_json::json!(0);
                    table
                        .insert(key.as_slice(), control::encode(&stored).unwrap().as_slice())
                        .unwrap();
                } else {
                    let mut table = tx
                        .open_table(super::super::canonical_schema::SCHEMA)
                        .unwrap();
                    let mut state: serde_json::Value =
                        control::decode(table.get("state").unwrap().unwrap().value()).unwrap();
                    match fault {
                        "missing_operation" => {
                            state.as_object_mut().unwrap().remove("receiptOperationId");
                        }
                        "source_gap" => state["source"]["generation"] = serde_json::json!("0"),
                        _ => state["source"]["storeId"] = serde_json::json!(uuid::Uuid::new_v4()),
                    }
                    table
                        .insert("state", control::encode(&state).unwrap().as_slice())
                        .unwrap();
                }
                tx.commit().unwrap();
            }
            let before = std::fs::read(target.join("meta.db")).unwrap();
            assert!(inspect_schema(&target).is_err(), "{fault}");
            assert!(RedbStorageEngine::open(&target).is_err(), "{fault}");
            assert!(
                MetadataStore::open(&target.join("meta.db")).is_err(),
                "{fault}"
            );
            assert_eq!(
                std::fs::read(target.join("meta.db")).unwrap(),
                before,
                "{fault}"
            );
        }
    }

    #[tokio::test]
    async fn interrupted_schema_publication_is_absent_or_complete_and_offline() {
        for stop in [
            Stage::Copied,
            Stage::Converted,
            Stage::Validated,
            Stage::Published,
        ] {
            let source = fixture().await;
            let before = snapshot(source.path());
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::schema_migration::tests::schema_interruption_child",
                    "--nocapture",
                ])
                .env("RECALLD_SCHEMA_TEST_SOURCE", source.path())
                .env("RECALLD_SCHEMA_TEST_TARGET", &target)
                .env("RECALLD_SCHEMA_TEST_STOP", format!("{stop:?}"))
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(75));
            assert_eq!(snapshot(source.path()), before);
            if stop == Stage::Published {
                assert!(inspect_schema(&target).unwrap().schema.is_some());
                assert!(RedbStorageEngine::open(&target).is_err());
            } else {
                assert!(!target.exists());
                migrate_schema(source.path(), &target, &options(source.path())).unwrap();
            }
        }
    }
}
