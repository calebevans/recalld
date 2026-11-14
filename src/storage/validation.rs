//! Strict legacy-store inspection. Never initializes missing canonical files.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use redb::{ReadableTable, ReadableTableMetadata};

use super::StorageError;
use super::edges::{FORWARD_EDGES, PersistedEdge, REVERSE_EDGES};
use super::metadata::{META_TABLE, NAMESPACE_TABLE};
use super::text::{TextRef, TextStore, recover_text_compaction};
use super::vectors::VectorStore;
use crate::model::validation::validate_namespace_name;
use crate::model::{DecayPhase, DiskRecord, MemoryId, NamespaceConfig};

pub(crate) fn require_file(path: &Path) -> Result<(), StorageError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(StorageError::RecoveryRequired(format!(
            "missing, empty or nonregular canonical file {}",
            path.display()
        )));
    }
    Ok(())
}

// redb 2.x writes bookkeeping even on open. Inspect disposable copies and
// abort repair; source files remain byte-for-byte unchanged.
fn inspect_database(path: &Path) -> Result<(tempfile::TempDir, redb::Database), StorageError> {
    require_file(path)?;
    let temp = tempfile::tempdir()?;
    let copy = temp.path().join("inspect.db");
    std::fs::copy(path, &copy)?;
    let mut builder = redb::Builder::new();
    builder.set_repair_callback(|session| session.abort());
    let mut db = builder.open(copy)?;
    if !db.check_integrity()? {
        return Err(StorageError::RecoveryRequired(format!(
            "database integrity failure: {}",
            path.display()
        )));
    }
    Ok((temp, db))
}

/// Caller owns/quiesces the store, or supplies an inaccessible staging copy.
/// Returns the required canonical inventory derived from validated metadata.
pub(crate) fn validate_existing(path: &Path) -> Result<HashSet<String>, StorageError> {
    recover_text_compaction(path)?;
    for name in ["meta.db", "edges.db", "fulltext.dat"] {
        require_file(&path.join(name))?;
    }
    let (_meta_copy, meta) = inspect_database(&path.join("meta.db"))?;
    let read = meta.begin_read()?;
    let control = super::control::validate_existing(&read)?;
    super::metadata::removal_ledger::read_export(&read, control.as_ref())?;
    if control.is_some() {
        read.open_table(super::metadata::NAMESPACE_COUNTER)?;
    }
    let namespaces = read.open_table(NAMESPACE_TABLE)?;
    let mut inventory: HashSet<String> = ["meta.db", "edges.db", "fulltext.dat"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut vectors = HashMap::new();
    let mut names = HashSet::new();
    for item in namespaces.iter()? {
        let (key, value) = item?;
        let config: NamespaceConfig = serde_json::from_slice(value.value())
            .map_err(|e| StorageError::Deserialize(e.to_string()))?;
        validate_namespace_name(&config.name).map_err(|reason| {
            StorageError::InvalidNamespaceName {
                name: config.name.clone(),
                reason: reason.to_string(),
            }
        })?;
        if key.value() != config.id.get()
            || !names.insert(config.name.to_ascii_lowercase())
            || !(1..=8192).contains(&config.embedding_dim)
        {
            return Err(StorageError::InvalidMutation(
                "invalid namespace identity/dimension".into(),
            ));
        }
        let dir = path.join(&config.name);
        if !std::fs::symlink_metadata(&dir)?.is_dir() {
            return Err(StorageError::InvalidPath);
        }
        require_file(&dir.join("vectors.dat"))?;
        inventory.insert(format!("{}/vectors.dat", config.name));
        // VectorStore opens an existing, nonempty file without modifying it.
        vectors.insert(
            config.id.get(),
            VectorStore::open(&dir, config.embedding_dim as usize)?,
        );
    }
    let text = TextStore::open_read_only(&path.join("fulltext.dat"))?;
    let records = read.open_table(META_TABLE)?;
    let mut ids = HashSet::new();
    let mut owners = HashSet::new();
    for item in records.iter()? {
        let (key, value) = item?;
        let record = DiskRecord::from_bytes(value.value())?;
        if key.value() != record.id {
            return Err(StorageError::InvalidMutation(
                "record key/ID mismatch".into(),
            ));
        }
        let id = MemoryId::from_bytes(record.id);
        ids.insert(id);
        let store = vectors
            .get(&record.namespace_id)
            .ok_or(StorageError::NamespaceNotFound(record.namespace_id))?;
        if record.phase != DecayPhase::Tombstone
            && (!owners.insert((record.namespace_id, record.vector_slot))
                || store.is_free(record.vector_slot)
                || store
                    .get_vector(record.vector_slot)
                    .is_none_or(|v| v.iter().any(|f| !f.is_finite())))
        {
            return Err(StorageError::OwnershipMismatch(format!(
                "invalid vector owner {id}"
            )));
        }
        if record.text_offset != 0 || record.text_length != 0 {
            text.read(TextRef {
                file_offset: record.text_offset,
                length: record.text_length,
            })?;
        }
    }
    let (_edges_copy, edges) = inspect_database(&path.join("edges.db"))?;
    let read = edges.begin_read()?;
    let forward = read.open_table(FORWARD_EDGES)?;
    let reverse = read.open_table(REVERSE_EDGES)?;
    if forward.len()? != reverse.len()? {
        return Err(StorageError::CorruptIndex(
            "edge index counts disagree".into(),
        ));
    }
    for item in forward.iter()? {
        let (key, value) = item?;
        let edge = PersistedEdge::from_forward(key.value(), value.value())?;
        let mut reverse_key = [0u8; 33];
        reverse_key[..16].copy_from_slice(edge.target.as_bytes());
        reverse_key[16] = edge.edge_type.as_u8();
        reverse_key[17..].copy_from_slice(edge.source.as_bytes());
        if !ids.contains(&edge.source)
            || !ids.contains(&edge.target)
            || reverse.get(reverse_key.as_slice())?.is_none()
        {
            return Err(StorageError::CorruptIndex(
                "dangling or inconsistent edge".into(),
            ));
        }
    }
    Ok(inventory)
}

pub(crate) fn canonical_files(path: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let mut files: Vec<_> = validate_existing(path)?
        .into_iter()
        .map(|name| path.join(name))
        .collect();
    files.sort();
    Ok(files)
}
