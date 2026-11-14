//! Bounded canonical purge steps and automatic recovery before admission.
use super::*;
use crate::storage::metadata::purge::CHUNK_LIMIT;
use crate::storage::purge::{PurgeProgress, PurgeStage};
use uuid::Uuid;

impl RedbStorageEngine {
    pub fn begin_namespace_purge(
        &mut self,
        ctx: &MutationContext,
        force: bool,
    ) -> Result<PurgeProgress, StorageError> {
        self.meta_store.ensure_handle_available()?;
        self.meta_store.begin_purge(ctx, force)
    }

    pub fn lookup_namespace_purge(
        &self,
        store_id: Uuid,
        namespace: NamespaceId,
        parent: Uuid,
    ) -> Result<Option<PurgeProgress>, StorageError> {
        self.meta_store
            .lookup_namespace_purge(store_id, namespace, parent)
    }

    /// One bounded record transaction, or one finalization/cleanup step. The
    /// reserved child ID is selected from durable progress, never by the caller.
    pub fn resume_namespace_purge(
        &mut self,
        store_id: Uuid,
        namespace: NamespaceId,
        parent: Uuid,
        chunk_limit: usize,
    ) -> Result<PurgeProgress, StorageError> {
        let p = self
            .lookup_namespace_purge(store_id, namespace, parent)?
            .ok_or_else(|| StorageError::InvalidMutation("unknown purge operation".into()))?;
        self.purge_step(&p, chunk_limit, None)?;
        self.lookup_namespace_purge(store_id, namespace, parent)?
            .ok_or_else(|| StorageError::InvalidControl("lost purge progress".into()))
    }

    fn purge_step(
        &mut self,
        p: &PurgeProgress,
        limit: usize,
        parent: Option<&crate::storage::work::WorkTicket>,
    ) -> Result<Option<crate::storage::metadata::effects::EffectResult>, StorageError> {
        self.meta_store.ensure_handle_available()?;
        match p.stage {
            PurgeStage::Complete => Ok(None),
            PurgeStage::Deleting => {
                let guard = self.meta_store.purge_guard(parent)?;
                let result = self.meta_store.advance_purge(
                    &guard,
                    p,
                    limit,
                    &self.edge_store.load_all_edges()?,
                )?;
                if let Some(result) = &result {
                    effects::apply_pending(
                        &self.meta_store,
                        &guard,
                        &self.edge_store,
                        Some(&mut self.vector_manager),
                    )
                    .map_err(|source| StorageError::CommittedMutation {
                        receipt: Box::new(result.receipt.clone()),
                        source: Box::new(source),
                    })?;
                }
                Ok(result)
            }
            PurgeStage::Cleanup => {
                let guard = self.meta_store.purge_guard(parent)?;
                drop(self.vector_manager.remove(p.namespace_id));
                let path = self.namespace_dir(&p.name)?;
                let root = &self.db_path;
                self.meta_store.finish_purge_cleanup(&guard, p, || {
                    // Name validity and the durable lease were checked before
                    // mutation. Refuse symlink/non-directory replacement.
                    match std::fs::symlink_metadata(&path) {
                        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
                            if path.canonicalize()?.parent() != Some(root.canonicalize()?.as_path())
                            {
                                return Err(StorageError::InvalidPath);
                            }
                            std::fs::remove_dir_all(&path)?;
                        }
                        Ok(_) => return Err(StorageError::InvalidPath),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    crate::storage::fsync::fsync_dir(root)?;
                    crate::storage::metadata::effects::crash_point("after_purge_directory_cleanup");
                    Ok(())
                })?;
                Ok(None)
            }
        }
    }

    /// Startup and the actual completion coordinator drive all accepted work.
    /// Each canonical transaction is bounded; there is no forgotten queue that
    /// needs a manual reopen to make an otherwise healthy purge progress.
    pub(crate) fn resume_pending_purges(&mut self) -> Result<(), StorageError> {
        self.resume_pending_purges_with_work(None)
    }

    pub(crate) fn resume_pending_purges_with_work(
        &mut self,
        parent: Option<&crate::storage::work::WorkTicket>,
    ) -> Result<(), StorageError> {
        loop {
            let active = self
                .meta_store
                .purge_progress()?
                .into_iter()
                .find(|p| p.stage != PurgeStage::Complete);
            let Some(p) = active else {
                return Ok(());
            };
            self.purge_step(&p, CHUNK_LIMIT, parent)?;
        }
    }

    /// Legacy unkeyed caller. Canonical progress/receipts survive a lost response;
    /// blind name-only resubmission has no cross-incarnation replay guarantee.
    pub fn purge_namespace(
        &mut self,
        namespace: NamespaceId,
    ) -> Result<PurgedNamespace, NamespacePurgeError> {
        self.purge_namespace_checked(namespace, true)
    }

    pub(crate) fn purge_namespace_checked(
        &mut self,
        namespace: NamespaceId,
        force: bool,
    ) -> Result<PurgedNamespace, NamespacePurgeError> {
        let config = self
            .meta_store
            .get_namespace(namespace)
            .map_err(NamespacePurgeError::nothing_destroyed)?
            .ok_or_else(|| {
                NamespacePurgeError::nothing_destroyed(StorageError::NamespaceNotFound(
                    namespace.get(),
                ))
            })?;
        let directory_existed = self
            .namespace_dir(&config.name)
            .map_err(NamespacePurgeError::nothing_destroyed)?
            .try_exists()
            .map_err(|e| NamespacePurgeError::nothing_destroyed(e.into()))?;
        let mut p = self
            .begin_namespace_purge(&MutationContext::unkeyed(namespace), force)
            .map_err(NamespacePurgeError::nothing_destroyed)?;
        let mut records = Vec::new();
        let mut peers = std::collections::HashMap::new();
        while p.stage != PurgeStage::Complete {
            match self.purge_step(&p, CHUNK_LIMIT, None) {
                Ok(Some(result)) => {
                    records.extend(result.removed);
                    peers.extend(result.peer_counts);
                }
                Ok(None) => {}
                Err(source) => {
                    return Err(NamespacePurgeError::partial(
                        records,
                        match source {
                            // This child has its own commit evidence. Never
                            // replace it with the previous progress receipt.
                            current @ (StorageError::CommittedMutation { .. }
                            | StorageError::CommitOutcomeUnknown { .. }) => current,
                            source => StorageError::CommittedMutation {
                                receipt: Box::new(p.last_receipt.clone()),
                                source: Box::new(source),
                            },
                        },
                    ));
                }
            }
            p = self
                .lookup_namespace_purge(p.store_id, namespace, p.start_receipt.operation_id)
                .map_err(|source| NamespacePurgeError::partial(records.clone(), source))?
                .ok_or_else(|| {
                    NamespacePurgeError::partial(
                        records.clone(),
                        StorageError::InvalidControl("lost purge operation".into()),
                    )
                })?;
        }
        let ids: std::collections::HashSet<_> = records.iter().map(|(id, _)| *id).collect();
        Ok(PurgedNamespace {
            config,
            live_removed: (p.removed - p.tombstones_removed) as usize,
            tombstones_removed: p.tombstones_removed as usize,
            edges_removed: p.edges_removed as usize,
            peer_edge_counts: peers
                .into_iter()
                .filter(|(id, _)| !ids.contains(id))
                .collect(),
            records,
            directory_removed: directory_existed,
            receipt: p.last_receipt,
        })
    }
}
