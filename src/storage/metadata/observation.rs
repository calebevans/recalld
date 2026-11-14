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
        self.record_exposure_batch_scoped(ctx, accesses, sampled_at, None)
    }

    pub(crate) fn record_exposure_batch_scoped(
        &self,
        ctx: &MutationContext,
        accesses: &[(MemoryId, AccessKind)],
        sampled_at: i64,
        scope: Option<&crate::storage::control::ExposureScope>,
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
        let (tx, receipt) =
            self.prepare_exposures_scoped(&publication, ctx, accesses, sampled_at, scope)?;
        self.commit_observation(tx, &receipt)?;
        Ok(Some(receipt))
    }

    #[cfg(test)]
    fn prepare_exposures(
        &self,
        publication: &std::sync::RwLockWriteGuard<'_, ()>,
        ctx: &MutationContext,
        accesses: &[(MemoryId, AccessKind)],
        sampled_at: i64,
    ) -> Result<(redb::WriteTransaction, WriteReceipt), StorageError> {
        self.prepare_exposures_scoped(publication, ctx, accesses, sampled_at, None)
    }

    fn prepare_exposures_scoped(
        &self,
        _publication: &std::sync::RwLockWriteGuard<'_, ()>,
        ctx: &MutationContext,
        accesses: &[(MemoryId, AccessKind)],
        sampled_at: i64,
        scope: Option<&crate::storage::control::ExposureScope>,
    ) -> Result<(redb::WriteTransaction, WriteReceipt), StorageError> {
        self.effects_stamp()?;
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
        let scope_bytes = scope.map(control::encode).transpose()?;
        let hash = match scope_bytes.as_deref() {
            Some(assertion) => {
                control::request_hash(ctx, "record_exposure_batch", &[&bytes, assertion])
            }
            None => control::request_hash(ctx, "record_exposure_batch", &[&bytes]),
        };
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok((tx, receipt));
        }
        validate_observation_time(sampled_at)?;
        let canonical_namespace = read_namespace(&tx, namespace)?;
        if let Some(scope) = scope {
            if canonical_namespace.name != scope.namespace {
                return Err(StorageError::MutationNamespaceMismatch);
            }
            if let Some(source) = scope.source {
                super::removal_ledger::ensure_insertable(&tx, source)?;
                read_record(&tx.open_table(META_TABLE)?, source, namespace)?;
            }
        }
        let mut outcomes = Vec::with_capacity(distinct.len());
        {
            let mut table = tx.open_table(META_TABLE)?;
            for (id, kind) in distinct {
                super::removal_ledger::ensure_insertable(&tx, id)?;
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
        self.record_legacy_feedback_scoped(ctx, id, quality, sampled_at, global_multiplier, None)
    }

    pub(crate) fn record_legacy_feedback_scoped(
        &self,
        ctx: &MutationContext,
        id: MemoryId,
        quality: u8,
        sampled_at: i64,
        global_multiplier: f64,
        expected_namespace: Option<&str>,
    ) -> Result<WriteReceipt, StorageError> {
        let publication = self.mutation_guard()?;
        let (tx, receipt) = self.prepare_feedback_scoped(
            &publication,
            ctx,
            id,
            quality,
            sampled_at,
            global_multiplier,
            expected_namespace,
        )?;
        self.commit_observation(tx, &receipt)?;
        Ok(receipt)
    }

    #[cfg(test)]
    fn prepare_feedback(
        &self,
        publication: &std::sync::RwLockWriteGuard<'_, ()>,
        ctx: &MutationContext,
        id: MemoryId,
        quality: u8,
        sampled_at: i64,
        global_multiplier: f64,
    ) -> Result<(redb::WriteTransaction, WriteReceipt), StorageError> {
        self.prepare_feedback_scoped(
            publication,
            ctx,
            id,
            quality,
            sampled_at,
            global_multiplier,
            None,
        )
    }

    fn prepare_feedback_scoped(
        &self,
        _publication: &std::sync::RwLockWriteGuard<'_, ()>,
        ctx: &MutationContext,
        id: MemoryId,
        quality: u8,
        sampled_at: i64,
        global_multiplier: f64,
        expected_namespace: Option<&str>,
    ) -> Result<(redb::WriteTransaction, WriteReceipt), StorageError> {
        let namespace = legacy_scope(ctx)?;
        let hash = match expected_namespace {
            Some(name) => control::request_hash(
                ctx,
                "record_legacy_feedback",
                &[id.as_bytes(), &[quality], name.as_bytes()],
            ),
            None => {
                control::request_hash(ctx, "record_legacy_feedback", &[id.as_bytes(), &[quality]])
            }
        };
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok((tx, receipt));
        }
        validate_observation_time(sampled_at)?;
        let policy = read_namespace(&tx, namespace)?;
        if expected_namespace.is_some_and(|name| name != policy.name) {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        super::removal_ledger::ensure_insertable(&tx, id)?;
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
    let ns = NamespaceConfig::decode(value.value())
        .map_err(|e| StorageError::Deserialize(e.to_string()))?;
    if ns.id != id {
        return Err(StorageError::MutationNamespaceMismatch);
    }
    if ns.schema.as_ref().is_some_and(|s| {
        s.lifecycle_version == crate::model::schema::LifecycleVersion::ExplicitFeedbackV2
    }) {
        return Err(StorageError::InvalidMutation(
            "explicit-feedback observation writer unavailable".into(),
        ));
    }
    if ns.schema.is_some() {
        return Err(StorageError::InvalidMutation(
            "schema-v2 observation writer unavailable".into(),
        ));
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
    if record.schema.as_ref().is_some_and(|s| {
        s.lifecycle.lifecycle_version == crate::model::schema::LifecycleVersion::ExplicitFeedbackV2
    }) {
        return Err(StorageError::InvalidMutation(
            "explicit-feedback observation writer unavailable".into(),
        ));
    }
    if record.schema.is_some() {
        return Err(StorageError::InvalidMutation(
            "schema-v2 observation writer unavailable".into(),
        ));
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
