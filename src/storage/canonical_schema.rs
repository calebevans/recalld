//! Offline schema-v2 metadata. Consumers must opt in only after all writers participate.
use super::{
    StorageError,
    control::{ControlState, Generation, GenerationRef},
};
use redb::{
    MultimapTableHandle, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const SCHEMA: TableDefinition<&str, &[u8]> = TableDefinition::new("canonical_schema");
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServingMode {
    Offline,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalSchemaState {
    pub canonical_schema_version: u32,
    pub migration_id: Uuid,
    pub receipt_operation_id: Option<Uuid>,
    pub migration_now: i64,
    pub source: GenerationRef,
    /// Generation of this publication in source.store_id, retained across forks.
    pub publication_generation: Generation,
    pub serving_mode: ServingMode,
}

pub(crate) fn read_state(
    read: &redb::ReadTransaction,
    control: &ControlState,
) -> Result<Option<CanonicalSchemaState>, StorageError> {
    if read
        .list_multimap_tables()?
        .any(|t| t.name() == SCHEMA.name())
    {
        return Err(StorageError::InvalidControl(
            "canonical schema is a multimap".into(),
        ));
    }
    let present = read.list_tables()?.any(|t| t.name() == SCHEMA.name());
    if !present {
        if control.control_schema_version != 1 {
            return Err(StorageError::InvalidControl(
                "canonical schema marker missing".into(),
            ));
        }
        return Ok(None);
    }
    let table = read.open_table(SCHEMA)?;
    if table.len()? != 1 {
        return Err(StorageError::InvalidControl(
            "invalid canonical schema singleton".into(),
        ));
    }
    let bytes = table
        .get("state")?
        .ok_or_else(|| StorageError::InvalidControl("canonical schema state missing".into()))?;
    let state: CanonicalSchemaState = super::control::decode(bytes.value())?;
    if control.control_schema_version != 2
        || state.canonical_schema_version != 2
        || state.migration_id.is_nil()
        || !super::control::permits_generation_origin(read, control, state.source)?
        || !super::control::permits_generation_origin(
            read,
            control,
            GenerationRef {
                store_id: state.source.store_id,
                generation: state.publication_generation,
            },
        )?
        || (state.source.store_id == control.store_id
            && state.publication_generation != control.committed_generation)
        || chrono::DateTime::from_timestamp_millis(state.migration_now).is_none()
    {
        return Err(StorageError::InvalidControl(
            "unsupported or inconsistent canonical schema".into(),
        ));
    }
    if let Some(operation_id) = state.receipt_operation_id {
        let key = super::control::scoped_key(
            state.source.store_id,
            super::control::MutationScope::Catalog,
            operation_id.as_bytes(),
        );
        let receipts = read.open_table(super::control::RECEIPTS)?;
        let receipt = receipts.get(key.as_slice())?.ok_or_else(|| {
            StorageError::InvalidControl("schema migration receipt missing".into())
        })?;
        let receipt = super::control::validate_receipt(receipt.value())?.receipt;
        if receipt.operation_id != operation_id
            || receipt.store_id != state.source.store_id
            || receipt.committed_generation != state.source.generation.next()?
            || receipt.committed_generation != state.publication_generation
            || !super::control::permits_generation_origin(
                read,
                control,
                GenerationRef {
                    store_id: receipt.store_id,
                    generation: receipt.committed_generation,
                },
            )?
            || (state.source.store_id == control.store_id
                && receipt.committed_generation != control.committed_generation)
            || !matches!(receipt.outcome, super::control::MutationOutcome::SchemaMigrated { migration_id, attested_namespaces, .. } if migration_id == state.migration_id && attested_namespaces > 0)
        {
            return Err(StorageError::InvalidControl(
                "schema migration receipt mismatch".into(),
            ));
        }
    } else if state.source.generation != state.publication_generation {
        // There are no ordinary offline-v2 writers. A format-only migration
        // cannot explain an increment; an attestation must retain its evidence,
        // including after a restore fork changes the active store ID/counter.
        return Err(StorageError::InvalidControl(
            "format-only migration changed generation without an attestation receipt".into(),
        ));
    }
    Ok(Some(state))
}

/// Existing vector bytes must satisfy the declared convention; never normalize
/// legacy bytes while attesting their provenance. f64 accumulation avoids f32
/// overflow and admits normal f32 normalization rounding, not arbitrary scaling.
pub(crate) fn validate_bound_vector(vector: &[f32]) -> Result<(), StorageError> {
    let norm_squared: f64 = vector.iter().map(|v| f64::from(*v).powi(2)).sum();
    if vector.iter().any(|v| !v.is_finite())
        || !norm_squared.is_finite()
        || (norm_squared - 1.0).abs() > 1e-5
    {
        return Err(StorageError::InvalidMutation(
            "attested vector is not finite unit-normalized; re-embedding is required".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_record_namespace(
    record: &crate::model::DiskRecord,
    namespace: &crate::model::NamespaceConfig,
    state: Option<&CanonicalSchemaState>,
) -> Result<(), StorageError> {
    match (state, &record.schema, &namespace.schema) {
        (None, None, None) if record.version == 1 => Ok(()),
        (Some(_), Some(record_schema), Some(namespace_schema)) if record.version == 2 => {
            namespace_schema
                .validate(namespace)
                .map_err(StorageError::InvalidControl)?;
            record_schema
                .validate(record)
                .map_err(StorageError::InvalidControl)?;
            if record_schema.lifecycle.lifecycle_version != namespace_schema.lifecycle_version
                || record_schema.lifecycle.policy_revision > namespace_schema.policy_revision
            {
                return Err(StorageError::InvalidControl(
                    "record/namespace policy mismatch".into(),
                ));
            }
            Ok(())
        }
        _ => Err(StorageError::InvalidControl(
            "mixed canonical record/namespace schema".into(),
        )),
    }
}
