//! Canonical legacy exposure/feedback transactions. Consumer awaiting and
//! derived publication are a separate dependency; no explicit-v2 writes here.
use super::*;
use crate::storage::control::ExposureOutcome;

impl MetadataStore {
    /// First occurrence order is retained; identical duplicates append one event.
    /// Different kinds for the same UUID are rejected, never guessed. Only direct
    /// and associative retrieval are exposure kinds. Empty input reserves no key.
    /// Sampled server time is not retry identity; replay returns original times.
    pub fn record_exposure_batch(
        &self,
        ctx: &MutationContext,
        accesses: &[(MemoryId, AccessKind)],
        sampled_at: i64,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        self.ensure_available()?;
        let namespace = legacy_scope(ctx)?;
        if accesses.is_empty() {
            let state = self.control_state()?;
            if ctx.store_id.is_some_and(|id| id != state.store_id) {
                return Err(StorageError::StoreLineageMismatch);
            }
            if self.get_namespace(namespace)?.is_none() {
                return Err(StorageError::NamespaceNotFound(namespace.get()));
            }
            return Ok(None);
        }
        let publication = self.mutation_guard()?;
        let (tx, receipt) = self.prepare_exposures(&publication, ctx, accesses, sampled_at)?;
        self.commit_observation(tx, &receipt)?;
        Ok(Some(receipt))
    }

    fn prepare_exposures(
        &self,
        _publication: &std::sync::RwLockWriteGuard<'_, ()>,
        ctx: &MutationContext,
        accesses: &[(MemoryId, AccessKind)],
        sampled_at: i64,
    ) -> Result<(redb::WriteTransaction, WriteReceipt), StorageError> {
        let namespace = legacy_scope(ctx)?;
        let mut seen = HashMap::new();
        let mut distinct = Vec::new();
        for &(id, kind) in accesses {
            if id == MemoryId::nil()
                || !matches!(
                    kind,
                    AccessKind::DirectRetrieval | AccessKind::AssociativeRetrieval
                )
            {
                return Err(StorageError::InvalidMutation(
                    "invalid exposure ID/kind".into(),
                ));
            }
            match seen.insert(id, kind) {
                Some(previous) if previous != kind => {
                    return Err(StorageError::InvalidMutation(
                        "contradictory exposure kinds for one UUID".into(),
                    ));
                }
                Some(_) => {}
                None => distinct.push((id, kind)),
            }
        }
        if distinct.is_empty() {
            return Err(StorageError::InvalidMutation(
                "empty exposure transaction".into(),
            ));
        }
        let bytes = control::encode(&distinct)?;
        let hash = control::request_hash(ctx, "record_exposure_batch", &[&bytes]);
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok((tx, receipt));
        }
        validate_observation_time(sampled_at)?;
        read_namespace(&tx, namespace)?;
        let mut outcomes = Vec::with_capacity(distinct.len());
        {
            let mut table = tx.open_table(META_TABLE)?;
            for (id, kind) in distinct {
                let mut record = read_record(&table, id, namespace)?;
                // Legacy exposure still updates the decay anchor/history. A clock
                // moving backwards must not overwrite a later canonical anchor.
                let timestamp = sampled_at
                    .max(record.created_at)
                    .max(record.last_accessed_at);
                validate_observation_time(timestamp)?;
                record.last_accessed_at = timestamp;
                if record.access_history.len() >= ACCESS_HISTORY_MAX {
                    record.access_history.remove(0);
                }
                record.access_history.push(AccessEvent { timestamp, kind });
                table.insert(id.as_bytes().as_slice(), record.to_bytes().as_slice())?;
                outcomes.push(ExposureOutcome {
                    memory_id: id,
                    kind,
                    accessed_at: timestamp,
                });
            }
        }
        let receipt = control::record_receipt(
            &tx,
            ctx,
            hash,
            MutationOutcome::ExposuresRecorded { accesses: outcomes },
        )?;
        Ok((tx, receipt))
    }

    /// Reads and plans against the current record/policy inside the write
    /// transaction. Quality/ID are semantic input; sampled time and server
    /// configuration are not. The caller freezes configuration for the call.
    pub fn record_legacy_feedback(
        &self,
        ctx: &MutationContext,
        id: MemoryId,
        quality: u8,
        sampled_at: i64,
        global_multiplier: f64,
    ) -> Result<WriteReceipt, StorageError> {
        let publication = self.mutation_guard()?;
        let (tx, receipt) = self.prepare_feedback(
            &publication,
            ctx,
            id,
            quality,
            sampled_at,
            global_multiplier,
        )?;
        self.commit_observation(tx, &receipt)?;
        Ok(receipt)
    }

    fn prepare_feedback(
        &self,
        _publication: &std::sync::RwLockWriteGuard<'_, ()>,
        ctx: &MutationContext,
        id: MemoryId,
        quality: u8,
        sampled_at: i64,
        global_multiplier: f64,
    ) -> Result<(redb::WriteTransaction, WriteReceipt), StorageError> {
        let namespace = legacy_scope(ctx)?;
        let hash =
            control::request_hash(ctx, "record_legacy_feedback", &[id.as_bytes(), &[quality]]);
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok((tx, receipt));
        }
        validate_observation_time(sampled_at)?;
        let policy = read_namespace(&tx, namespace)?;
        let mut record = read_record(&tx.open_table(META_TABLE)?, id, namespace)?;
        let resolved = crate::decay::lifecycle::resolve_policy(
            &policy,
            &crate::decay::DecayConfig::default(),
            global_multiplier,
        )
        .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
        let plan = crate::decay::lifecycle::plan_feedback(&record, &resolved, sampled_at, quality)
            .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
        validate_observation_time(plan.timestamp)?;
        if !super::lifecycle_updates::patch_legacy_feedback(&mut record, &plan)? {
            return Err(StorageError::InvalidMutation(
                "feedback content unavailable".into(),
            ));
        }
        tx.open_table(META_TABLE)?
            .insert(id.as_bytes().as_slice(), record.to_bytes().as_slice())?;
        let receipt = control::record_receipt(
            &tx,
            ctx,
            hash,
            MutationOutcome::LegacyFeedbackRecorded {
                memory_id: id,
                quality,
                applied_at: plan.timestamp,
            },
        )?;
        Ok((tx, receipt))
    }

    pub(super) fn commit_observation(
        &self,
        tx: redb::WriteTransaction,
        receipt: &WriteReceipt,
    ) -> Result<(), StorageError> {
        if !receipt.replayed {
            tx.commit().map_err(|source| {
                self.mark_unavailable();
                StorageError::CommitOutcomeUnknown {
                    operation_id: receipt.operation_id,
                    source,
                }
            })?;
        }
        Ok(())
    }
}

fn legacy_scope(ctx: &MutationContext) -> Result<NamespaceId, StorageError> {
    ctx.validate()?;
    if ctx.contract_version != 1 {
        return Err(StorageError::InvalidMutationContext(
            "explicit-v2 observation is not enabled".into(),
        ));
    }
    match ctx.scope {
        MutationScope::Namespace(id) => Ok(id),
        MutationScope::Catalog => Err(StorageError::InvalidMutationContext(
            "observation requires namespace scope".into(),
        )),
    }
}

fn read_namespace(
    tx: &redb::WriteTransaction,
    id: NamespaceId,
) -> Result<NamespaceConfig, StorageError> {
    let table = tx.open_table(NAMESPACE_TABLE)?;
    let value = table
        .get(id.get())?
        .ok_or(StorageError::NamespaceNotFound(id.get()))?;
    let ns: NamespaceConfig = serde_json::from_slice(value.value())
        .map_err(|e| StorageError::Deserialize(e.to_string()))?;
    if ns.id != id {
        return Err(StorageError::MutationNamespaceMismatch);
    }
    Ok(ns)
}

fn read_record(
    table: &redb::Table<&[u8], &[u8]>,
    id: MemoryId,
    ns: NamespaceId,
) -> Result<DiskRecord, StorageError> {
    let value = table
        .get(id.as_bytes().as_slice())?
        .ok_or(StorageError::NotFound(id.into_inner()))?;
    let record = DiskRecord::from_bytes(value.value())?;
    if record.id != *id.as_bytes() {
        return Err(StorageError::InvalidMutation(
            "record key/UUID mismatch".into(),
        ));
    }
    if record.namespace_id != ns.get() {
        return Err(StorageError::MutationNamespaceMismatch);
    }
    if record.phase == DecayPhase::Tombstone {
        return Err(StorageError::NotFound(id.into_inner()));
    }
    Ok(record)
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;

fn validate_observation_time(timestamp: i64) -> Result<(), StorageError> {
    if chrono::DateTime::from_timestamp_millis(timestamp).is_none() {
        return Err(StorageError::InvalidMutation(
            "unrepresentable observation timestamp".into(),
        ));
    }
    Ok(())
}
