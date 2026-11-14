//! Versioned canonical backup description. Callers hold source ownership or
//! inspect private restore staging; inspection never opens canonical redb writable.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use redb::{ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::manifest::BackupManifest;
use crate::model::{DecayPhase, DiskRecord, NamespaceConfig};
use crate::storage::{StorageError, canonical_schema, control, metadata, validation};

/// Credential-free description of the validated snapshot before any restore fork.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupManifestV2 {
    pub backend: String,
    pub configuration_schema_version: u32,
    pub control: control::ControlState,
    pub schema: canonical_schema::CanonicalSchemaState,
    pub namespaces: Vec<NamespaceConfig>,
    /// Historical ancestry is copied from the shared validated lineage table.
    /// Its version and origin bounds remain the lineage module's responsibility.
    pub restore_ancestry: Option<serde_json::Value>,
    pub(crate) selected_text_generation: metadata::text_generation::TextGeneration,
    pub counts: LogicalCounts,
    pub derived_artifacts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogicalCounts {
    pub records: u64,
    pub tombstones: u64,
    pub vectors: u64,
    pub edges: u64,
    pub receipts: u64,
    /// Per namespace, Full/Summary/Ghost/Tombstone in that fixed order.
    pub records_by_namespace_phase: BTreeMap<u32, [u64; 4]>,
    /// Persisted EdgeType discriminants, including zero counts for known types.
    pub edges_by_type: BTreeMap<u8, u64>,
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidControl(format!("backup manifest: {message}"))
}

pub(crate) fn sha256_file(path: &Path) -> Result<[u8; 32], std::io::Error> {
    let mut input = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buf)?;
        if count == 0 {
            break;
        }
        hash.update(&buf[..count]);
    }
    Ok(hash.finalize().into())
}

/// Called only after common complete semantic validation, with ownership held.
pub(crate) fn capture(path: &Path) -> Result<Option<BackupManifestV2>, StorageError> {
    let (_copy, metadata) = validation::inspect_database(&path.join("meta.db"))?;
    let read = metadata.begin_read()?;
    let Some(control) = control::validate_existing(&read)? else {
        return Ok(None);
    };
    let Some(schema) = canonical_schema::read_state(&read, &control)? else {
        return Ok(None);
    };
    let mut namespaces = Vec::new();
    let mut counts = LogicalCounts::default();
    for row in read.open_table(metadata::NAMESPACE_TABLE)?.iter()? {
        let (_, value) = row?;
        let namespace = NamespaceConfig::decode(value.value())?;
        counts
            .records_by_namespace_phase
            .insert(namespace.id.get(), [0; 4]);
        namespaces.push(namespace);
    }
    for row in read.open_table(metadata::META_TABLE)?.iter()? {
        let (_, value) = row?;
        let record = DiskRecord::from_bytes(value.value())?;
        counts.records += 1;
        counts
            .records_by_namespace_phase
            .get_mut(&record.namespace_id)
            .ok_or_else(|| invalid("record references missing namespace"))?
            [record.phase as usize - 1] += 1;
        if record.phase == DecayPhase::Tombstone {
            counts.tombstones += 1;
        } else {
            counts.vectors += 1;
        }
    }
    counts.receipts = read.open_table(control::RECEIPTS)?.len()?;
    let selected_text_generation = metadata::text_generation::read_selection(&read)?
        .ok_or_else(|| invalid("v2 snapshot lacks selected text generation"))?;
    let ancestry_table: TableDefinition<&str, &[u8]> = TableDefinition::new("restore_lineage_v1");
    let restore_ancestry = if read
        .list_tables()?
        .any(|t| t.name() == ancestry_table.name())
    {
        let table = read.open_table(ancestry_table)?;
        if table.len()? != 1 {
            return Err(invalid("invalid lineage singleton"));
        }
        let value = table
            .get("state")?
            .ok_or_else(|| invalid("missing lineage state"))?;
        Some(control::decode(value.value())?)
    } else {
        None
    };
    let (_copy, edges) = validation::inspect_database(&path.join("edges.db"))?;
    let edge_read = edges.begin_read()?;
    for kind in 1..=7 {
        counts.edges_by_type.insert(kind, 0);
    }
    for row in edge_read
        .open_table(crate::storage::edges::FORWARD_EDGES)?
        .iter()?
    {
        let (key, value) = row?;
        let edge = crate::storage::PersistedEdge::from_forward(key.value(), value.value())?;
        counts.edges += 1;
        *counts
            .edges_by_type
            .get_mut(&edge.edge_type.as_u8())
            .unwrap() += 1;
    }
    Ok(Some(BackupManifestV2 {
        backend: "redb-external-payloads-v1".into(),
        configuration_schema_version: 2,
        control,
        schema,
        namespaces,
        restore_ancestry,
        selected_text_generation,
        counts,
        derived_artifacts: [
            ("fts", "omitted_requires_rebuild"),
            ("vector_search", "omitted_requires_rebuild"),
            (
                "phase_tag_namespace_indexes",
                "embedded_untrusted_rebuild_before_serving",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect(),
    }))
}

/// Validate an extracted snapshot BEFORE deletion reconciliation or lineage fork.
/// V1 CRC archives receive the same complete canonical validation. No recovery,
/// create, provider calls, receipt rebinding, or derived-readiness claim occurs.
pub fn validate_manifest_snapshot(
    path: &Path,
    manifest: &BackupManifest,
) -> Result<(), StorageError> {
    manifest.validate_format().map_err(|e| invalid(&e))?;
    let canonical = validation::validate_existing(path)?;
    let mut declared = BTreeSet::new();
    let mut total = 0u64;
    for file in &manifest.files {
        // Match validated metadata-derived names before touching any supplied path.
        // The legacy optional FTS file is derived and never substitutes a canonical file.
        if !(canonical.contains(&file.name)
            || manifest.backup_format_version == 1 && file.name == "fts.db")
            || !declared.insert(file.name.clone())
        {
            return Err(invalid("unknown or duplicate file"));
        }
        let path = path.join(&file.name);
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() != file.size_bytes {
            return Err(invalid("file type/length mismatch"));
        }
        total = total
            .checked_add(file.size_bytes)
            .ok_or_else(|| invalid("size overflow"))?;
        if let Some(hash) = file.sha256
            && sha256_file(&path)? != hash
        {
            return Err(invalid("SHA-256 mismatch"));
        }
    }
    if total != manifest.total_size_bytes || canonical.iter().any(|name| !declared.contains(name)) {
        return Err(invalid("canonical inventory/total mismatch"));
    }
    if manifest.backup_format_version == 2 {
        let actual =
            capture(path)?.ok_or_else(|| invalid("v2 manifest describes a v1 snapshot"))?;
        if control::encode(&actual)? != control::encode(manifest.snapshot.as_ref().unwrap())? {
            return Err(invalid(
                "snapshot metadata/counts disagree with canonical files",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
