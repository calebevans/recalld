//! Versioned restore provenance. Origin generations are compared only within lineage.
use super::{
    StorageError,
    control::{ControlState, GenerationRef},
    metadata::removal_ledger::LedgerState,
};
use redb::{
    MultimapTableHandle, ReadTransaction, ReadableTableMetadata, TableDefinition, TableHandle,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const LINEAGE: TableDefinition<&str, &[u8]> = TableDefinition::new("restore_lineage_v1");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestoreMode {
    Serving,
    Historical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LedgerSource {
    pub source: GenerationRef,
    pub coverage: LedgerState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LineageState {
    pub version: u32,
    pub active_store_id: Uuid,
    pub operation_id: Uuid,
    pub mode: RestoreMode,
    pub source_snapshot: GenerationRef,
    pub captured_ledger_source: Option<GenerationRef>,
    pub archive_sha256: [u8; 32],
    pub ancestors: Vec<GenerationRef>,
    pub ledger_sources: Vec<LedgerSource>,
}

impl LineageState {
    pub(crate) fn permits(&self, origin: GenerationRef) -> bool {
        self.ancestors
            .iter()
            .any(|a| a.store_id == origin.store_id && a.generation >= origin.generation)
    }
    pub(crate) fn validate(&self, control: &ControlState) -> Result<(), StorageError> {
        let mut ids = std::collections::HashSet::new();
        if self.version != 1
            || self.active_store_id != control.store_id
            || self.operation_id.is_nil()
            || (self.mode == RestoreMode::Serving) != self.captured_ledger_source.is_some()
            || self.ancestors.is_empty()
            || self.ancestors.iter().any(|a| {
                a.store_id.is_nil() || a.store_id == control.store_id || !ids.insert(a.store_id)
            })
            || !self.permits(self.source_snapshot)
            || self
                .captured_ledger_source
                .is_some_and(|s| !self.permits(s))
            || self.ledger_sources.is_empty()
            || self.ledger_sources.iter().any(|s| {
                !self.permits(s.source)
                    || s.coverage.ledger_version != 1
                    || !self.permits(s.coverage.tracking_started_at)
            })
        {
            return Err(StorageError::InvalidControl(
                "invalid restore lineage/provenance".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn read_lineage(
    tx: &ReadTransaction,
    control: &ControlState,
) -> Result<Option<LineageState>, StorageError> {
    let present = tx.list_tables()?.any(|t| t.name() == LINEAGE.name());
    let wrong = tx
        .list_multimap_tables()?
        .any(|t| t.name() == LINEAGE.name());
    if !present && !wrong && control.restore_lineage_version.is_none() {
        return Ok(None);
    }
    if wrong || !present || control.restore_lineage_version != Some(1) {
        return Err(StorageError::InvalidControl(
            "missing/unmarked/unsupported restore lineage".into(),
        ));
    }
    let table = tx.open_table(LINEAGE)?;
    if table.len()? != 1 {
        return Err(StorageError::InvalidControl(
            "invalid lineage row count".into(),
        ));
    }
    let value = table
        .get("state")?
        .ok_or_else(|| StorageError::InvalidControl("missing lineage state".into()))?;
    let state: LineageState = serde_json::from_slice(value.value())
        .map_err(|e| StorageError::InvalidControl(e.to_string()))?;
    state.validate(control)?;
    Ok(Some(state))
}

/// Schema/export consumers use this to validate preserved historical provenance.
pub(crate) fn permits_origin(
    tx: &ReadTransaction,
    control: &ControlState,
    origin: GenerationRef,
) -> Result<bool, StorageError> {
    let lineage = read_lineage(tx, control)?;
    Ok(if origin.store_id == control.store_id {
        origin.generation <= control.committed_generation
    } else {
        lineage.is_some_and(|s| s.permits(origin))
    })
}

pub(crate) fn ensure_serving(
    tx: &ReadTransaction,
    control: &ControlState,
) -> Result<(), StorageError> {
    if read_lineage(tx, control)?.is_some_and(|s| s.mode == RestoreMode::Historical) {
        return Err(StorageError::RecoveryRequired(
            "historical restore is read-only and excluded from ordinary serving".into(),
        ));
    }
    Ok(())
}
