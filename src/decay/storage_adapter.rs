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
/// State, history and an unkeyed receipt commit together; cache invalidation
/// finishes before this adapter acknowledges success.
/// Lock order: storage only here. Do not acquire/await graph while holding this
/// guard: sweep publication holds graph first, then acquires storage.
pub async fn reinforce(
    storage: std::sync::Arc<std::sync::RwLock<crate::storage::RedbStorageEngine>>,
    cache: std::sync::Arc<crate::cache::CacheManager>,
    id: MemoryId,
    quality: u8,
    global_multiplier: f64,
    now_millis: i64,
) -> Result<super::lifecycle::FeedbackPlan, super::lifecycle::LifecycleError> {
    use super::lifecycle::{LifecycleError, plan_feedback, resolve_policy};
    tokio::task::spawn_blocking(move || {
        let storage = storage
            .write()
            .map_err(|e| LifecycleError::Storage(e.to_string()))?;
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
        plan_feedback(&record, &policy, now_millis, quality)?;
        let receipt = storage
            .record_legacy_feedback(
                &crate::storage::control::MutationContext::unkeyed(namespace.id),
                id,
                quality,
                now_millis,
                global_multiplier,
            )
            .map_err(|e| super::observation::ObservationError::storage(e, vec![]))?;
        super::observation::publish(&storage, std::slice::from_ref(&receipt), || {
            tokio::runtime::Handle::current().block_on(cache.invalidate(id));
        })?;
        // Return canonical values, not the preflight plan: the metadata
        // transaction is the authority even if an internal writer bypassed
        // the outer storage lock. No historical response payload is retained.
        let current = storage
            .get_record(id)
            .map_err(|e| super::observation::ObservationError::storage(e, vec![receipt.clone()]))?
            .ok_or_else(|| {
                super::observation::ObservationError::storage(
                    crate::storage::StorageError::NotFound(id.into_inner()),
                    vec![receipt.clone()],
                )
            })?;
        Ok(super::lifecycle::FeedbackPlan {
            timestamp: current.last_accessed_at,
            stability: current.stability,
            strength: current.strength,
            phase: current.phase,
            is_permastore: current.is_permastore != 0,
        })
    })
    .await
    .map_err(|e| {
        LifecycleError::Observation(super::observation::ObservationError::worker_failed(e))
    })?
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
