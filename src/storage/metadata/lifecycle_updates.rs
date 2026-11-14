//! Current-layout lifecycle staging shared by legacy feedback and receipted maintenance.
use super::*;
use crate::storage::maintenance::{DecayMutationResult, DecaySkip};

enum StagedPatch {
    Changed {
        before: DiskRecord,
        after: DiskRecord,
    },
    Skipped(DecaySkip),
}
use crate::decay::lifecycle::FeedbackPlan;

#[derive(Clone, Copy)]
enum LegacyPatch<'a> {
    Feedback(&'a FeedbackPlan),
    Decay {
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
    },
}

impl MetadataStore {
    /// Patch feedback state and append its legacy event in one transaction.
    /// False means the record/policy snapshot changed; nothing was committed.
    #[cfg(test)]
    pub(crate) fn apply_legacy_feedback(
        &self,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        plan: &FeedbackPlan,
    ) -> Result<bool, StorageError> {
        self.apply_legacy_patch(id, expected, namespace, LegacyPatch::Feedback(plan))
    }

    /// Materialize strength, optionally losing exactly one retained phase.
    /// V1 Summary->Ghost clears summary only; tags and other fields are retained.
    #[cfg(test)]
    pub(crate) fn apply_legacy_decay(
        &self,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
    ) -> Result<bool, StorageError> {
        self.apply_legacy_patch(
            id,
            expected,
            namespace,
            LegacyPatch::Decay {
                phase,
                strength,
                decay_strength,
            },
        )
    }

    #[cfg(test)]
    fn apply_legacy_patch(
        &self,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        patch: LegacyPatch<'_>,
    ) -> Result<bool, StorageError> {
        let publication = self.mutation_guard()?;
        let Some((tx, record)) =
            self.prepare_legacy_patch(&publication, id, expected, namespace, patch)?
        else {
            return Ok(false);
        };
        self.commit(tx)?; // Uncertain commit poisons admission; never publish an assumed rollback.
        if record.phase != expected.phase {
            self.phase_index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .transition(
                    record.namespace_id,
                    record.vector_slot,
                    expected.phase,
                    record.phase,
                );
        }
        Ok(true)
    }

    /// Actual write preparation, shared with boundary tests that abandon the
    /// redb transaction after all fields and generation have been staged.
    #[cfg(test)]
    fn prepare_legacy_patch(
        &self,
        _publication: &std::sync::RwLockWriteGuard<'_, ()>,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        patch: LegacyPatch<'_>,
    ) -> Result<Option<(redb::WriteTransaction, DiskRecord)>, StorageError> {
        let tx = self.db.begin_write()?;
        let StagedPatch::Changed {
            before,
            after: record,
        } = self.stage_legacy_patch(&tx, id, expected, namespace, patch)?
        else {
            return Ok(None);
        };
        let state = control::advance_generation(&tx)?;
        removal_ledger::record_change(
            &tx,
            state.generation_ref(),
            &before,
            &record,
            uuid::Uuid::new_v4(),
        )?;
        Ok(Some((tx, record)))
    }
    fn stage_legacy_patch(
        &self,
        tx: &redb::WriteTransaction,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        patch: LegacyPatch<'_>,
    ) -> Result<StagedPatch, StorageError> {
        let mut record = {
            let table = tx.open_table(META_TABLE)?;
            let Some(value) = table.get(id.as_bytes().as_slice())? else {
                return Ok(StagedPatch::Skipped(DecaySkip::Stale));
            };
            DiskRecord::from_bytes(value.value())?
        };
        if record.id != *id.as_bytes()
            || record.phase == DecayPhase::Tombstone
            || !same_inputs(&record, expected)
        {
            return Ok(StagedPatch::Skipped(DecaySkip::Stale));
        }
        // Read policy in the SAME write transaction, under the publication gate.
        // Concrete namespace configuration is the v1 policy snapshot; no invented
        // policy revision or public CAS token. Global config is caller-owned/frozen.
        {
            let current = match namespace_state::require_namespace_active(
                tx,
                NamespaceId::new(record.namespace_id),
            ) {
                Ok(config) => config,
                Err(StorageError::NamespaceNotFound(_)) => {
                    return Ok(StagedPatch::Skipped(DecaySkip::Stale));
                }
                Err(error) => return Err(error),
            };
            if current.id != namespace.id
                || current.id.get() != record.namespace_id
                || serde_json::to_vec(&current)
                    .map_err(|e| StorageError::Serialize(e.to_string()))?
                    != serde_json::to_vec(namespace)
                        .map_err(|e| StorageError::Serialize(e.to_string()))?
            {
                return Ok(StagedPatch::Skipped(DecaySkip::Stale));
            }
        }
        if record.schema.is_some() || record.version != 1 || namespace.schema.is_some() {
            return Err(StorageError::InvalidMutation(
                "native lifecycle requires participating schema-v2 planner".into(),
            ));
        }
        let before = record.clone();
        match patch {
            LegacyPatch::Feedback(plan) => {
                if !patch_legacy_feedback(&mut record, plan)? {
                    return Ok(StagedPatch::Skipped(DecaySkip::Stale));
                }
            }
            LegacyPatch::Decay {
                phase,
                strength,
                decay_strength,
            } => {
                if !valid_strength(strength) || !valid_strength(decay_strength) {
                    return Err(StorageError::InvalidMutation(
                        "invalid legacy decay strength".into(),
                    ));
                }
                if record.is_permastore != 0 || namespace.decay_rate_multiplier == Some(0.0) {
                    return Ok(StagedPatch::Skipped(DecaySkip::Exempt));
                }
                if phase != record.phase {
                    match (record.phase, phase) {
                        (DecayPhase::Full, DecayPhase::Summary) if !record.summary.is_empty() => {
                            record.text_offset = 0;
                            record.text_length = 0;
                        }
                        (DecayPhase::Summary, DecayPhase::Ghost) => record.summary.clear(),
                        _ => {
                            return Err(StorageError::InvalidMutation(
                                "invalid legacy phase loss".into(),
                            ));
                        }
                    }
                }
                record.phase = phase;
                record.strength = strength;
                record.decay_strength = decay_strength;
            }
        }
        if matches!(patch, LegacyPatch::Decay { .. }) && before.encode()? == record.encode()? {
            return Ok(StagedPatch::Skipped(DecaySkip::Unchanged));
        }
        // Patch the freshly read record: edge counts, tags and fields outside
        // lifecycle are preserved, never copied back from a stale full record.
        {
            let mut table = tx.open_table(META_TABLE)?;
            table.insert(id.as_bytes().as_slice(), record.to_bytes().as_slice())?;
        }
        Ok(StagedPatch::Changed {
            before,
            after: record,
        })
    }

    pub(crate) fn decay_with_effects(
        &self,
        ctx: &MutationContext,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
        edges: &[crate::storage::PersistedEdge],
    ) -> Result<DecayMutationResult, StorageError> {
        let guard = self.mutation_guard()?;
        ctx.validate()?;
        if ctx.contract_version != 1 || ctx.scope != MutationScope::Namespace(namespace.id) {
            return Err(StorageError::InvalidMutationContext(
                "legacy maintenance requires matching namespace scope".into(),
            ));
        }
        let hash = control::request_hash(
            ctx,
            "lifecycle_materialize_v1",
            &[
                id.as_bytes(),
                &expected.encode()?,
                &control::encode(namespace)?,
                &[phase.as_u8()],
                &strength.to_bits().to_be_bytes(),
                &decay_strength.to_bits().to_be_bytes(),
            ],
        );
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok(DecayMutationResult::Committed(receipt));
        }
        let staged = self.stage_legacy_patch(
            &tx,
            id,
            expected,
            namespace,
            LegacyPatch::Decay {
                phase,
                strength,
                decay_strength,
            },
        )?;
        let StagedPatch::Changed { before, after } = staged else {
            let StagedPatch::Skipped(reason) = staged else {
                unreachable!()
            };
            return Ok(DecayMutationResult::Skipped(reason));
        };
        self.finish_record_effects(
            &guard,
            tx,
            ctx,
            hash,
            edges,
            &[],
            &[],
            false,
            false,
            effects::RecordMutation::LifecyclePatched {
                before: &before,
                after: &after,
            },
            None,
        )
        .map(|result| DecayMutationResult::Committed(result.receipt))
    }
}

/// Shared patch semantics for planned legacy feedback and receipt-bearing feedback.
pub(super) fn patch_legacy_feedback(
    record: &mut DiskRecord,
    plan: &FeedbackPlan,
) -> Result<bool, StorageError> {
    if !matches!(record.phase, DecayPhase::Full | DecayPhase::Summary)
        || (record.phase == DecayPhase::Summary && record.summary.is_empty())
    {
        return Ok(false);
    }
    if plan.phase != record.phase
        || plan.timestamp < record.created_at.max(record.last_accessed_at)
        || !plan.stability.is_finite()
        || plan.stability <= 0.0
        || !valid_strength(plan.strength)
        || (record.is_permastore != 0 && !plan.is_permastore)
    {
        return Err(StorageError::InvalidMutation(
            "invalid legacy feedback patch".into(),
        ));
    }
    record.strength = plan.strength;
    record.decay_strength = plan.strength;
    record.stability = plan.stability;
    record.is_permastore = u8::from(plan.is_permastore);
    record.last_accessed_at = plan.timestamp;
    if record.access_history.len() >= ACCESS_HISTORY_MAX {
        record.access_history.remove(0);
    }
    record.access_history.push(AccessEvent {
        timestamp: plan.timestamp,
        kind: AccessKind::ManualReinforcement,
    });
    Ok(true)
}

fn valid_strength(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn same_inputs(record: &DiskRecord, expected: &DiskRecord) -> bool {
    record.id == expected.id
        && record.namespace_id == expected.namespace_id
        && record.phase == expected.phase
        && record.created_at == expected.created_at
        && record.last_accessed_at == expected.last_accessed_at
        && record.stability.to_bits() == expected.stability.to_bits()
        && record.difficulty.to_bits() == expected.difficulty.to_bits()
        && record.is_permastore == expected.is_permastore
        && record.summary == expected.summary
        && record.text_offset == expected.text_offset
        && record.text_length == expected.text_length
        && record.vector_slot == expected.vector_slot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decay::{
        DecayConfig,
        lifecycle::{plan_feedback, resolve_policy},
    };
    use crate::model::Tag;

    fn fixture() -> (tempfile::TempDir, MetadataStore, NamespaceConfig, MemoryId) {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.id = store.create_namespace(&ns).unwrap();
        let id = MemoryId::new();
        store
            .insert(
                id,
                &DiskRecord {
                    schema: None,
                    version: DiskRecord::CURRENT_VERSION,
                    id: *id.as_bytes(),
                    namespace_id: ns.id.get(),
                    created_at: 0,
                    last_accessed_at: 0,
                    phase: DecayPhase::Full,
                    strength: 1.,
                    decay_strength: 1.,
                    stability: 1.,
                    difficulty: 5.,
                    is_permastore: 0,
                    vector_slot: 0,
                    edge_count: 0,
                    summary: "retained summary".into(),
                    tags: vec![Tag::new("retained").unwrap()],
                    access_history: Vec::new(),
                    text_offset: 42,
                    text_length: 100,
                },
            )
            .unwrap();
        (dir, store, ns, id)
    }

    fn feedback(record: &DiskRecord, ns: &NamespaceConfig) -> FeedbackPlan {
        plan_feedback(
            record,
            &resolve_policy(ns, &DecayConfig::default(), 1.).unwrap(),
            8 * 86_400_000,
            3,
        )
        .unwrap()
    }

    fn generation(store: &MetadataStore) -> u64 {
        store.control_state().unwrap().committed_generation.get()
    }

    fn assert_unchanged(
        store: &MetadataStore,
        id: MemoryId,
        before: &DiskRecord,
        generation_before: u64,
    ) {
        assert_eq!(
            store.get(id).unwrap().unwrap().to_bytes(),
            before.to_bytes()
        );
        assert_eq!(generation(store), generation_before);
        assert_eq!(store.ids_in_phase(before.phase).unwrap(), vec![id]);
    }

    #[test]
    fn feedback_commits_state_and_bounded_history_once_and_preserves_unowned_fields_on_reopen() {
        let (dir, store, ns, id) = fixture();
        store
            .update_record(&id, |r| {
                r.access_history = (0..ACCESS_HISTORY_MAX)
                    .map(|n| AccessEvent {
                        timestamp: n as i64,
                        kind: AccessKind::DirectRetrieval,
                    })
                    .collect();
                Ok(())
            })
            .unwrap();
        let before = store.get(id).unwrap().unwrap();
        let plan = feedback(&before, &ns);
        // An unrelated writer does not make a stale whole-record replacement safe.
        store.update_edge_count(id, 7).unwrap();
        let g = generation(&store);
        assert!(
            store
                .apply_legacy_feedback(id, &before, &ns, &plan)
                .unwrap()
        );
        let after = store.get(id).unwrap().unwrap();
        assert_eq!(generation(&store), g + 1);
        assert_eq!(
            (after.stability, after.strength, after.decay_strength),
            (plan.stability, plan.strength, plan.strength)
        );
        assert_eq!(after.last_accessed_at, plan.timestamp);
        assert_eq!(after.access_history.len(), ACCESS_HISTORY_MAX);
        assert_eq!(after.access_history[0].timestamp, 1);
        let event = after.access_history.last().unwrap();
        assert_eq!(
            (event.timestamp, event.kind),
            (plan.timestamp, AccessKind::ManualReinforcement)
        );
        assert_eq!(after.edge_count, 7);
        assert_eq!(
            (
                after.text_offset,
                after.text_length,
                &after.summary,
                &after.tags
            ),
            (42, 100, &before.summary, &before.tags)
        );
        drop(store);
        let reopened = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_unchanged(&reopened, id, &after, g + 1);
    }

    #[test]
    fn phase_loss_commits_strength_and_exact_v1_content_once_and_rebuilds_indexes() {
        let (dir, store, ns, id) = fixture();
        let before = store.get(id).unwrap().unwrap();
        let g = generation(&store);
        assert!(
            store
                .apply_legacy_decay(id, &before, &ns, DecayPhase::Summary, 0.6, 0.65)
                .unwrap()
        );
        let summary = store.get(id).unwrap().unwrap();
        assert_eq!(generation(&store), g + 1);
        assert_eq!((summary.text_offset, summary.text_length), (0, 0));
        assert_eq!(summary.summary, before.summary);
        assert_eq!((summary.strength, summary.decay_strength), (0.6, 0.65));
        assert!(store.ids_in_phase(DecayPhase::Full).unwrap().is_empty());
        assert_eq!(store.ids_in_phase(DecayPhase::Summary).unwrap(), vec![id]);
        assert!(
            store
                .apply_legacy_decay(id, &summary, &ns, DecayPhase::Ghost, 0.1, 0.15)
                .unwrap()
        );
        let ghost = store.get(id).unwrap().unwrap();
        assert!(ghost.summary.is_empty());
        assert_eq!((ghost.strength, ghost.decay_strength), (0.1, 0.15));
        assert_eq!(ghost.tags, before.tags); // Legacy strip_summary does NOT clear tags.
        assert_eq!(ghost.stability, before.stability);
        assert_eq!(ghost.access_history, before.access_history);
        assert_eq!(store.memories_with_tag("retained").unwrap(), vec![id]);
        assert_eq!(generation(&store), g + 2);
        drop(store);
        let reopened = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_unchanged(&reopened, id, &ghost, g + 2);
        assert!(
            reopened
                .ids_in_phase(DecayPhase::Summary)
                .unwrap()
                .is_empty()
        );
        assert_eq!(reopened.memories_with_tag("retained").unwrap(), vec![id]);
    }

    #[test]
    fn abandoned_precommit_transactions_leave_every_lifecycle_field_and_generation_unchanged() {
        // Real production preparation: drop redb's write transaction at the last
        // boundary before commit, after both the record and generation are staged.
        for kind in 0..3 {
            let (dir, store, ns, id) = fixture();
            if kind == 2 {
                let r = store.get(id).unwrap().unwrap();
                store
                    .apply_legacy_decay(id, &r, &ns, DecayPhase::Summary, 0.6, 0.6)
                    .unwrap();
            }
            let before = store.get(id).unwrap().unwrap();
            let g = generation(&store);
            let ledger_before = store.removal_ledger_export().unwrap();
            let plan = feedback(&before, &ns);
            let patch = match kind {
                0 => LegacyPatch::Feedback(&plan),
                1 => LegacyPatch::Decay {
                    phase: DecayPhase::Summary,
                    strength: 0.6,
                    decay_strength: 0.6,
                },
                _ => LegacyPatch::Decay {
                    phase: DecayPhase::Ghost,
                    strength: 0.1,
                    decay_strength: 0.1,
                },
            };
            let guard = store.mutation_guard().unwrap();
            let (tx, staged) = store
                .prepare_legacy_patch(&guard, id, &before, &ns, patch)
                .unwrap()
                .unwrap();
            assert_ne!(staged.to_bytes(), before.to_bytes());
            assert_eq!(
                control::read_control(&tx)
                    .unwrap()
                    .committed_generation
                    .get(),
                g + 1
            );
            drop(tx);
            drop(guard);
            assert_unchanged(&store, id, &before, g);
            assert_eq!(store.removal_ledger_export().unwrap(), ledger_before);
            drop(store);
            let reopened = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
            assert_unchanged(&reopened, id, &before, g);
            assert_eq!(reopened.removal_ledger_export().unwrap(), ledger_before);
        }
    }

    #[test]
    fn real_precommit_generation_failure_rolls_back_feedback_and_both_phase_losses() {
        for kind in 0..3 {
            let (dir, store, ns, id) = fixture();
            if kind == 2 {
                let r = store.get(id).unwrap().unwrap();
                store
                    .apply_legacy_decay(id, &r, &ns, DecayPhase::Summary, 0.6, 0.6)
                    .unwrap();
            }
            let before = store.get(id).unwrap().unwrap();
            let mut state = store.control_state().unwrap();
            state.committed_generation = control::Generation::MAX;
            let guard = store.mutation_guard().unwrap();
            let tx = store.db.begin_write().unwrap();
            tx.open_table(control::CONTROL)
                .unwrap()
                .insert("state", control::encode(&state).unwrap().as_slice())
                .unwrap();
            tx.commit().unwrap();
            drop(guard);
            // advance_generation fails AFTER the patched record is inserted into
            // the real transaction. No mocked backend or artificial error flag.
            let result = match kind {
                0 => store.apply_legacy_feedback(id, &before, &ns, &feedback(&before, &ns)),
                1 => store.apply_legacy_decay(id, &before, &ns, DecayPhase::Summary, 0.6, 0.6),
                _ => store.apply_legacy_decay(id, &before, &ns, DecayPhase::Ghost, 0.1, 0.1),
            };
            assert!(matches!(result, Err(StorageError::GenerationExhausted)));
            assert_unchanged(&store, id, &before, control::Generation::MAX.get());
            drop(store);
            let reopened = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
            assert_unchanged(&reopened, id, &before, control::Generation::MAX.get());
        }
    }

    #[test]
    fn stale_record_and_policy_are_checked_inside_final_transaction() {
        let (_dir, store, mut ns, id) = fixture();
        let before = store.get(id).unwrap().unwrap();
        let plan = feedback(&before, &ns);
        store
            .update_access(id, 100, AccessKind::DirectRetrieval)
            .unwrap();
        let current = store.get(id).unwrap().unwrap();
        let g = generation(&store);
        assert!(
            !store
                .apply_legacy_feedback(id, &before, &ns, &plan)
                .unwrap()
        );
        assert!(
            !store
                .apply_legacy_decay(id, &before, &ns, DecayPhase::Summary, 0.6, 0.6)
                .unwrap()
        );
        assert_unchanged(&store, id, &current, g);
        let old_policy = ns.clone();
        ns.decay_rate_multiplier = Some(0.0);
        let guard = store.mutation_guard().unwrap();
        let tx = store.db.begin_write().unwrap();
        tx.open_table(NAMESPACE_TABLE)
            .unwrap()
            .insert(ns.id.get(), serde_json::to_vec(&ns).unwrap().as_slice())
            .unwrap();
        control::advance_generation(&tx).unwrap();
        tx.commit().unwrap();
        drop(guard);
        let g = generation(&store);
        assert!(
            !store
                .apply_legacy_feedback(id, &current, &old_policy, &feedback(&current, &old_policy))
                .unwrap()
        );
        assert!(
            !store
                .apply_legacy_decay(id, &current, &old_policy, DecayPhase::Summary, 0.6, 0.6)
                .unwrap()
        );
        assert!(
            !store
                .apply_legacy_decay(id, &current, &ns, DecayPhase::Summary, 0.6, 0.6)
                .unwrap()
        );
        assert_unchanged(&store, id, &current, g);
    }

    #[test]
    fn terminal_records_and_illegal_promotions_cannot_be_resurrected() {
        let (dir, store, ns, id) = fixture();
        let before = store.get(id).unwrap().unwrap();
        let g = generation(&store);
        assert!(
            store
                .apply_legacy_decay(id, &before, &ns, DecayPhase::Ghost, 0.1, 0.1)
                .is_err()
        );
        assert_unchanged(&store, id, &before, g);
        store
            .apply_legacy_decay(id, &before, &ns, DecayPhase::Summary, 0.6, 0.6)
            .unwrap();
        let summary = store.get(id).unwrap().unwrap();
        let g = generation(&store);
        assert!(
            store
                .apply_legacy_decay(id, &summary, &ns, DecayPhase::Full, 1., 1.)
                .is_err()
        );
        assert_unchanged(&store, id, &summary, g);
        store
            .apply_legacy_decay(id, &summary, &ns, DecayPhase::Ghost, 0.1, 0.1)
            .unwrap();
        let ghost = store.get(id).unwrap().unwrap();
        let g = generation(&store);
        assert!(
            !store
                .apply_legacy_feedback(id, &ghost, &ns, &feedback(&before, &ns))
                .unwrap()
        );
        assert_unchanged(&store, id, &ghost, g);
        store.tombstone(id).unwrap();
        let tombstone = store.get(id).unwrap().unwrap();
        let g = generation(&store);
        assert!(
            !store
                .apply_legacy_feedback(id, &before, &ns, &feedback(&before, &ns))
                .unwrap()
        );
        assert!(
            !store
                .apply_legacy_decay(id, &before, &ns, DecayPhase::Summary, 0.6, 0.6)
                .unwrap()
        );
        assert_unchanged(&store, id, &tombstone, g);
        drop(store);
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_unchanged(&store, id, &tombstone, g);
        store.delete(id).unwrap();
        let g = generation(&store);
        assert!(
            !store
                .apply_legacy_feedback(id, &before, &ns, &feedback(&before, &ns))
                .unwrap()
        );
        assert!(
            !store
                .apply_legacy_decay(id, &before, &ns, DecayPhase::Summary, 0.6, 0.6)
                .unwrap()
        );
        assert!(store.get(id).unwrap().is_none());
        assert_eq!(generation(&store), g);
    }
    #[test]
    fn phase_publication_holds_admission_after_atomic_content_commit() {
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};
        let (_dir, store, ns, id) = fixture();
        let expected = store.get(id).unwrap().unwrap();
        let store = Arc::new(store);
        // Pause after commit, before bitmap publication. Lock order remains
        // admission -> redb -> phase index; no graph lock inside metadata.
        let bitmap = store.phase_index.write().unwrap();
        let writer_store = store.clone();
        let writer = std::thread::spawn(move || {
            writer_store
                .apply_legacy_decay(id, &expected, &ns, DecayPhase::Summary, 0.6, 0.6)
                .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let record = store.get(id).unwrap().unwrap();
            if record.phase == DecayPhase::Summary {
                assert_eq!((record.text_offset, record.text_length), (0, 0));
                assert_eq!((record.strength, record.decay_strength), (0.6, 0.6));
                break;
            }
            assert!(Instant::now() < deadline, "writer did not commit");
            std::thread::yield_now();
        }
        let (write_tx, write_rx) = mpsc::channel();
        let second_store = store.clone();
        let second = std::thread::spawn(move || {
            second_store
                .update_record(&id, |r| {
                    write_tx.send(()).unwrap();
                    r.edge_count = 7;
                    Ok(())
                })
                .unwrap();
        });
        let (read_tx, read_rx) = mpsc::channel();
        let reader_store = store.clone();
        let reader = std::thread::spawn(move || {
            read_tx
                .send(reader_store.ids_in_phase(DecayPhase::Summary).unwrap())
                .unwrap();
        });
        let write_blocked = write_rx.recv_timeout(Duration::from_millis(100)).is_err();
        let read_blocked = read_rx.recv_timeout(Duration::from_millis(100)).is_err();
        drop(bitmap);
        assert!(writer.join().unwrap());
        second.join().unwrap();
        reader.join().unwrap();
        assert!(write_blocked && read_blocked);
        assert_eq!(
            read_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            vec![id]
        );
        assert_eq!(store.get(id).unwrap().unwrap().edge_count, 7);
        assert!(store.ids_in_phase(DecayPhase::Full).unwrap().is_empty());
    }
}
