use super::*;
use crate::embedding::runtime::GeneratedEmbedding;
use crate::storage::native_lifecycle::NativePolicyInputs;
use crate::storage::native_records::{NativeInsertIntent, NativeTombstoneIntent, hash_bytes};
use crate::storage::work::WorkTicket;
use crate::time::Clock;

impl RedbStorageEngine {
    fn native_text_hash(&self, id: MemoryId) -> Result<Option<[u8; 32]>, StorageError> {
        let record = self
            .meta_store
            .get(id)?
            .ok_or(StorageError::NotFound(id.into_inner()))?;
        if record.text_length == 0 {
            return Ok(None);
        }
        let text = self.text_store.read(TextRef {
            file_offset: record.text_offset,
            length: record.text_length,
        })?;
        Ok(Some(hash_bytes(text.as_bytes())))
    }
    pub(crate) fn insert_native_record(
        &mut self,
        ctx: &MutationContext,
        intent: &NativeInsertIntent,
        generated: &GeneratedEmbedding,
        policy: &NativePolicyInputs,
        clock: &dyn Clock,
        parent: Option<&WorkTicket>,
    ) -> Result<WriteReceipt, StorageError> {
        // Replay before current row/policy/provider output, then repeat inside
        // the actual final transaction. A miss here reserves nothing.
        if let Some(receipt) =
            self.meta_store
                .lookup_native_insert(ctx, &intent.original, parent)?
        {
            return Ok(receipt);
        }
        let guard = self.meta_store.native_guard_with_work(parent)?;
        let edges = self.edge_store.load_all_edges()?;
        let mut hashes = std::collections::HashMap::new();
        for (id, _) in &intent.capture.records {
            hashes.insert(*id, self.native_text_hash(*id)?);
        }
        let embedding = intent
            .original
            .memory
            .embedding
            .as_deref()
            .unwrap_or(&generated.values);
        let profile = intent
            .original
            .embedding_profile
            .as_ref()
            .or(generated.profile.as_ref())
            .ok_or_else(|| {
                StorageError::InvalidMutation("native embedding profile missing".into())
            })?;
        let vectors = &mut self.vector_manager;
        let text = &mut self.text_store;
        let mut staged = false;
        let result = self.meta_store.insert_native_with_effects(
            &guard,
            ctx,
            intent,
            policy,
            clock,
            embedding,
            profile,
            &edges,
            &hashes,
            |namespace, record, normalized| {
                let store = vectors
                    .get_mut(namespace.id)
                    .ok_or(StorageError::NamespaceNotFound(namespace.id.get()))?;
                staged = true;
                record.vector_slot = store.insert_vector(normalized)?;
                if let Some(value) = &intent.original.memory.full_text {
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
        if result.is_err() && staged {
            self.meta_store.mark_unavailable();
        }
        let receipt = result?;
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
    pub(crate) fn tombstone_native_record(
        &mut self,
        ctx: &MutationContext,
        intent: &NativeTombstoneIntent,
        policy: &NativePolicyInputs,
        clock: &dyn Clock,
        parent: Option<&WorkTicket>,
    ) -> Result<WriteReceipt, StorageError> {
        let guard = self.meta_store.native_guard_with_work(parent)?;
        if let Some(receipt) = self
            .meta_store
            .lookup_native_tombstone(&guard, ctx, intent)?
        {
            return Ok(receipt);
        }
        let text_hash = self.native_text_hash(intent.memory_id)?;
        let receipt = self.meta_store.tombstone_native_with_effects(
            &guard,
            ctx,
            intent,
            policy,
            clock,
            &self.edge_store.load_all_edges()?,
            text_hash,
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
