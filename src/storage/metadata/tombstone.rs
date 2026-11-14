use super::*;
use crate::storage::edges::PersistedEdge;

impl MetadataStore {
    pub(crate) fn tombstone_with_effects(
        &self,
        ctx: &MutationContext,
        id: MemoryId,
        existing_edges: &[PersistedEdge],
    ) -> Result<WriteReceipt, StorageError> {
        self.tombstone_reserved_with_effects(ctx, id, existing_edges, None)
    }

    pub(crate) fn tombstone_reserved_with_effects(
        &self,
        ctx: &MutationContext,
        id: MemoryId,
        existing_edges: &[PersistedEdge],
        child: Option<&crate::storage::batch::BatchChild>,
    ) -> Result<WriteReceipt, StorageError> {
        let guard = self.mutation_guard()?;
        self.ensure_available()?;
        ctx.validate()?;
        let MutationScope::Namespace(namespace) = ctx.scope else {
            return Err(StorageError::InvalidMutationContext(
                "tombstone requires namespace scope".into(),
            ));
        };
        if ctx.contract_version != 1 {
            return Err(StorageError::InvalidMutationContext(
                "explicit-v2 tombstone is not enabled".into(),
            ));
        }
        let tx = self.db.begin_write()?;
        let hash = crate::storage::tombstone::request_hash(ctx, id);
        // Historical identity is checked before CAS or current existence.
        if let Some(child) = child {
            if let Some(receipt) = super::batch::replay_child(&tx, child, hash)? {
                return Ok(receipt);
            }
            super::batch::check_policy(&tx, child)?;
        } else if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok(receipt);
        }
        control::read_control(&tx)?.committed_generation.next()?;
        let config = NamespaceConfig::decode(
            tx.open_table(NAMESPACE_TABLE)?
                .get(namespace.get())?
                .ok_or(StorageError::NamespaceNotFound(namespace.get()))?
                .value(),
        )?;
        if config.schema.is_some() || control::read_control(&tx)?.control_schema_version != 1 {
            return Err(StorageError::RecoveryRequired(
                "schema-v2 tombstone requires participating writers; serving remains disabled"
                    .into(),
            ));
        }
        let mut record = DiskRecord::from_bytes(
            tx.open_table(META_TABLE)?
                .get(id.as_bytes().as_slice())?
                .ok_or(StorageError::NotFound(id.into_inner()))?
                .value(),
        )?;
        if record.id != *id.as_bytes()
            || record.namespace_id != namespace.get()
            || config.id != namespace
        {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        if record.schema.is_some() || record.version != 1 {
            return Err(StorageError::InvalidMutation(
                "schema-v2 tombstone is not enabled".into(),
            ));
        }
        let old_phase = record.phase;
        if old_phase != DecayPhase::Tombstone {
            for tag in &record.tags {
                tx.open_multimap_table(TAG_INDEX)?
                    .remove(tag.as_str(), id.as_bytes().as_slice())?;
            }
            tx.open_multimap_table(NAMESPACE_INDEX)?
                .remove(record.namespace_id, id.as_bytes().as_slice())?;
            record.summary.clear();
            record.tags.clear();
            record.text_offset = 0;
            record.text_length = 0;
            record.phase = DecayPhase::Tombstone;
            record.strength = 0.0;
            record.decay_strength = 0.0;
            tx.open_table(META_TABLE)?
                .insert(id.as_bytes().as_slice(), record.encode()?.as_slice())?;
        }
        self.finish_record_effects(
            &guard,
            tx,
            ctx,
            hash,
            existing_edges,
            &[],
            &[],
            false,
            false,
            effects::RecordMutation::Tombstoned {
                record: &record,
                old_phase,
            },
            child,
        )
        .map(|result| result.receipt)
    }
}
