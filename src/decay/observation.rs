//! Awaited legacy consumers. Each namespace commits separately; errors retain
//! every completed receipt. No public CAS, keyed transport, or v2 policy here.
use std::sync::{Arc, RwLock};

use crate::cache::CacheManager;
use crate::model::{AccessKind, MemoryId, NamespaceId};
use crate::storage::control::{MutationContext, WriteReceipt};
use crate::storage::{RedbStorageEngine, StorageEngine, StorageError};

/// Payload-free failure, including preceding namespace commits. Never present
/// a failed multi-namespace observation as an ordinary safely retryable write.
#[derive(Debug, Clone, serde::Serialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("observation failed: {}", serde_json::to_string(self).unwrap_or_default())]
pub struct ObservationError {
    pub commit_state: &'static str,
    pub ready: bool,
    pub receipts: Vec<WriteReceipt>,
    pub unknown_operation_id: Option<uuid::Uuid>,
    pub message: String,
}

impl ObservationError {
    pub(super) fn worker_failed(error: impl std::fmt::Display) -> Self {
        Self {
            commit_state: "unknown",
            ready: false,
            receipts: vec![],
            unknown_operation_id: None,
            message: format!("observation worker failed; do not blindly retry: {error}"),
        }
    }

    fn failed(error: impl std::fmt::Display, receipts: Vec<WriteReceipt>) -> Self {
        Self {
            commit_state: if receipts.is_empty() {
                "notCommitted"
            } else {
                "committed"
            },
            ready: false,
            receipts,
            unknown_operation_id: None,
            message: error.to_string(),
        }
    }

    pub(super) fn storage(error: StorageError, receipts: Vec<WriteReceipt>) -> Self {
        let unknown = match &error {
            StorageError::CommitOutcomeUnknown { operation_id, .. } => Some(*operation_id),
            _ => None,
        };
        let mut result = Self::failed(error, receipts);
        if unknown.is_some() {
            result.commit_state = "unknown";
            result.unknown_operation_id = unknown;
        }
        result
    }
}

/// Invalidation has no fallible normal path. A panic is a publication failure:
/// fail closed and retain the committed receipts. This runs on the blocking
/// worker under storage.write, so cancellation cannot drop publication after
/// commit. No graph lock is acquired here (graph -> storage remains valid).
pub(super) fn publish(
    storage: &RedbStorageEngine,
    receipts: &[WriteReceipt],
    invalidate: impl FnOnce(),
) -> Result<(), ObservationError> {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(invalidate)).is_err() {
        storage.mark_unavailable();
        return Err(ObservationError::failed(
            "INDEX_NOT_READY: cache publication failed; reopen required",
            receipts.to_vec(),
        ));
    }
    Ok(())
}

pub async fn record_exposures(
    storage: Arc<RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    accesses: Vec<(MemoryId, AccessKind)>,
    sampled_at: i64,
) -> Result<Vec<WriteReceipt>, ObservationError> {
    if accesses.is_empty() {
        return Ok(Vec::new());
    }
    tokio::task::spawn_blocking(move || {
        let storage = storage
            .write()
            .map_err(|e| ObservationError::failed(e, vec![]))?;
        let result = record_locked(&storage, &accesses, sampled_at);
        let receipts = match &result {
            Ok(r) => r,
            Err(e) => &e.receipts,
        };
        let ids: Vec<_> = accesses.iter().map(|(id, _)| *id).collect();
        // Publish even preceding successful batches when a later batch fails.
        if let Err(mut publication) = publish(&storage, receipts, || {
            tokio::runtime::Handle::current().block_on(cache.batch_invalidate(&ids));
        }) {
            if let Err(prior) = &result
                && prior.commit_state == "unknown"
            {
                publication.commit_state = "unknown";
                publication.unknown_operation_id = prior.unknown_operation_id;
            }
            return Err(publication);
        }
        result
    })
    .await
    .map_err(ObservationError::worker_failed)?
}

fn record_locked(
    storage: &RedbStorageEngine,
    accesses: &[(MemoryId, AccessKind)],
    sampled_at: i64,
) -> Result<Vec<WriteReceipt>, ObservationError> {
    // First occurrence order within each namespace; namespace order ascending.
    // Direct exposure dominates an additional associative occurrence.
    let mut distinct: Vec<(MemoryId, AccessKind)> = Vec::new();
    let mut positions = std::collections::HashMap::<MemoryId, usize>::new();
    for &(id, kind) in accesses {
        if !matches!(
            kind,
            AccessKind::DirectRetrieval | AccessKind::AssociativeRetrieval
        ) {
            return Err(ObservationError::failed("invalid exposure kind", vec![]));
        }
        if let Some(&index) = positions.get(&id) {
            if kind == AccessKind::DirectRetrieval {
                distinct[index].1 = kind;
            }
        } else {
            positions.insert(id, distinct.len());
            distinct.push((id, kind));
        }
    }
    let mut groups = std::collections::BTreeMap::<u32, Vec<_>>::new();
    for (id, kind) in distinct {
        let record = storage
            .get_record(id)
            .map_err(|e| ObservationError::storage(e, vec![]))?
            .ok_or_else(|| ObservationError::failed(format!("memory {id} not found"), vec![]))?;
        groups
            .entry(record.namespace_id)
            .or_default()
            .push((id, kind));
    }
    let mut receipts = Vec::new();
    for (namespace, batch) in groups {
        match storage.record_exposure_batch(
            &MutationContext::unkeyed(NamespaceId::new(namespace)),
            &batch,
            sampled_at,
        ) {
            Ok(Some(receipt)) => receipts.push(receipt),
            Ok(None) => {}
            Err(error) => return Err(ObservationError::storage(error, receipts)),
        }
    }
    Ok(receipts)
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
