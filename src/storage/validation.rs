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
pub(crate) fn inspect_database(
    path: &Path,
) -> Result<(tempfile::TempDir, redb::Database), StorageError> {
    inspect_database_copy(path, false)
}

pub(crate) fn inspect_database_copy(
    path: &Path,
    recover_copy: bool,
) -> Result<(tempfile::TempDir, redb::Database), StorageError> {
    require_file(path)?;
    let temp = tempfile::tempdir()?;
    let copy = temp.path().join("inspect.db");
    std::fs::copy(path, &copy)?;
    let mut builder = redb::Builder::new();
    if !recover_copy {
        builder.set_repair_callback(|session| session.abort());
    }
    let mut db = builder.open(copy)?;
    if !db.check_integrity()? {
        return Err(StorageError::RecoveryRequired(format!(
            "database integrity failure: {}",
            path.display()
        )));
    }
    Ok((temp, db))
}

/// Canonical text preflight on a disposable metadata copy. This precedes any
/// writable canonical open; pending graph effects are deliberately not inspected.
pub(crate) fn preflight_text(path: &Path) -> Result<(), StorageError> {
    // An unclean process exit may require redb free-list recovery. Perform it
    // only on the disposable copy before touching the original database.
    let (_copy, metadata) = inspect_database_copy(&path.join("meta.db"), true)?;
    let read = metadata.begin_read()?;
    if super::control::validate_existing(&read)?.is_some_and(|c| c.control_schema_version == 2) {
        return Err(StorageError::RecoveryRequired("schema v2 remains offline until participating writers are enabled; use schema inspect/export".into()));
    }
    let text = selected_text(path, &read)?;
    let records = read.open_table(META_TABLE)?;
    for row in records.iter()? {
        let (_, bytes) = row?;
        let record = DiskRecord::from_bytes(bytes.value())?;
        if record.text_offset != 0 || record.text_length != 0 {
            text.read(TextRef {
                file_offset: record.text_offset,
                length: record.text_length,
            })?;
        }
    }
    Ok(())
}

fn selected_text(path: &Path, read: &redb::ReadTransaction) -> Result<TextStore, StorageError> {
    match super::metadata::text_generation::read_selection(read)? {
        Some(descriptor) => {
            super::text::generation::validate_rejection_gate(path, descriptor.store_id)?;
            TextStore::open_generation(path, &descriptor, false)
        }
        None => TextStore::open_read_only(&path.join("fulltext.dat")),
    }
}

/// Caller owns/quiesces the store, or supplies an inaccessible staging copy.
/// Returns the required canonical inventory derived from validated metadata.
pub(crate) fn validate_existing(path: &Path) -> Result<HashSet<String>, StorageError> {
    validate_snapshot(path, false)
}

/// Only the logical importer may inspect its unpublished, explicitly importing
/// stage. Ordinary inspection/export/backup must not treat partial imports as snapshots.
pub(crate) fn validate_import_stage(path: &Path) -> Result<HashSet<String>, StorageError> {
    validate_snapshot(path, true)
}

fn validate_snapshot(path: &Path, importing: bool) -> Result<HashSet<String>, StorageError> {
    if !importing && std::fs::symlink_metadata(path.join(".recalld-importing")).is_ok() {
        return Err(StorageError::RecoveryRequired(
            "incomplete logical import; resume using its original export".into(),
        ));
    }
    recover_text_compaction(path)?;
    for name in ["meta.db", "edges.db", "fulltext.dat"] {
        require_file(&path.join(name))?;
    }
    let (_meta_copy, meta) = inspect_database(&path.join("meta.db"))?;
    let read = meta.begin_read()?;
    let control = super::control::validate_existing(&read)?;
    let schema_state = control
        .as_ref()
        .map(|c| super::canonical_schema::read_state(&read, c))
        .transpose()?
        .flatten();
    if control.is_some() {
        read.open_table(super::metadata::NAMESPACE_COUNTER)?;
    }
    let namespaces = read.open_table(NAMESPACE_TABLE)?;
    let mut inventory: HashSet<String> = ["meta.db", "edges.db", "fulltext.dat"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut vectors = HashMap::new();
    let mut configs = HashMap::new();
    let mut names = HashSet::new();
    for item in namespaces.iter()? {
        let (key, value) = item?;
        let config = NamespaceConfig::decode(value.value())?;
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
        if schema_state.is_some() != config.schema.is_some() {
            return Err(StorageError::InvalidControl(
                "mixed namespace schema".into(),
            ));
        }
        configs.insert(config.id.get(), config.clone());
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
    if let Some(descriptor) = super::metadata::text_generation::read_selection(&read)? {
        inventory.insert(descriptor.filename);
    }
    let text = selected_text(path, &read)?;
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
        let namespace = configs
            .get(&record.namespace_id)
            .ok_or(StorageError::NamespaceNotFound(record.namespace_id))?;
        super::canonical_schema::validate_record_namespace(
            &record,
            namespace,
            schema_state.as_ref(),
        )?;
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
        if let Some(reference) = record
            .schema
            .as_ref()
            .and_then(|schema| schema.vector.as_ref())
        {
            use sha2::{Digest, Sha256};
            let vector = store
                .get_vector(record.vector_slot)
                .ok_or_else(|| StorageError::OwnershipMismatch("missing v2 vector".into()))?;
            let mut hash = Sha256::new();
            for value in vector {
                hash.update(value.to_le_bytes());
            }
            if <[u8; 32]>::from(hash.finalize()) != reference.sha256 {
                return Err(StorageError::OwnershipMismatch(
                    "v2 vector hash mismatch".into(),
                ));
            }
            if matches!(
                &namespace.schema.as_ref().unwrap().embedding_binding,
                crate::model::schema::EmbeddingBinding::Bound { .. }
            ) {
                super::canonical_schema::validate_bound_vector(vector)?;
            }
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
