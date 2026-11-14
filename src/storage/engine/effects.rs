//! Commit metadata first, then deterministically materialize the separate edge DB.
use super::*;
use crate::storage::metadata::effects::{EffectResult, PendingEffects, crash_point};
use std::sync::atomic::Ordering;

impl RedbStorageEngine {
    /// Called under the caller's graph -> storage write guards. No graph lock
    /// is acquired here. The caller publishes graph/cache then acknowledges.
    pub(crate) fn put_edges_with_effects(
        &self,
        puts: &[PersistedEdge],
    ) -> Result<Option<EffectResult>, StorageError> {
        let guard = self.meta_store.mutation_guard()?;
        let Some(first) = puts.first() else {
            return Ok(None);
        };
        let record = self
            .meta_store
            .get(first.source)?
            .ok_or(StorageError::NotFound(first.source.into_inner()))?;
        let result = self.meta_store.commit_graph_effects(
            &guard,
            NamespaceId::new(record.namespace_id),
            &self.edge_store.load_all_edges()?,
            puts,
            &[],
            false,
            false,
        )?;
        apply_pending(&self.meta_store, &self.edge_store, None).map_err(|source| {
            StorageError::CommittedEffectsPending {
                operation_id: result.receipt.operation_id,
                source: Box::new(source),
            }
        })?;
        Ok(Some(result))
    }

    /// One canonical transaction removes records/incident edges, writes bridge
    /// edges and absolute peer counts, and records recovery/derived work.
    pub(crate) fn delete_memory_with_edges(
        &mut self,
        id: MemoryId,
        bridges: &[PersistedEdge],
    ) -> Result<Option<EffectResult>, StorageError> {
        let guard = self.meta_store.mutation_guard()?;
        let Some(record) = self.meta_store.get(id)? else {
            return Ok(None);
        };
        let result = self.meta_store.commit_graph_effects(
            &guard,
            NamespaceId::new(record.namespace_id),
            &self.edge_store.load_all_edges()?,
            bridges,
            &[id],
            true,
            true,
        )?;
        apply_pending(
            &self.meta_store,
            &self.edge_store,
            Some(&mut self.vector_manager),
        )
        .map_err(|source| StorageError::CommittedEffectsPending {
            operation_id: result.receipt.operation_id,
            source: Box::new(source),
        })?;
        Ok(Some(result))
    }

    pub(crate) fn recover_pending_effects(&mut self) -> Result<(), StorageError> {
        // Constructor only: no handle has escaped, and normal admission is
        // blocked for pending canonical effects. Replay precedes orphan cleanup.
        apply_pending(
            &self.meta_store,
            &self.edge_store,
            Some(&mut self.vector_manager),
        )
    }

    pub(crate) fn acknowledge_graph_effects(
        &self,
        operation_id: uuid::Uuid,
    ) -> Result<(), StorageError> {
        self.meta_store
            .acknowledge_derived_effects(operation_id)
            .map_err(|source| StorageError::CommittedEffectsPending {
                operation_id,
                source: Box::new(source),
            })
    }
}

pub(super) fn apply_pending(
    meta: &MetadataStore,
    edges: &EdgeStore,
    mut vectors: Option<&mut VectorManager>,
) -> Result<(), StorageError> {
    let result = (|| {
        #[cfg(test)]
        if meta.fail_effect_application.swap(false, Ordering::SeqCst) {
            return Err(StorageError::RecoveryRequired(
                "injected effects application failure".into(),
            ));
        }
        let pending = meta.pending_effects()?;
        for effect in pending.iter().filter(|e| !e.canonical_applied) {
            let records = meta.scan_all_for_recovery()?;
            // Refuse corrupt recovery instructions before changing any files.
            validate_plan(effect, &records)?;
            edges.apply_effects(&effect.puts, &effect.deletes)?;
            crash_point("after_edge_commit");
            for release in &effect.releases {
                let manager = vectors.as_mut().ok_or_else(|| {
                    StorageError::RecoveryRequired(
                        "vector release requires exclusive recovery".into(),
                    )
                })?;
                let store = manager
                    .get_mut(release.namespace_id)
                    .ok_or(StorageError::NamespaceNotFound(release.namespace_id.get()))?;
                store.free_slot(release.slot)?;
                store.sync()?;
            }
            crash_point("after_vector_sync");
            meta.acknowledge_canonical_effects(effect)?;
            crash_point("after_canonical_ack");
        }
        meta.canonical_pending.store(false, Ordering::SeqCst);
        Ok(())
    })();
    if result.is_err() {
        meta.mark_unavailable();
    }
    result
}

fn validate_plan(
    effect: &PendingEffects,
    records: &[(MemoryId, DiskRecord)],
) -> Result<(), StorageError> {
    if effect
        .removed_ids
        .iter()
        .any(|id| records.iter().any(|(key, _)| key == id))
    {
        return Err(StorageError::InvalidControl(
            "removed effect ID still exists".into(),
        ));
    }
    for release in &effect.releases {
        if records.iter().any(|(id, r)| {
            *id == release.memory_id
                || (r.phase != DecayPhase::Tombstone
                    && r.namespace_id == release.namespace_id.get()
                    && r.vector_slot == release.slot)
        }) {
            return Err(StorageError::OwnershipMismatch(
                "effect attempts to release an owned vector".into(),
            ));
        }
    }
    for (id, count) in &effect.peer_counts {
        if records
            .iter()
            .find(|(key, _)| key == id)
            .is_none_or(|(_, r)| r.edge_count != *count)
        {
            return Err(StorageError::InvalidControl(
                "effect peer count disagrees with canonical metadata".into(),
            ));
        }
    }
    for edge in &effect.puts {
        let MutationScope::Namespace(namespace) = effect.scope else {
            return Err(StorageError::InvalidControl(
                "graph effect has catalog scope".into(),
            ));
        };
        if [edge.source, edge.target].iter().any(|id| {
            records
                .iter()
                .find(|(key, _)| key == id)
                .is_none_or(|(_, record)| record.namespace_id != namespace.get())
        }) {
            return Err(StorageError::InvalidControl(
                "effect put has missing or foreign endpoint".into(),
            ));
        }
    }
    Ok(())
}
