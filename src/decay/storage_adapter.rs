//! Adapter layer between the sweep runner and the storage engine.
//!
//! Provides sweep-specific helper functions that extract decay-relevant
//! data from the real `StorageEngine` trait. These functions bridge the
//! gap between the sweep's needs (e.g. `DecayMetadata`) and the storage
//! engine's interface (e.g. `get_record` returning a full `DiskRecord`).

use crate::model::{DecayPhase, MemoryId};
use crate::storage::StorageEngine;
use super::sweep::{DecayMetadata, SweepError};

/// Extract decay-relevant metadata from a `DiskRecord`.
pub fn get_decay_metadata(
    storage: &dyn StorageEngine,
    id: MemoryId,
) -> Result<Option<DecayMetadata>, SweepError> {
    match storage.get_record(id) {
        Ok(Some(record)) => Ok(Some(DecayMetadata {
            stability: record.stability,
            last_accessed_at: record.last_accessed_at,
            is_permastore: record.is_permastore != 0,
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(SweepError::Storage(e.to_string())),
    }
}

/// Check whether a memory has a non-empty summary.
pub fn has_summary(
    storage: &dyn StorageEngine,
    id: MemoryId,
) -> Result<bool, SweepError> {
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
    stability: f32,
    is_permastore: bool,
) -> Result<(), SweepError> {
    storage
        .update_decay_state(id, phase, strength, stability, is_permastore)
        .map_err(|e| SweepError::Storage(e.to_string()))
}
