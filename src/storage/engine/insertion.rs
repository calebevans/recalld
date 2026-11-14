use super::*;
use crate::storage::insertion::InsertMemory;

impl RedbStorageEngine {
    /// Optional pre-provider lookup. The final mutation repeats verification;
    /// absence here never reserves a key or permits bypassing final checks.
    pub fn lookup_insert_replay(
        &self,
        ctx: &MutationContext,
        input: &InsertMemory,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        self.meta_store.lookup_insert_replay(ctx, input)
    }

    /// Commit a canonical insertion and durable publication work. The receipt
    /// acknowledges canonical state, not readiness; the caller completes all
    /// derived views before returning a ready result. Replay never allocates.
    pub fn insert_memory_with_receipt(
        &mut self,
        ctx: &MutationContext,
        input: &InsertMemory,
        generated_embedding: &[f32],
        sampled_at: i64,
    ) -> Result<WriteReceipt, StorageError> {
        self.ensure_available()?;
        let edges = self.edge_store.load_all_edges()?;
        let vectors = &mut self.vector_manager;
        let text = &mut self.text_store;
        let mut staging_started = false;
        let embedding = input.embedding.as_deref().unwrap_or(generated_embedding);
        let result = self.meta_store.insert_with_effects(
            ctx,
            input,
            sampled_at,
            embedding,
            &edges,
            |namespace, record, normalized| {
                let store = vectors
                    .get_mut(namespace.id)
                    .ok_or(StorageError::NamespaceNotFound(namespace.id.get()))?;
                staging_started = true;
                record.vector_slot = store.insert_vector(normalized)?;
                if let Some(value) = &input.full_text {
                    let reference = text.append(value)?;
                    record.text_offset = reference.file_offset;
                    record.text_length = reference.length;
                }
                store.sync()?;
                text.sync_data()?;
                crate::storage::metadata::effects::crash_point("after_insert_payload_sync");
                Ok(())
            },
        );
        if result.is_err() && staging_started {
            // Never free a possibly committed slot on an I/O return. Recovery
            // validates every canonical owner before reclaiming unowned bytes.
            self.meta_store.mark_unavailable();
        }
        let receipt = result?;
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

#[cfg(test)]
#[path = "insertion_tests.rs"]
mod tests;
