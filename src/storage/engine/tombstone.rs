use super::*;

impl RedbStorageEngine {
    /// Retain terminal identity and structural edges, committing stripped metadata,
    /// receipt, ledger and durable publication work together. Derived readiness
    /// requires completion; separate payload files are reclaimed through effects.
    pub fn tombstone_memory_with_receipt(
        &mut self,
        ctx: &MutationContext,
        id: MemoryId,
    ) -> Result<WriteReceipt, StorageError> {
        self.tombstone_memory_reserved(ctx, id, None)
    }

    pub(crate) fn tombstone_memory_reserved(
        &mut self,
        ctx: &MutationContext,
        id: MemoryId,
        child: Option<&crate::storage::batch::BatchChild>,
    ) -> Result<WriteReceipt, StorageError> {
        self.tombstone_memory_reserved_with_work(ctx, id, child, None)
    }

    pub(crate) fn tombstone_memory_with_work(
        &mut self,
        ctx: &MutationContext,
        id: MemoryId,
        parent: Option<&crate::storage::work::WorkTicket>,
    ) -> Result<WriteReceipt, StorageError> {
        self.tombstone_memory_reserved_with_work(ctx, id, None, parent)
    }

    pub(crate) fn tombstone_memory_reserved_with_work(
        &mut self,
        ctx: &MutationContext,
        id: MemoryId,
        child: Option<&crate::storage::batch::BatchChild>,
        parent: Option<&crate::storage::work::WorkTicket>,
    ) -> Result<WriteReceipt, StorageError> {
        self.ensure_available()?;
        let guard = self.meta_store.mutation_guard_with_work(parent)?;
        let receipt = self.meta_store.tombstone_reserved_with_effects(
            &guard,
            ctx,
            id,
            &self.edge_store.load_all_edges()?,
            child,
        )?;
        if !receipt.replayed {
            effects::apply_pending(
                &self.meta_store,
                &guard,
                &self.edge_store,
                Some(&mut self.vector_manager),
            )
            .map_err(|source| StorageError::CommittedMutation {
                receipt: Box::new(receipt.clone()),
                source: Box::new(source),
            })?;
        }
        Ok(receipt)
    }
}
