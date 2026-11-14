//! Adapter layer between the sweep runner and the storage engine.
//!
//! Provides sweep-specific helper functions that extract decay-relevant
//! data from the real `StorageEngine` trait. These functions bridge the
//! gap between the sweep's needs (e.g. `DecayMetadata`) and the storage
//! engine's interface (e.g. `get_record` returning a full `DiskRecord`).

use super::sweep::{DecayMetadata, SweepError};
use crate::model::{DecayPhase, MemoryId, NamespaceId};
use crate::storage::StorageEngine;

/// Serialize current-layout feedback with other users of the storage lock.
/// State and history commit together; public receipts and retry safety remain
/// outside this legacy compatibility adapter.
/// Lock order: storage only here. Do not acquire/await graph while holding this
/// guard: sweep publication holds graph first, then acquires storage.
pub async fn reinforce(
    storage: std::sync::Arc<std::sync::RwLock<crate::storage::RedbStorageEngine>>,
    id: MemoryId,
    quality: u8,
    global_multiplier: f64,
    now_millis: i64,
    expected_namespace: Option<String>,
) -> Result<super::lifecycle::FeedbackPlan, super::lifecycle::LifecycleError> {
    use super::lifecycle::{LifecycleError, plan_feedback, resolve_policy};
    tokio::task::spawn_blocking(move || {
        let storage = storage
            .write()
            .map_err(|e| LifecycleError::Storage(e.to_string()))?;
        if let Some(namespace) = expected_namespace.as_deref() {
            storage
                .assert_memory_namespace(id, namespace)
                .map_err(|error| match error {
                    crate::storage::StorageError::NamespaceMismatch { .. } => {
                        LifecycleError::NamespaceMismatch(error.to_string())
                    }
                    crate::storage::StorageError::NotFound(_) => LifecycleError::NotFound,
                    crate::storage::StorageError::InvalidNamespaceName { .. } => {
                        LifecycleError::InvalidInput("invalid namespace assertion")
                    }
                    _ => LifecycleError::Storage(error.to_string()),
                })?;
        }
        let record = storage
            .get_record(id)
            .map_err(|e| LifecycleError::Storage(e.to_string()))?
            .ok_or(LifecycleError::NotFound)?;
        let namespace = storage
            .get_namespace(NamespaceId::new(record.namespace_id))
            .map_err(|e| LifecycleError::Storage(e.to_string()))?
            .ok_or(LifecycleError::NotFound)?;
        let policy = resolve_policy(
            &namespace,
            &super::DecayConfig::default(),
            global_multiplier,
        )?;
        let plan = plan_feedback(&record, &policy, now_millis, quality)?;
        if !storage
            .apply_legacy_feedback(id, &record, &namespace, &plan)
            .map_err(|e| LifecycleError::Storage(e.to_string()))?
        {
            return Err(LifecycleError::Storage(
                "lifecycle snapshot changed before feedback commit".into(),
            ));
        }
        Ok(plan)
    })
    .await
    .map_err(|e| LifecycleError::Storage(e.to_string()))?
}

/// Extract decay-relevant metadata from a `DiskRecord`, including the
/// namespace's decay rate multiplier (if set).
pub fn get_decay_metadata(
    storage: &dyn StorageEngine,
    id: MemoryId,
) -> Result<Option<DecayMetadata>, SweepError> {
    match storage.get_record(id) {
        Ok(Some(record)) => {
            if record.phase == DecayPhase::Tombstone {
                return Ok(None);
            }
            let namespace = storage
                .get_namespace(NamespaceId::new(record.namespace_id))
                .map_err(|e| SweepError::Storage(e.to_string()))?
                .ok_or_else(|| SweepError::Storage("namespace policy not found".to_string()))?;

            Ok(Some(DecayMetadata {
                phase: record.phase,
                created_at: record.created_at,
                stability: record.stability,
                last_accessed_at: record.last_accessed_at,
                is_permastore: record.is_permastore != 0,
                namespace,
                has_summary: !record.summary.is_empty(),
            }))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(SweepError::Storage(e.to_string())),
    }
}

/// Check whether a memory has a non-empty summary.
pub fn has_summary(storage: &dyn StorageEngine, id: MemoryId) -> Result<bool, SweepError> {
    match storage.get_record(id) {
        Ok(Some(record)) => Ok(!record.summary.is_empty()),
        Ok(None) => Err(SweepError::MemoryNotFound(id)),
        Err(e) => Err(SweepError::Storage(e.to_string())),
    }
}

/// Fallback phase scan returning only MemoryIds.
pub fn scan_phase_ids(
    storage: &dyn StorageEngine,
    phase: DecayPhase,
) -> Result<Vec<MemoryId>, SweepError> {
    storage
        .scan_phase_records(phase)
        .map(|v| v.into_iter().map(|(id, _)| id).collect())
        .map_err(|e| SweepError::Storage(e.to_string()))
}

/// Wrap `ids_in_phase` with `SweepError`.
pub fn ids_in_phase(
    storage: &dyn StorageEngine,
    phase: DecayPhase,
) -> Result<Vec<MemoryId>, SweepError> {
    storage
        .ids_in_phase(phase)
        .map_err(|e| SweepError::Storage(e.to_string()))
}

/// Update decay state with `SweepError` wrapping.
pub fn update_decay_state(
    storage: &dyn StorageEngine,
    id: MemoryId,
    phase: DecayPhase,
    strength: f32,
    decay_strength: f32,
    stability: f32,
    is_permastore: bool,
) -> Result<(), SweepError> {
    storage
        .update_decay_state(
            id,
            phase,
            strength,
            decay_strength,
            stability,
            is_permastore,
        )
        .map_err(|e| SweepError::Storage(e.to_string()))
}
