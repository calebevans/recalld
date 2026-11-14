//! In-place native lifecycle transactions on the existing metadata/publication
//! core. An already-owned engine can use these primitives without a new session.
use super::*;
use crate::storage::native_lifecycle::*;
use crate::time::Clock;
use sha2::{Digest, Sha256};

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_NATIVE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CAPTURE_CHECKED: std::cell::Cell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn capture_checked() {
    CAPTURE_CHECKED.with(|slot| {
        if let Some(sender) = slot.take() {
            sender.send(()).unwrap();
        }
    });
}

fn policy_hash(
    ns: &NamespaceConfig,
    p: &crate::decay::lifecycle::EffectiveLifecyclePolicy,
) -> Result<[u8; 32], StorageError> {
    let c = &p.config;
    let values = [
        c.initial_stability,
        c.difficulty,
        c.phase_1_threshold,
        c.phase_2_threshold,
        c.phase_3_threshold,
        c.permastore_threshold,
        c.max_connection_bonus,
        c.partial_access_weight,
        p.decay_rate_multiplier,
    ];
    Ok(Sha256::digest(control::encode(&(ns, &values))?).into())
}
fn normalize(
    accesses: &[(MemoryId, AccessKind)],
) -> Result<Vec<(MemoryId, AccessKind)>, StorageError> {
    let mut result: Vec<(MemoryId, AccessKind)> = Vec::new();
    for &(id, kind) in accesses {
        if !matches!(
            kind,
            AccessKind::DirectRetrieval | AccessKind::AssociativeRetrieval
        ) {
            return Err(StorageError::InvalidMutation(
                "exposure kind must be direct or associative".into(),
            ));
        }
        if let Some((_, old)) = result.iter_mut().find(|(v, _)| *v == id) {
            if kind == AccessKind::DirectRetrieval {
                *old = kind;
            }
        } else {
            result.push((id, kind));
        }
    }
    Ok(result)
}
impl MetadataStore {
    /// Only for this already-validated, owned handle, under publication. Opening
    /// the handle proves the complete receipt/schema suffix. Participating writes
    /// preserve it atomically; an uncertain commit poisons the handle until a
    /// strict reopen. Do not run that lifetime-history proof on each observation.
    fn native_control_at(
        &self,
        read: &redb::ReadTransaction,
    ) -> Result<ControlState, StorageError> {
        self.ensure_available()?;
        if !self.native_lifecycle_only {
            return Err(StorageError::RecoveryRequired(
                "explicit native lifecycle entry required".into(),
            ));
        }
        let table = read.open_table(control::CONTROL)?;
        if table.len()? != 1 {
            return Err(invalid("invalid native control singleton"));
        }
        let value = table
            .get("state")?
            .ok_or_else(|| invalid("missing native control"))?;
        let state = ControlState::decode(value.value())?;
        if state.control_schema_version != 2 {
            return Err(invalid("native control schema required"));
        }
        crate::storage::lineage::ensure_serving(read, &state)?;
        Ok(state)
    }
    pub(super) fn native_guard(&self) -> Result<MutationGuard<'_>, StorageError> {
        if !self.native_lifecycle_only {
            return Err(StorageError::RecoveryRequired(
                "explicit native lifecycle entry required".into(),
            ));
        }
        self.native_guard_with_work(None)
    }
    pub(crate) fn native_guard_with_work(
        &self,
        parent: Option<&WorkTicket>,
    ) -> Result<MutationGuard<'_>, StorageError> {
        let guard = self.publication_guard(parent)?;
        self.ensure_available()?;
        let read = self.db.begin_read()?;
        self.native_control_at(&read)?;
        Ok(guard)
    }
    pub fn capture_exposure(
        &self,
        scope: LifecycleScope,
        original: &[(MemoryId, AccessKind)],
        policy: &NativePolicyInputs,
    ) -> Result<ExposureIntent, StorageError> {
        self.ensure_available()?;
        #[cfg(test)]
        capture_checked();
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        let read = self.db.begin_read()?;
        self.capture_exposure_at(&read, scope, original, policy)
    }
    /// Bind to this exact canonical read transaction. A query adapter must pass
    /// its actual guarded snapshot, never call a later capture and relabel it.
    pub(crate) fn capture_exposure_at(
        &self,
        read: &redb::ReadTransaction,
        scope: LifecycleScope,
        original: &[(MemoryId, AccessKind)],
        policy: &NativePolicyInputs,
    ) -> Result<ExposureIntent, StorageError> {
        self.ensure_available()?;
        let original = normalize(original)?;
        let mut ids = original.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        if let Some(id) = scope.source {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let capture = self.capture_at(read, &scope, &ids, policy, true)?;
        Ok(ExposureIntent {
            scope,
            original,
            capture,
        })
    }
    pub fn capture_feedback(
        &self,
        id: MemoryId,
        namespace: Option<NamespaceId>,
        quality: u8,
        policy: &NativePolicyInputs,
    ) -> Result<FeedbackIntent, StorageError> {
        if !(1..=4).contains(&quality) {
            return Err(StorageError::InvalidMutation(
                "quality must be in 1..=4".into(),
            ));
        }
        self.ensure_available()?;
        #[cfg(test)]
        capture_checked();
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        self.ensure_available()?;
        let read = self.db.begin_read()?;
        let record = {
            let table = read.open_table(META_TABLE)?;
            let value = table
                .get(id.as_bytes().as_slice())?
                .ok_or(StorageError::NotFound(id.into_inner()))?;
            DiskRecord::from_bytes(value.value())?
        };
        let ns = NamespaceId::new(record.namespace_id);
        if namespace.is_some_and(|expected| expected != ns) {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        let cfg = {
            let table = read.open_table(NAMESPACE_TABLE)?;
            let value = table
                .get(ns.get())?
                .ok_or(StorageError::NamespaceNotFound(ns.get()))?;
            NamespaceConfig::decode(value.value())?
        };
        let scope = LifecycleScope {
            namespace_id: ns,
            namespace_name: cfg.name,
            source: None,
        };
        let capture = self.capture_at(&read, &scope, &[id], policy, true)?;
        Ok(FeedbackIntent {
            scope,
            id,
            quality,
            capture,
        })
    }
    pub(super) fn capture_at(
        &self,
        read: &redb::ReadTransaction,
        scope: &LifecycleScope,
        ids: &[MemoryId],
        policy: &NativePolicyInputs,
        live_only: bool,
    ) -> Result<CapturedLifecycle, StorageError> {
        let state = self.native_control_at(read)?;
        let ns = {
            let table = read.open_table(NAMESPACE_TABLE)?;
            let value = table
                .get(scope.namespace_id.get())?
                .ok_or(StorageError::NamespaceNotFound(scope.namespace_id.get()))?;
            NamespaceConfig::decode(value.value())?
        };
        if ns.name != scope.namespace_name {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        let schema = ns
            .schema
            .as_ref()
            .ok_or_else(|| invalid("native namespace schema missing"))?;
        let resolved = policy.resolve(&ns)?;
        let mut records = Vec::new();
        let table = read.open_table(META_TABLE)?;
        for id in ids {
            let value = table
                .get(id.as_bytes().as_slice())?
                .ok_or(StorageError::NotFound(id.into_inner()))?;
            let r = DiskRecord::from_bytes(value.value())?;
            if r.namespace_id != ns.id.get() {
                return Err(StorageError::MutationNamespaceMismatch);
            }
            let fields = LifecycleState::capture(&r)?;
            if live_only && !fields.eligible() {
                return Err(StorageError::LifecycleContentUnavailable);
            }
            records.push((*id, fields.lifecycle.record_generation));
        }
        Ok(CapturedLifecycle {
            generation: state.generation_ref(),
            version: schema.lifecycle_version,
            revision: schema.policy_revision,
            policy_hash: policy_hash(&ns, &resolved)?,
            records,
        })
    }
    pub(super) fn check_native_capture(
        &self,
        tx: &redb::WriteTransaction,
        scope: &LifecycleScope,
        capture: &CapturedLifecycle,
        policy: &NativePolicyInputs,
        live_only: bool,
    ) -> Result<
        (
            ControlState,
            NamespaceConfig,
            crate::decay::lifecycle::EffectiveLifecyclePolicy,
            HashMap<MemoryId, DiskRecord>,
        ),
        StorageError,
    > {
        let state = control::read_control(tx)?;
        if state.store_id != capture.generation.store_id {
            return Err(StorageError::StoreLineageMismatch);
        }
        if capture.generation.generation > state.committed_generation {
            return Err(StorageError::LifecycleConflict(
                "capture generation is in the future".into(),
            ));
        }
        let ns = namespace_state::require_namespace_active(tx, scope.namespace_id)?;
        if ns.name != scope.namespace_name {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        let schema = ns
            .schema
            .as_ref()
            .ok_or_else(|| invalid("native namespace schema missing"))?;
        let resolved = policy.resolve(&ns)?;
        if schema.lifecycle_version != capture.version
            || schema.policy_revision != capture.revision
            || policy_hash(&ns, &resolved)? != capture.policy_hash
        {
            return Err(StorageError::LifecycleConflict(
                "namespace policy changed after capture".into(),
            ));
        }
        let mut current = HashMap::new();
        {
            let table = tx.open_table(META_TABLE)?;
            for (id, token) in &capture.records {
                let value = table
                    .get(id.as_bytes().as_slice())?
                    .ok_or(StorageError::NotFound(id.into_inner()))?;
                let record = DiskRecord::from_bytes(value.value())?;
                if record.namespace_id != scope.namespace_id.get() {
                    return Err(StorageError::MutationNamespaceMismatch);
                }
                let fields = LifecycleState::capture(&record)?;
                if live_only && !fields.eligible() {
                    return Err(StorageError::LifecycleContentUnavailable);
                }
                if fields.lifecycle.record_generation != *token
                    || fields.lifecycle.lifecycle_version != capture.version
                    || fields.lifecycle.policy_revision != capture.revision
                {
                    return Err(StorageError::LifecycleConflict(format!(
                        "record {id} changed after capture"
                    )));
                }
                current.insert(*id, record);
            }
        }
        Ok((state, ns, resolved, current))
    }
    pub fn record_native_exposure(
        &self,
        ctx: &MutationContext,
        intent: &ExposureIntent,
        policy: &NativePolicyInputs,
        clock: &dyn Clock,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        let guard = self.native_guard()?;
        let (tx, receipt) = self.prepare_native(
            &guard,
            ctx,
            &intent.scope,
            &intent.capture,
            &intent.original,
            None,
            policy,
            clock,
        )?;
        self.commit_native(tx, receipt)
    }
    pub fn apply_native_feedback(
        &self,
        ctx: &MutationContext,
        intent: &FeedbackIntent,
        policy: &NativePolicyInputs,
        clock: &dyn Clock,
    ) -> Result<WriteReceipt, StorageError> {
        let guard = self.native_guard()?;
        let (tx, receipt) = self.prepare_native(
            &guard,
            ctx,
            &intent.scope,
            &intent.capture,
            &[(intent.id, AccessKind::ManualReinforcement)],
            Some(intent.quality),
            policy,
            clock,
        )?;
        self.commit_native(tx, receipt)?
            .ok_or_else(|| invalid("feedback receipt missing"))
    }
    fn commit_native(
        &self,
        tx: redb::WriteTransaction,
        receipt: Option<WriteReceipt>,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        if let Some(receipt) = &receipt {
            if !receipt.replayed {
                let committed = tx.commit();
                // A durable transaction can still be reported as an I/O error.
                // Exercise reconciliation against the actual committed database.
                #[cfg(test)]
                let committed = committed.and_then(|()| {
                    if FAIL_AFTER_NATIVE_COMMIT.with(|flag| flag.replace(false)) {
                        Err(redb::CommitError::Storage(redb::StorageError::Io(
                            std::io::Error::other("injected unknown native commit"),
                        )))
                    } else {
                        Ok(())
                    }
                });
                committed.map_err(|source| {
                    self.mark_unavailable();
                    StorageError::CommitOutcomeUnknown {
                        operation_id: receipt.operation_id,
                        source,
                    }
                })?;
            }
        }
        Ok(receipt)
    }
    // Tests can abandon the actual fully prepared transaction before commit.
    fn prepare_native(
        &self,
        publication: &MutationGuard<'_>,
        ctx: &MutationContext,
        scope: &LifecycleScope,
        capture: &CapturedLifecycle,
        original: &[(MemoryId, AccessKind)],
        quality: Option<u8>,
        policy: &NativePolicyInputs,
        clock: &dyn Clock,
    ) -> Result<(redb::WriteTransaction, Option<WriteReceipt>), StorageError> {
        publication.check(self)?;
        ctx.validate()?;
        if ctx.contract_version != 2 {
            return Err(StorageError::InvalidMutationContext(
                "native lifecycle operations require contract version 2".into(),
            ));
        }
        if ctx.scope != MutationScope::Namespace(scope.namespace_id) {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        let semantic = control::encode(&(scope, original, quality))?;
        let hash = control::request_hash(
            ctx,
            if quality.is_some() {
                "native_feedback"
            } else {
                "native_exposure"
            },
            &[&semantic],
        );
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok((tx, Some(receipt)));
        }
        let (state, _namespace, resolved, mut current) =
            self.check_native_capture(&tx, scope, capture, policy, true)?;
        if original.is_empty() {
            return Ok((tx, None));
        }
        let sampled_at = clock.now_millis();
        if chrono::DateTime::from_timestamp_millis(sampled_at).is_none() {
            return Err(StorageError::InvalidMutation(
                "unrepresentable clock sample".into(),
            ));
        }
        let feedback = quality
            .map(|quality| {
                Ok::<_, StorageError>(FeedbackEvidence {
                    quality,
                    decay_rate_multiplier: resolved
                        .decay_rate_multiplier
                        .try_into()
                        .map_err(StorageError::InvalidMutation)?,
                    permastore_threshold: resolved
                        .config
                        .permastore_threshold
                        .try_into()
                        .map_err(StorageError::InvalidMutation)?,
                })
            })
            .transpose()?;
        let mut changes = Vec::new();
        {
            let mut table = tx.open_table(META_TABLE)?;
            for &(id, kind) in original {
                let mut record = current
                    .remove(&id)
                    .ok_or_else(|| invalid("original target lacks captured precondition"))?;
                let before = LifecycleState::capture(&record)?;
                let history_before = history_hash(&record.access_history)?;
                let after = if let Some(evidence) = &feedback {
                    reinforced(&before, sampled_at, evidence)?
                } else {
                    exposed(&before, sampled_at)?
                };
                after.apply(&mut record);
                let evicted_event =
                    append_event(&mut record, after.lifecycle.lifecycle_updated_at, kind);
                let history_after = history_hash(&record.access_history)?;
                table.insert(id.as_bytes().as_slice(), record.encode()?.as_slice())?;
                changes.push(LifecycleChange {
                    memory_id: id,
                    kind,
                    before,
                    after,
                    history_before,
                    history_after,
                    evicted_event,
                });
            }
        }
        let transition = NativeLifecycleWrite {
            namespace_id: scope.namespace_id,
            lifecycle_version: capture.version,
            policy_revision: capture.revision,
            previous_generation: state.committed_generation,
            capture_generation: capture.generation,
            sampled_at,
            records: changes,
            feedback,
        };
        let outcome = if quality.is_some() {
            MutationOutcome::LifecycleFeedbackApplied { transition }
        } else {
            MutationOutcome::LifecycleExposureRecorded { transition }
        };
        let receipt = control::record_receipt(&tx, ctx, hash, outcome)?;
        Ok((tx, Some(receipt)))
    }
}

#[cfg(test)]
mod tests;
