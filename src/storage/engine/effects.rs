//! Commit metadata first, then deterministically materialize the separate edge DB.
use super::*;
use crate::storage::metadata::effects::{EffectResult, crash_point};
use std::sync::atomic::Ordering;

impl RedbStorageEngine {
    /// Called under the caller's graph -> storage write guards. No graph lock
    /// is acquired here. The caller publishes graph/cache then acknowledges.
    pub(crate) fn put_edges_with_effects(
        &self,
        puts: &[PersistedEdge],
    ) -> Result<Option<EffectResult>, StorageError> {
        self.put_edges_with_effects_with_work(puts, None)
    }

    pub(crate) fn put_edges_with_effects_with_work(
        &self,
        puts: &[PersistedEdge],
        parent: Option<&crate::storage::work::WorkTicket>,
    ) -> Result<Option<EffectResult>, StorageError> {
        let guard = self.meta_store.mutation_guard_with_work(parent)?;
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
        apply_pending(&self.meta_store, &guard, &self.edge_store, None).map_err(|source| {
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
            &guard,
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
        let guard = self.meta_store.canonical_effects_guard(None)?;
        apply_pending(
            &self.meta_store,
            &guard,
            &self.edge_store,
            Some(&mut self.vector_manager),
        )
    }

    pub(crate) fn acknowledge_graph_effects(
        &self,
        operation_id: uuid::Uuid,
    ) -> Result<(), StorageError> {
        self.acknowledge_graph_effects_with_work(operation_id, None)
    }

    pub(crate) fn acknowledge_graph_effects_with_work(
        &self,
        operation_id: uuid::Uuid,
        parent: Option<&crate::storage::work::WorkTicket>,
    ) -> Result<(), StorageError> {
        self.meta_store
            .acknowledge_derived_effects_with_work(operation_id, parent)
            .map_err(|source| StorageError::CommittedEffectsPending {
                operation_id,
                source: Box::new(source),
            })
    }
}

pub(super) fn apply_pending(
    meta: &MetadataStore,
    guard: &crate::storage::metadata::MutationGuard<'_>,
    edges: &EdgeStore,
    mut vectors: Option<&mut VectorManager>,
) -> Result<(), StorageError> {
    let result = (|| {
        guard.check(meta)?;
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
            crate::storage::metadata::effects::validate_plan(effect, &records)?;
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
            meta.acknowledge_canonical_effects(guard, effect)?;
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
