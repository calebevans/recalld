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
        self.ensure_available()?;
        let receipt =
            self.meta_store
                .tombstone_with_effects(ctx, id, &self.edge_store.load_all_edges()?)?;
        if !receipt.replayed {
            effects::apply_pending(
                &self.meta_store,
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
