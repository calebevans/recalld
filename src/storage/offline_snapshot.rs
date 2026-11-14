//! Explicit offline canonical reads. This is not a serving engine or a recovery
//! writer: pending canonical effects must be recovered before opening a snapshot.
use std::collections::HashMap;
use std::path::Path;

use redb::{ReadableTable, ReadableTableMetadata};

use super::control::ControlState;
use super::metadata::{META_TABLE, NAMESPACE_TABLE};
use super::ownership::{StoreOwnership, lock_directory};
use super::{StorageError, TextRef, TextStore, VectorStore};
use crate::model::{DecayPhase, DiskRecord, MemoryId, NamespaceConfig};

pub struct OfflineRecord {
    pub record: DiskRecord,
    pub full_text: Option<String>,
    pub embedding: Option<Vec<f32>>,
}

/// Owns the source for its entire lifetime. Metadata is read from a disposable
/// validated copy; payload reads hold their file handles and source ownership.
/// Drop the snapshot before opening a writer. No provider calls or effects replay.
pub struct OfflineSnapshot {
    pub(crate) metadata: redb::Database,
    pub(crate) edges: redb::Database,
    control: ControlState,
    namespaces: Vec<NamespaceConfig>,
    vectors: HashMap<u32, VectorStore>,
    text: TextStore,
    _meta_copy: tempfile::TempDir,
    _edges_copy: tempfile::TempDir,
    _source_lock: std::fs::File,
    _owner: StoreOwnership,
}

impl OfflineSnapshot {
    pub fn open(source: &Path) -> Result<Self, StorageError> {
        if !source.is_dir() {
            return Err(StorageError::InvalidPath);
        }
        let owner = StoreOwnership::acquire(source)?;
        let source = &owner.path;
        let source_lock = lock_directory(source)?;
        super::validation::canonical_files(source)?;
        let (meta_copy, metadata) = super::validation::inspect_database(&source.join("meta.db"))?;
        let (edges_copy, edges) = super::validation::inspect_database(&source.join("edges.db"))?;
        let read = metadata.begin_read()?;
        let control = super::control::validate_existing(&read)?.ok_or_else(|| {
            StorageError::RecoveryRequired(
                "offline snapshot requires an adopted durable lineage".into(),
            )
        })?;
        let mut namespaces = Vec::new();
        let mut vectors = HashMap::new();
        for row in read.open_table(NAMESPACE_TABLE)?.iter()? {
            let (_, value) = row?;
            let ns = NamespaceConfig::decode(value.value())?;
            vectors.insert(
                ns.id.get(),
                VectorStore::open(&source.join(&ns.name), ns.embedding_dim as usize)?,
            );
            namespaces.push(ns);
        }
        let text = match super::metadata::text_generation::read_selection(&read)? {
            Some(selected) => TextStore::open_generation(source, &selected, false)?,
            None => TextStore::open_read_only(&source.join("fulltext.dat"))?,
        };
        drop(read);
        Ok(Self {
            metadata,
            edges,
            control,
            namespaces,
            vectors,
            text,
            _meta_copy: meta_copy,
            _edges_copy: edges_copy,
            _source_lock: source_lock,
            _owner: owner,
        })
    }

    pub fn control(&self) -> &ControlState {
        &self.control
    }
    pub fn namespaces(&self) -> &[NamespaceConfig] {
        &self.namespaces
    }
    pub fn record_count(&self) -> Result<u64, StorageError> {
        Ok(self.metadata.begin_read()?.open_table(META_TABLE)?.len()?)
    }
    pub fn for_each_edge(
        &self,
        mut visit: impl FnMut(super::PersistedEdge) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let tx = self.edges.begin_read()?;
        for row in tx.open_table(super::edges::FORWARD_EDGES)?.iter()? {
            let (key, value) = row?;
            visit(super::PersistedEdge::from_forward(
                key.value(),
                value.value(),
            )?)?;
        }
        Ok(())
    }
    pub fn record(&self, id: MemoryId) -> Result<Option<OfflineRecord>, StorageError> {
        let tx = self.metadata.begin_read()?;
        let table = tx.open_table(META_TABLE)?;
        table
            .get(id.as_bytes().as_slice())?
            .map(|value| self.materialize(value.value()))
            .transpose()
    }
    pub fn for_each_record(
        &self,
        mut visit: impl FnMut(OfflineRecord) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let tx = self.metadata.begin_read()?;
        for row in tx.open_table(META_TABLE)?.iter()? {
            let (_, value) = row?;
            visit(self.materialize(value.value())?)?;
        }
        Ok(())
    }
    fn materialize(&self, bytes: &[u8]) -> Result<OfflineRecord, StorageError> {
        let record = DiskRecord::from_bytes(bytes)?;
        let full_text = if record.text_offset != 0 || record.text_length != 0 {
            Some(self.text.read(TextRef {
                file_offset: record.text_offset,
                length: record.text_length,
            })?)
        } else {
            None
        };
        let embedding = if record.phase == DecayPhase::Tombstone {
            None
        } else {
            Some(
                self.vectors
                    .get(&record.namespace_id)
                    .ok_or(StorageError::NamespaceNotFound(record.namespace_id))?
                    .get_vector(record.vector_slot)
                    .ok_or_else(|| {
                        StorageError::OwnershipMismatch("missing snapshot vector".into())
                    })?
                    .to_vec(),
            )
        };
        Ok(OfflineRecord {
            record,
            full_text,
            embedding,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema_migration::{MigrationOptions, inspect_schema, migrate_schema};
    use crate::storage::{RedbStorageEngine, StorageEngine};
    use crate::test_support::Fixture;

    #[tokio::test]
    async fn offline_snapshot_reads_canonical_v2_without_serving_or_source_changes() {
        let fixture = Fixture::new();
        let id = fixture.insert_memory().await;
        fixture.storage.write().unwrap().sync().unwrap();
        let source = fixture.close();
        let target_parent = tempfile::tempdir().unwrap();
        let target = target_parent.path().join("offline");
        let expected_source = inspect_schema(source.path())
            .unwrap()
            .control
            .generation_ref();
        migrate_schema(
            source.path(),
            &target,
            &MigrationOptions {
                migration_now: 100,
                expected_source,
                attestations: vec![],
            },
        )
        .unwrap();
        let files = super::super::validation::canonical_files(&target).unwrap();
        let before: Vec<_> = files
            .iter()
            .map(std::fs::read)
            .collect::<Result<_, _>>()
            .unwrap();
        let view = OfflineSnapshot::open(&target).unwrap();
        assert_eq!(view.control().generation_ref(), expected_source);
        assert_eq!(view.namespaces().len(), 1);
        assert_eq!(view.record_count().unwrap(), 1);
        let item = view.record(id).unwrap().unwrap();
        assert_eq!(item.record.version, 2);
        assert!(item.record.schema.is_some());
        assert_eq!(item.embedding.unwrap(), vec![0.; 4]);
        let mut count = 0;
        view.for_each_record(|item| {
            assert_eq!(item.record.id, *id.as_bytes());
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            OfflineSnapshot::open(&target),
            Err(StorageError::DatabaseLocked)
        ));
        assert!(RedbStorageEngine::open(&target).is_err());
        drop(view);
        assert!(RedbStorageEngine::open(&target).is_err());
        assert_eq!(
            files
                .iter()
                .map(std::fs::read)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            before
        );
    }
}
