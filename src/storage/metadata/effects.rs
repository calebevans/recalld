//! Durable metadata/edge effects. Canonical application and derived publication
//! are separate acknowledgements; a committed operation is never rolled back.
use super::*;
use crate::model::EdgeType;
use crate::storage::edges::PersistedEdge;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_AFTER_RECORD_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Ephemeral inputs only; receipts and effects never retain stripped content.
pub(super) enum RecordMutation<'a> {
    GraphOnly,
    PurgeChunk(&'a crate::storage::purge::PurgeProgress),
    Inserted(&'a DiskRecord, u32),
    Tombstoned {
        record: &'a DiskRecord,
        old_phase: DecayPhase,
    },
}

pub(crate) const SCHEMA: TableDefinition<&str, u32> = TableDefinition::new("effects_schema");
pub(crate) const EFFECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mutation_effects");

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VectorRelease {
    pub namespace_id: NamespaceId,
    pub slot: u32,
    pub memory_id: MemoryId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingEffects {
    pub version: u32,
    pub receipt: WriteReceipt,
    pub scope: MutationScope,
    pub puts: Vec<PersistedEdge>,
    pub deletes: Vec<PersistedEdge>,
    pub releases: Vec<VectorRelease>,
    pub invalidated_ids: Vec<MemoryId>,
    pub removed_ids: Vec<MemoryId>,
    pub peer_counts: Vec<(MemoryId, u16)>,
    pub canonical_applied: bool,
}

/// Result data for the caller's graph/cache publication. Removed records are
/// returned in memory only; durable effects/receipts never retain their content.
#[derive(Debug)]
pub(crate) struct EffectResult {
    pub receipt: WriteReceipt,
    pub peer_counts: Vec<(MemoryId, u16)>,
    pub removed: Vec<(MemoryId, DiskRecord)>,
    pub edges_removed: usize,
    pub edges_put: usize,
}

pub(crate) fn initialize(tx: &redb::WriteTransaction) -> Result<(), StorageError> {
    tx.open_table(SCHEMA)?.insert("version", 1)?;
    tx.open_table(EFFECTS)?;
    Ok(())
}

pub(crate) fn validate_existing(
    tx: &redb::ReadTransaction,
) -> Result<Vec<PendingEffects>, StorageError> {
    use redb::{MultimapTableHandle, TableHandle};
    let reserved = [SCHEMA.name(), EFFECTS.name()];
    if tx
        .list_multimap_tables()?
        .any(|t| reserved.contains(&t.name()))
    {
        return Err(StorageError::InvalidControl(
            "effects schema table is a multimap".into(),
        ));
    }
    let names: Vec<_> = tx.list_tables()?.map(|t| t.name().to_owned()).collect();
    let present = reserved.map(|n| names.iter().any(|s| s == n));
    if !present.iter().any(|p| *p) {
        if names.iter().any(|n| n == control::RECEIPTS.name()) {
            for entry in tx.open_table(control::RECEIPTS)?.iter()? {
                let (_, value) = entry?;
                if matches!(
                    control::validate_receipt(value.value())?.receipt.outcome,
                    MutationOutcome::GraphChanged { .. }
                        | MutationOutcome::MemoryInserted { .. }
                        | MutationOutcome::MemoryTombstoned { .. }
                        | MutationOutcome::NamespacePurgeStarted { .. }
                        | MutationOutcome::NamespacePurgeAdvanced { .. }
                ) {
                    return Err(StorageError::InvalidControl(
                        "effects schema missing from adopted store".into(),
                    ));
                }
            }
        }
        return Ok(Vec::new());
    }
    if !present.iter().all(|p| *p) {
        return Err(StorageError::InvalidControl(
            "incomplete effects schema".into(),
        ));
    }
    let schema = tx.open_table(SCHEMA)?;
    if schema.len()? != 1 || schema.get("version")?.is_none_or(|v| v.value() != 1) {
        return Err(StorageError::InvalidControl(
            "unsupported effects schema".into(),
        ));
    }
    let state = control::ControlState::decode(
        tx.open_table(control::CONTROL)?
            .get("state")?
            .ok_or_else(|| StorageError::InvalidControl("effects without control state".into()))?
            .value(),
    )?;
    let receipts = tx.open_table(control::RECEIPTS)?;
    let table = tx.open_table(EFFECTS)?;
    let mut effects = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        let effect: PendingEffects = control::decode(value.value())?;
        if effect.version != 1
            || key.value() != effect.receipt.operation_id.as_bytes()
            || effect.receipt.store_id != state.store_id
            || effect.receipt.replayed
            || effect.receipt.committed_generation > state.committed_generation
        {
            return Err(StorageError::InvalidControl(
                "invalid effects identity/version".into(),
            ));
        }
        let receipt_key = control::scoped_key(
            state.store_id,
            effect.scope,
            effect.receipt.operation_id.as_bytes(),
        );
        let stored = receipts
            .get(receipt_key.as_slice())?
            .ok_or_else(|| StorageError::InvalidControl("effects without receipt".into()))?;
        if control::validate_receipt(stored.value())?.receipt != effect.receipt {
            return Err(StorageError::InvalidControl(
                "effects receipt mismatch".into(),
            ));
        }
        match effect.receipt.outcome {
            MutationOutcome::MemoryTombstoned { memory_id, status }
                if matches!(effect.scope, MutationScope::Namespace(_))
                    && effect.puts.is_empty()
                    && effect.deletes.is_empty()
                    && effect.removed_ids.is_empty()
                    && effect.peer_counts.is_empty()
                    && effect.invalidated_ids == [memory_id]
                    && match status {
                        crate::storage::tombstone::TombstoneStatus::Tombstoned => {
                            effect.releases.len() == 1
                                && effect.releases.iter().all(|r| {
                                    r.memory_id == memory_id
                                        && effect.scope == MutationScope::Namespace(r.namespace_id)
                                })
                        }
                        crate::storage::tombstone::TombstoneStatus::AlreadyTombstoned => {
                            effect.releases.is_empty()
                        }
                    } => {}
            MutationOutcome::MemoryInserted {
                memory_id,
                edges_put,
                ..
            } if matches!(effect.scope, MutationScope::Namespace(_))
                && edges_put == effect.puts.len()
                && effect.deletes.is_empty()
                && effect.removed_ids.is_empty()
                && effect.releases.is_empty()
                && effect.invalidated_ids.contains(&memory_id) => {}
            MutationOutcome::NamespacePurgeAdvanced {
                stage,
                removed,
                edges_removed,
                ..
            } if effect.puts.is_empty()
                && effect.releases.is_empty()
                && removed == effect.removed_ids.len() as u64
                && edges_removed == effect.deletes.len() as u64
                && (stage == crate::storage::purge::PurgeStage::Deleting
                    || (removed == 0
                        && edges_removed == 0
                        && effect.invalidated_ids.is_empty()
                        && effect.peer_counts.is_empty())) => {}
            MutationOutcome::GraphChanged {
                edges_put,
                edges_removed,
                memories_removed,
            } if edges_put == effect.puts.len()
                && edges_removed == effect.deletes.len()
                && memories_removed == effect.removed_ids.len() => {}
            _ => {
                return Err(StorageError::InvalidControl(
                    "effects outcome disagrees with actions".into(),
                ));
            }
        }
        if effect
            .releases
            .iter()
            .any(|r| !effect.removed_ids.contains(&r.memory_id) && !matches!(effect.receipt.outcome, MutationOutcome::MemoryTombstoned { memory_id, .. } if memory_id == r.memory_id))
        {
            return Err(StorageError::InvalidControl(
                "release without removed owner".into(),
            ));
        }
        // Deletes may clean up legacy invalid edges; only new puts must
        // satisfy current admission rules (including no self edge).
        for edge in &effect.puts {
            if !edge.weight.is_finite()
                || !(0.0..=1.0).contains(&edge.weight)
                || edge.source == edge.target
            {
                return Err(StorageError::InvalidControl("invalid effect edge".into()));
            }
        }
        effects.push(effect);
    }
    effects.sort_by_key(|e| e.receipt.committed_generation);
    Ok(effects)
}

impl MetadataStore {
    pub(crate) fn effects_stamp(&self) -> Result<u64, StorageError> {
        let before = self.effects_epoch.load(Ordering::SeqCst);
        self.ensure_available()?;
        if self.derived_dirty.load(Ordering::SeqCst) {
            return Err(StorageError::RecoveryRequired(
                "derived graph effects pending publication".into(),
            ));
        }
        if before != self.effects_epoch.load(Ordering::SeqCst) {
            return Err(StorageError::RecoveryRequired(
                "effects changed during admission".into(),
            ));
        }
        Ok(before)
    }

    pub(crate) fn needs_effects_rebuild(&self) -> bool {
        self.derived_dirty.load(Ordering::SeqCst)
    }

    /// Called after every derived index is rebuilt, either during startup or
    /// under the online coordinator's publication/index/storage guards.
    pub(crate) fn acknowledge_rebuilt_effects(&self) -> Result<(), StorageError> {
        self.acknowledge_rebuilt_effects_with_work(None)
    }

    pub(crate) fn acknowledge_rebuilt_effects_with_work(
        &self,
        parent: Option<&WorkTicket>,
    ) -> Result<(), StorageError> {
        let work = self.work_tracker.admit(parent)?;
        let result = (|| {
            let ids: Vec<_> = self
                .pending_effects()?
                .iter()
                .map(|e| e.receipt.operation_id)
                .collect();
            for id in ids {
                self.acknowledge_derived_effects_with_work(id, Some(&work))?;
            }
            Ok(())
        })();
        self.finish_work(work);
        result
    }

    pub(crate) fn pending_effects(&self) -> Result<Vec<PendingEffects>, StorageError> {
        validate_existing(&self.db.begin_read()?)
    }

    /// Caller holds the publication write gate, including the edge snapshot.
    pub(crate) fn commit_graph_effects(
        &self,
        _guard: &MutationGuard<'_>,
        namespace: NamespaceId,
        existing_edges: &[PersistedEdge],
        puts: &[PersistedEdge],
        incident_ids: &[MemoryId],
        delete_records: bool,
        release_vectors: bool,
    ) -> Result<EffectResult, StorageError> {
        _guard.check(self)?;
        self.ensure_available()?;
        let tx = self.db.begin_write()?;
        let ctx = MutationContext::unkeyed(namespace);
        let request = control::encode(&(puts, incident_ids, delete_records, release_vectors))?;
        let hash = control::request_hash(&ctx, "graph_effects", &[&request]);
        self.finish_record_effects(
            _guard,
            tx,
            &ctx,
            hash,
            existing_edges,
            puts,
            incident_ids,
            delete_records,
            release_vectors,
            RecordMutation::GraphOnly,
        )
    }

    /// Shared transaction tail for graph mutations and canonical insertion.
    /// Insertion has already staged its new row/indexes in this transaction.
    pub(super) fn finish_record_effects(
        &self,
        _guard: &MutationGuard<'_>,
        tx: redb::WriteTransaction,
        ctx: &MutationContext,
        hash: [u8; 32],
        existing_edges: &[PersistedEdge],
        puts: &[PersistedEdge],
        incident_ids: &[MemoryId],
        delete_records: bool,
        release_vectors: bool,
        mutation: RecordMutation<'_>,
    ) -> Result<EffectResult, StorageError> {
        _guard.check(self)?;
        let MutationScope::Namespace(namespace) = ctx.scope else {
            return Err(StorageError::InvalidMutationContext(
                "record effects require namespace scope".into(),
            ));
        };
        if tx
            .open_table(NAMESPACE_TABLE)?
            .get(namespace.get())?
            .is_none()
        {
            return Err(StorageError::NamespaceNotFound(namespace.get()));
        }

        let mut removed = Vec::new();
        let mut releases = Vec::new();
        let mut invalidated: std::collections::HashSet<_> = incident_ids.iter().copied().collect();
        match &mutation {
            RecordMutation::Inserted(record, _) | RecordMutation::Tombstoned { record, .. } => {
                invalidated.insert(MemoryId::from_bytes(record.id));
            }
            RecordMutation::GraphOnly | RecordMutation::PurgeChunk(_) => {}
        }
        if let RecordMutation::Tombstoned { record, old_phase } = &mutation {
            if *old_phase != DecayPhase::Tombstone {
                releases.push(VectorRelease {
                    namespace_id: namespace,
                    slot: record.vector_slot,
                    memory_id: MemoryId::from_bytes(record.id),
                });
            }
        }
        let mut final_edges: Vec<_> = existing_edges
            .iter()
            .filter(|e| !incident_ids.contains(&e.source) && !incident_ids.contains(&e.target))
            .cloned()
            .collect();
        let deletes: Vec<_> = existing_edges
            .iter()
            .filter(|e| incident_ids.contains(&e.source) || incident_ids.contains(&e.target))
            .cloned()
            .collect();
        let mut affected: std::collections::HashSet<_> = deletes.iter().map(|e| e.source).collect();
        let mut actual_puts = Vec::new();
        let mut peer_counts = Vec::new();
        {
            let mut records = tx.open_table(META_TABLE)?;
            for edge in puts {
                if !edge.weight.is_finite()
                    || !(0.0..=1.0).contains(&edge.weight)
                    || edge.source == edge.target
                {
                    return Err(StorageError::InvalidMutation(
                        "invalid edge weight or self edge".into(),
                    ));
                }
                for id in [edge.source, edge.target] {
                    let record = DiskRecord::from_bytes(
                        records
                            .get(id.as_bytes().as_slice())?
                            .ok_or(StorageError::NotFound(id.into_inner()))?
                            .value(),
                    )?;
                    // V1 structural compatibility: correcting a retained old
                    // identity and naming a retained parent are allowed. New
                    // children/replacements and associative endpoints must live.
                    let retained_structural = match edge.edge_type {
                        EdgeType::Supersedes => id == edge.target || delete_records,
                        EdgeType::ParentChild => id == edge.source || delete_records,
                        _ => false,
                    };
                    if record.id != *id.as_bytes()
                        || record.namespace_id != namespace.get()
                        || (matches!(record.phase, DecayPhase::Ghost | DecayPhase::Tombstone)
                            && !retained_structural)
                        || (delete_records && incident_ids.contains(&id))
                    {
                        return Err(StorageError::InvalidMutation(
                            "edge endpoint is unavailable or in another namespace".into(),
                        ));
                    }
                }
                if edge.edge_type == EdgeType::Supersedes {
                    if final_edges.iter().any(|old| {
                        old.edge_type == EdgeType::Supersedes
                            && old.target == edge.target
                            && old.source != edge.source
                    }) {
                        return Err(StorageError::EdgeConflict("supersedes branch".into()));
                    }
                    let mut seen = std::collections::HashSet::new();
                    let mut visit = vec![edge.target];
                    while let Some(id) = visit.pop() {
                        if id == edge.source {
                            return Err(StorageError::EdgeConflict("supersedes cycle".into()));
                        }
                        if seen.insert(id) {
                            visit.extend(
                                final_edges
                                    .iter()
                                    .filter(|e| {
                                        e.edge_type == EdgeType::Supersedes && e.source == id
                                    })
                                    .map(|e| e.target),
                            );
                        }
                    }
                }
                if let Some(old) = final_edges.iter_mut().find(|old| same_edge(old, edge)) {
                    if old == edge {
                        continue;
                    }
                    *old = edge.clone();
                } else {
                    final_edges.push(edge.clone());
                }
                affected.insert(edge.source);
                invalidated.insert(edge.source);
                invalidated.insert(edge.target);
                actual_puts.retain(|old| !same_edge(old, edge));
                actual_puts.push(edge.clone());
            }
            if delete_records {
                let mut tags = tx.open_multimap_table(TAG_INDEX)?;
                let mut namespaces = tx.open_multimap_table(NAMESPACE_INDEX)?;
                for id in incident_ids {
                    let Some(bytes) = records.remove(id.as_bytes().as_slice())? else {
                        continue;
                    };
                    let record = DiskRecord::from_bytes(bytes.value())?;
                    if record.namespace_id != namespace.get() {
                        return Err(StorageError::MutationNamespaceMismatch);
                    }
                    for tag in &record.tags {
                        tags.remove(tag.as_str(), id.as_bytes().as_slice())?;
                    }
                    namespaces.remove(record.namespace_id, id.as_bytes().as_slice())?;
                    if release_vectors && record.phase != DecayPhase::Tombstone {
                        releases.push(VectorRelease {
                            namespace_id: namespace,
                            slot: record.vector_slot,
                            memory_id: *id,
                        });
                    }
                    removed.push((*id, record));
                }
            }
            for id in affected {
                let Some(bytes) = records.get(id.as_bytes().as_slice())? else {
                    continue;
                };
                let mut record = DiskRecord::from_bytes(bytes.value())?;
                drop(bytes);
                let count = u16::try_from(final_edges.iter().filter(|e| e.source == id).count())
                    .map_err(|_| {
                        StorageError::InvalidMutation(
                            "outgoing edge count exceeds record capacity".into(),
                        )
                    })?;
                record.edge_count = count;
                records.insert(id.as_bytes().as_slice(), record.to_bytes().as_slice())?;
                peer_counts.push((id, count));
                invalidated.insert(id);
            }
        }
        peer_counts.sort_by_key(|(id, _)| *id);
        let mut invalidated_ids: Vec<_> = invalidated.into_iter().collect();
        invalidated_ids.sort();
        let outcome = match &mutation {
            RecordMutation::Inserted(record, embedding_dim) => MutationOutcome::MemoryInserted {
                memory_id: MemoryId::from_bytes(record.id),
                created_at: record.created_at,
                initial_stability_bits: record.stability.to_bits(),
                embedding_dim: *embedding_dim,
                edges_put: actual_puts.len(),
            },
            RecordMutation::Tombstoned { record, old_phase } => MutationOutcome::MemoryTombstoned {
                memory_id: MemoryId::from_bytes(record.id),
                status: if *old_phase == DecayPhase::Tombstone {
                    crate::storage::tombstone::TombstoneStatus::AlreadyTombstoned
                } else {
                    crate::storage::tombstone::TombstoneStatus::Tombstoned
                },
            },
            RecordMutation::GraphOnly | RecordMutation::PurgeChunk(_) => {
                MutationOutcome::GraphChanged {
                    edges_put: actual_puts.len(),
                    edges_removed: deletes.len(),
                    memories_removed: removed.len(),
                }
            }
        };
        let receipt = if let RecordMutation::PurgeChunk(progress) = &mutation {
            super::purge::record_next(
                &tx,
                progress,
                crate::storage::purge::PurgeStage::Deleting,
                &removed,
                deletes.len(),
            )?
        } else {
            control::record_receipt(&tx, ctx, hash, outcome)?
        };
        if let RecordMutation::Tombstoned { record, old_phase } = &mutation {
            if *old_phase != DecayPhase::Tombstone {
                super::removal_ledger::record_removal(
                    &tx,
                    crate::storage::control::GenerationRef {
                        store_id: receipt.store_id,
                        generation: receipt.committed_generation,
                    },
                    record.namespace_id,
                    MemoryId::from_bytes(record.id),
                    Some(super::removal_ledger::TerminalKind::Tombstone),
                    super::removal_ledger::ALL,
                    receipt.operation_id,
                )?;
            }
        }
        // Removal evidence and effect receipt share this canonical transaction.
        for (id, record) in &removed {
            super::removal_ledger::record_removal(
                &tx,
                crate::storage::control::GenerationRef {
                    store_id: receipt.store_id,
                    generation: receipt.committed_generation,
                },
                record.namespace_id,
                *id,
                Some(super::removal_ledger::TerminalKind::HardDelete),
                super::removal_ledger::ALL,
                receipt.operation_id,
            )?;
        }
        let pending = PendingEffects {
            version: 1,
            receipt: receipt.clone(),
            scope: ctx.scope,
            puts: actual_puts,
            deletes,
            releases,
            removed_ids: removed.iter().map(|(id, _)| *id).collect(),
            invalidated_ids,
            peer_counts: peer_counts.clone(),
            canonical_applied: false,
        };
        tx.open_table(EFFECTS)?.insert(
            receipt.operation_id.as_bytes().as_slice(),
            control::encode(&pending)?.as_slice(),
        )?;
        crash_point("before_metadata_commit");
        // Publish dirty before advancing the epoch. Admission samples the
        // epoch twice; it cannot accept a new stamp with an old clean flag.
        let next = self
            .effects_epoch
            .load(Ordering::SeqCst)
            .checked_add(1)
            .ok_or(StorageError::GenerationExhausted)?;
        self.derived_dirty.store(true, Ordering::SeqCst);
        self.canonical_pending.store(true, Ordering::SeqCst);
        self.effects_epoch.store(next, Ordering::SeqCst);
        let committed = tx.commit();
        // Synthetic I/O ambiguity: the commit succeeded, but the caller must
        // not assume failure means its staged bytes can be reclaimed.
        #[cfg(test)]
        let committed = committed.and_then(|()| {
            if FAIL_AFTER_RECORD_COMMIT.with(|flag| flag.replace(false)) {
                Err(redb::CommitError::Storage(redb::StorageError::Io(
                    std::io::Error::other("injected unknown record commit"),
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
        crash_point("after_metadata_commit");
        let mut phases = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
        if let RecordMutation::Inserted(record, _) = &mutation {
            phases.transition(
                record.namespace_id,
                record.vector_slot,
                DecayPhase::Full,
                record.phase,
            );
        }
        if let RecordMutation::Tombstoned { record, old_phase } = &mutation {
            if *old_phase != DecayPhase::Tombstone {
                phases.remove(record.namespace_id, record.vector_slot, *old_phase);
            }
        }
        for (_, record) in &removed {
            if record.phase != DecayPhase::Tombstone {
                phases.remove(record.namespace_id, record.vector_slot, record.phase);
            }
        }
        Ok(EffectResult {
            receipt,
            peer_counts,
            removed,
            edges_removed: pending.deletes.len(),
            edges_put: pending.puts.len(),
        })
    }

    pub(crate) fn acknowledge_canonical_effects(
        &self,
        guard: &MutationGuard<'_>,
        effect: &PendingEffects,
    ) -> Result<(), StorageError> {
        guard.check(self)?;
        let mut completed = effect.clone();
        completed.canonical_applied = true;
        let tx = self.db.begin_write()?;
        tx.open_table(EFFECTS)?.insert(
            completed.receipt.operation_id.as_bytes().as_slice(),
            control::encode(&completed)?.as_slice(),
        )?;
        self.commit(tx)?;
        Ok(())
    }

    pub(crate) fn acknowledge_derived_effects(
        &self,
        operation_id: uuid::Uuid,
    ) -> Result<(), StorageError> {
        self.acknowledge_derived_effects_with_work(operation_id, None)
    }

    pub(crate) fn acknowledge_derived_effects_with_work(
        &self,
        operation_id: uuid::Uuid,
        parent: Option<&WorkTicket>,
    ) -> Result<(), StorageError> {
        let _publication = self.mutation_guard_with_work(parent)?;
        let tx = self.db.begin_write()?;
        {
            let mut effects = tx.open_table(EFFECTS)?;
            if let Some(value) = effects.get(operation_id.as_bytes().as_slice())? {
                let effect: PendingEffects = control::decode(value.value())?;
                if !effect.canonical_applied {
                    return Err(StorageError::RecoveryRequired(
                        "canonical effects pending".into(),
                    ));
                }
            }
            effects.remove(operation_id.as_bytes().as_slice())?;
        }
        self.commit(tx)?;
        self.derived_dirty
            .store(!self.pending_effects()?.is_empty(), Ordering::SeqCst);
        Ok(())
    }
}

fn same_edge(a: &PersistedEdge, b: &PersistedEdge) -> bool {
    a.source == b.source && a.target == b.target && a.edge_type == b.edge_type
}

pub(crate) fn crash_point(_point: &str) {
    #[cfg(test)]
    if std::env::var("RECALLD_TEST_EFFECT_CRASH").as_deref() == Ok(_point) {
        std::process::exit(91);
    }
}

pub(crate) fn validate_plan(
    effect: &PendingEffects,
    records: &[(MemoryId, DiskRecord)],
) -> Result<(), StorageError> {
    if effect
        .removed_ids
        .iter()
        .any(|id| records.iter().any(|(key, _)| key == id))
    {
        return Err(StorageError::InvalidControl(
            "removed effect ID still exists".into(),
        ));
    }
    if let MutationOutcome::MemoryInserted {
        memory_id,
        created_at,
        ..
    } = effect.receipt.outcome
    {
        if records
            .iter()
            .find(|(id, _)| *id == memory_id)
            .is_none_or(|(_, record)| {
                record.phase != DecayPhase::Full
                    || record.created_at != created_at
                    || effect.scope
                        != MutationScope::Namespace(NamespaceId::new(record.namespace_id))
            })
        {
            return Err(StorageError::InvalidControl(
                "pending insertion disagrees with canonical record".into(),
            ));
        }
    }
    if let MutationOutcome::MemoryTombstoned { memory_id, .. } = effect.receipt.outcome {
        if records
            .iter()
            .find(|(id, _)| *id == memory_id)
            .is_none_or(|(_, record)| {
                record.phase != DecayPhase::Tombstone
                    || !record.summary.is_empty()
                    || !record.tags.is_empty()
                    || record.text_offset != 0
                    || record.text_length != 0
                    || effect.scope
                        != MutationScope::Namespace(NamespaceId::new(record.namespace_id))
                    || effect.releases.iter().any(|r| r.slot != record.vector_slot)
            })
        {
            return Err(StorageError::InvalidControl(
                "pending tombstone disagrees with canonical record".into(),
            ));
        }
    }
    for release in &effect.releases {
        if records.iter().any(|(id, r)| {
            (*id == release.memory_id
                && !matches!(effect.receipt.outcome,
                MutationOutcome::MemoryTombstoned { memory_id, .. } if memory_id == *id))
                || (r.phase != DecayPhase::Tombstone
                    && r.namespace_id == release.namespace_id.get()
                    && r.vector_slot == release.slot)
        }) {
            return Err(StorageError::OwnershipMismatch(
                "effect attempts to release an owned vector".into(),
            ));
        }
    }
    for (id, count) in &effect.peer_counts {
        if records
            .iter()
            .find(|(key, _)| key == id)
            .is_none_or(|(_, r)| r.edge_count != *count)
        {
            return Err(StorageError::InvalidControl(
                "effect peer count disagrees with canonical metadata".into(),
            ));
        }
    }
    for edge in &effect.puts {
        let MutationScope::Namespace(namespace) = effect.scope else {
            return Err(StorageError::InvalidControl(
                "graph effect has catalog scope".into(),
            ));
        };
        if [edge.source, edge.target].iter().any(|id| {
            records
                .iter()
                .find(|(key, _)| key == id)
                .is_none_or(|(_, record)| record.namespace_id != namespace.get())
        }) {
            return Err(StorageError::InvalidControl(
                "effect put has missing or foreign endpoint".into(),
            ));
        }
    }
    Ok(())
}
