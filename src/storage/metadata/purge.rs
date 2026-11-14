//! Purge receipts and mutable progress share the record-removal transaction.
use super::*;
use crate::storage::purge::{PurgeProgress, PurgeStage};
use redb::{MultimapTableHandle, TableHandle};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) const SCHEMA: TableDefinition<&str, u32> =
    TableDefinition::new("namespace_purge_schema");
pub(crate) const PROGRESS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("namespace_purge_progress");
pub(crate) const CHUNK_LIMIT: usize = 1000;

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidControl(format!("namespace purge: {message}"))
}
fn pending() -> StorageError {
    StorageError::RecoveryRequired(
        "namespace purge pending; resume canonical deletion before admission".into(),
    )
}
fn is_purge(outcome: &MutationOutcome) -> bool {
    matches!(
        outcome,
        MutationOutcome::NamespacePurgeStarted { .. }
            | MutationOutcome::NamespacePurgeAdvanced { .. }
    )
}

/// Pure validation, also called before any writable source open. Unknown tables
/// are not adoption; unknown versions and inconsistent history are corruption.
pub(crate) fn validate_existing(
    tx: &redb::ReadTransaction,
    state: Option<&ControlState>,
) -> Result<Vec<PurgeProgress>, StorageError> {
    let reserved = [SCHEMA.name(), PROGRESS.name()];
    if tx
        .list_multimap_tables()?
        .any(|t| reserved.contains(&t.name()))
    {
        return Err(invalid("table is a multimap"));
    }
    let names: Vec<_> = tx.list_tables()?.map(|t| t.name().to_owned()).collect();
    let present = reserved.map(|n| names.iter().any(|s| s == n));
    let mut histories = Vec::new();
    let mut all_operations = std::collections::HashSet::new();
    if names.iter().any(|n| n == control::RECEIPTS.name()) {
        for row in tx.open_table(control::RECEIPTS)?.iter()? {
            let (_, value) = row?;
            let receipt = control::validate_receipt(value.value())?.receipt;
            all_operations.insert(receipt.operation_id);
            if is_purge(&receipt.outcome) {
                histories.push(receipt);
            }
        }
    }
    if !present.iter().any(|v| *v) {
        if !histories.is_empty() {
            return Err(invalid("receipts without progress tables"));
        }
        return Ok(vec![]);
    }
    if !present.iter().all(|v| *v) {
        return Err(invalid("partial schema"));
    }
    let state = state.ok_or_else(|| invalid("progress without lineage"))?;
    let schema = tx.open_table(SCHEMA)?;
    if schema.len()? != 1 || schema.get("version")?.is_none_or(|v| v.value() != 1) {
        return Err(invalid("unsupported schema"));
    }
    let table = tx.open_table(PROGRESS)?;
    let namespaces = tx.open_table(NAMESPACE_TABLE)?;
    let records = tx.open_table(META_TABLE)?;
    let mut found = Vec::new();
    let mut claimed = std::collections::HashSet::new();
    let mut namespace_ids = std::collections::HashSet::new();
    let mut leased_names = std::collections::HashSet::new();
    let mut reservations = std::collections::HashSet::new();
    for row in table.iter()? {
        let (key, bytes) = row?;
        let p: PurgeProgress = control::decode(bytes.value())?;
        crate::model::validation::validate_namespace_name(&p.name)
            .map_err(|_| invalid("invalid name lease"))?;
        // Active progress belongs to the current writer. Completed immutable
        // history may survive a restore fork only under its verified bound.
        let permitted_origin = crate::storage::lineage::permits_origin(
            tx,
            state,
            control::GenerationRef {
                store_id: p.store_id,
                generation: p.last_receipt.committed_generation,
            },
        )? && (p.store_id == state.store_id
            || p.stage == PurgeStage::Complete);
        if p.version != 1
            || !permitted_origin
            || p.start_receipt.store_id != p.store_id
            || p.namespace_id.is_unset()
            || p.start_receipt.replayed
            || p.last_receipt.replayed
            || p.name
                .eq_ignore_ascii_case(crate::model::constants::DEFAULT_NAMESPACE_NAME)
            || key.value() != p.start_receipt.operation_id.as_bytes()
            || !namespace_ids.insert(p.namespace_id)
            || !matches!(p.start_receipt.outcome, MutationOutcome::NamespacePurgeStarted { namespace_id,name_hash } if namespace_id == p.namespace_id && name_hash==<[u8;32]>::from(Sha256::digest(p.name.as_bytes())))
            || !histories.contains(&p.start_receipt)
        {
            return Err(invalid("progress identity/start receipt mismatch"));
        }
        claimed.insert(p.start_receipt.operation_id);
        let mut steps: Vec<_> = histories.iter().filter(|r| matches!(r.outcome, MutationOutcome::NamespacePurgeAdvanced {parent_operation_id,..} if parent_operation_id == p.start_receipt.operation_id)).collect();
        steps.sort_by_key(|r| r.committed_generation);
        let mut sequence = 0;
        let mut removed = 0u64;
        let mut tombstones = 0u64;
        let mut edges = 0u64;
        let mut cursor = None;
        let mut stage = PurgeStage::Deleting;
        let mut last = &p.start_receipt;
        for receipt in steps {
            let MutationOutcome::NamespacePurgeAdvanced {
                namespace_id,
                sequence: seq,
                stage: next,
                removed: n,
                tombstones_removed: t,
                edges_removed: e,
                last_id,
                ..
            } = receipt.outcome
            else {
                unreachable!()
            };
            sequence += 1;
            if seq != sequence
                || namespace_id != p.namespace_id
                || receipt.committed_generation <= last.committed_generation
                || receipt.store_id != p.store_id
            {
                return Err(invalid("child receipt chain"));
            }
            match (stage, next) {
                (PurgeStage::Deleting, PurgeStage::Deleting)
                    if n > 0
                        && n <= CHUNK_LIMIT as u64
                        && t <= n
                        && last_id.is_some_and(|id| {
                            id != MemoryId::nil() && cursor.is_none_or(|old| id > old)
                        }) =>
                {
                    cursor = last_id;
                }
                (PurgeStage::Deleting, PurgeStage::Cleanup)
                | (PurgeStage::Cleanup, PurgeStage::Complete)
                    if n == 0 && t == 0 && e == 0 && last_id.is_none() => {}
                _ => return Err(invalid("invalid child transition/count/cursor")),
            }
            removed = removed
                .checked_add(n)
                .ok_or_else(|| invalid("count overflow"))?;
            tombstones = tombstones
                .checked_add(t)
                .ok_or_else(|| invalid("count overflow"))?;
            edges = edges
                .checked_add(e)
                .ok_or_else(|| invalid("count overflow"))?;
            stage = next;
            last = receipt;
            claimed.insert(receipt.operation_id);
        }
        if p.sequence != sequence
            || p.stage != stage
            || p.removed != removed
            || p.tombstones_removed != tombstones
            || p.edges_removed != edges
            || p.cursor != cursor
            || &p.last_receipt != last
        {
            return Err(invalid("progress disagrees with immutable receipts"));
        }
        match (p.stage, p.next_operation_id) {
            (PurgeStage::Complete, None) => {}
            (PurgeStage::Deleting | PurgeStage::Cleanup, Some(id))
                if !id.is_nil() && !all_operations.contains(&id) && reservations.insert(id) => {}
            _ => return Err(invalid("missing, reused or invalid reserved child ID")),
        }
        let config = namespaces
            .get(p.namespace_id.get())?
            .map(|v| NamespaceConfig::decode(v.value()))
            .transpose()?;
        match p.stage {
            PurgeStage::Deleting
                if config
                    .as_ref()
                    .is_some_and(|c| c.id == p.namespace_id && c.name == p.name) => {}
            PurgeStage::Cleanup | PurgeStage::Complete if config.is_none() => {}
            _ => return Err(invalid("namespace configuration disagrees with progress")),
        }
        if p.stage != PurgeStage::Complete {
            if !leased_names.insert(p.name.to_ascii_lowercase()) {
                return Err(invalid("duplicate name lease"));
            }
            for row in namespaces.iter()? {
                let (id, value) = row?;
                let config = NamespaceConfig::decode(value.value())?;
                if id.value() != p.namespace_id.get() && config.name.eq_ignore_ascii_case(&p.name) {
                    return Err(invalid("name lease belongs to another namespace"));
                }
            }
        }
        for row in records.iter()? {
            let (key, value) = row?;
            let record = DiskRecord::from_bytes(value.value())?;
            if record.namespace_id == p.namespace_id.get()
                && (p.stage != PurgeStage::Deleting
                    || p.cursor
                        .is_some_and(|id| key.value() <= id.as_bytes().as_slice()))
            {
                return Err(invalid(
                    "remaining record behind cursor or after finalization",
                ));
            }
        }
        let counter = tx.open_table(NAMESPACE_COUNTER)?;
        if counter
            .get("max_id")?
            .is_none_or(|v| v.value() < p.namespace_id.get())
        {
            return Err(invalid("namespace counter behind progress"));
        }
        found.push(p);
    }
    if histories.iter().any(|r| !claimed.contains(&r.operation_id)) {
        return Err(invalid("orphan purge receipt"));
    }
    Ok(found)
}

pub(crate) fn require_no_active_write(tx: &redb::WriteTransaction) -> Result<(), StorageError> {
    if !tx.list_tables()?.any(|t| t.name() == PROGRESS.name()) {
        return Ok(());
    }
    for row in tx.open_table(PROGRESS)?.iter()? {
        let (_, bytes) = row?;
        let p: PurgeProgress = control::decode(bytes.value())?;
        if p.stage != PurgeStage::Complete {
            return Err(pending());
        }
    }
    Ok(())
}

impl MetadataStore {
    pub(crate) fn purge_progress(&self) -> Result<Vec<PurgeProgress>, StorageError> {
        let tx = self.db.begin_read()?;
        if !tx.list_tables()?.any(|t| t.name() == PROGRESS.name()) {
            return Ok(vec![]);
        }
        tx.open_table(PROGRESS)?
            .iter()?
            .map(|row| {
                let (_, bytes) = row?;
                control::decode(bytes.value())
            })
            .collect()
    }
    pub(crate) fn require_no_active_purge(&self) -> Result<(), StorageError> {
        if self.purge_pending.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(pending());
        }
        Ok(())
    }
}

fn load(tx: &redb::WriteTransaction, parent: Uuid) -> Result<PurgeProgress, StorageError> {
    let table = tx.open_table(PROGRESS)?;
    let value = table
        .get(parent.as_bytes().as_slice())?
        .ok_or_else(|| invalid("unknown parent operation"))?;
    control::decode(value.value())
}
fn save(tx: &redb::WriteTransaction, p: &PurgeProgress) -> Result<(), StorageError> {
    tx.open_table(PROGRESS)?.insert(
        p.start_receipt.operation_id.as_bytes().as_slice(),
        control::encode(p)?.as_slice(),
    )?;
    Ok(())
}

impl MetadataStore {
    /// The purge recovery path alone may enter while durable deletion admission
    /// is closed. Uncertain commits/canonical effects still block it.
    pub(crate) fn purge_guard(
        &self,
        parent: Option<&WorkTicket>,
    ) -> Result<MutationGuard<'_>, StorageError> {
        let guard = self.publication_guard(parent)?;
        self.ensure_handle_available()?;
        Ok(guard)
    }

    pub(crate) fn begin_purge(
        &self,
        ctx: &MutationContext,
        force: bool,
    ) -> Result<PurgeProgress, StorageError> {
        ctx.validate()?;
        let MutationScope::Namespace(namespace) = ctx.scope else {
            return Err(StorageError::InvalidMutationContext(
                "purge requires namespace scope".into(),
            ));
        };
        if ctx.contract_version != 1 {
            return Err(StorageError::InvalidMutationContext(
                "native v2 purge is not enabled".into(),
            ));
        }
        let hash = control::request_hash(ctx, "namespace_purge_v1", &[&[u8::from(force)]]);
        let _guard = self.purge_guard(None)?;
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            if !matches!(
                receipt.outcome,
                MutationOutcome::NamespacePurgeStarted { .. }
            ) {
                return Err(StorageError::IdempotencyConflict);
            }
            let mut p = load(&tx, receipt.operation_id)?;
            p.start_receipt = receipt;
            return Ok(p);
        }
        let config = super::namespace_state::require_namespace_active(&tx, namespace)?;
        if config.id != namespace
            || config.schema.is_some()
            || config
                .name
                .eq_ignore_ascii_case(crate::model::constants::DEFAULT_NAMESPACE_NAME)
        {
            return Err(StorageError::InvalidMutation(
                "default or schema-v2 namespace cannot be purged by the legacy writer".into(),
            ));
        }
        crate::model::validation::validate_namespace_name(&config.name)
            .map_err(|_| StorageError::InvalidPath)?;
        if !force {
            for row in tx.open_table(META_TABLE)?.iter()? {
                let (_, value) = row?;
                let r = DiskRecord::from_bytes(value.value())?;
                if r.namespace_id == namespace.get() && r.phase != DecayPhase::Tombstone {
                    return Err(StorageError::InvalidMutation(
                        "namespace contains live memories; force is required".into(),
                    ));
                }
            }
        }
        tx.open_table(SCHEMA)?.insert("version", 1)?;
        tx.open_table(PROGRESS)?;
        let receipt = control::record_receipt(
            &tx,
            ctx,
            hash,
            MutationOutcome::NamespacePurgeStarted {
                namespace_id: namespace,
                name_hash: Sha256::digest(config.name.as_bytes()).into(),
            },
        )?;
        let p = PurgeProgress {
            version: 1,
            store_id: receipt.store_id,
            namespace_id: namespace,
            name: config.name,
            start_receipt: receipt.clone(),
            last_receipt: receipt.clone(),
            sequence: 0,
            stage: PurgeStage::Deleting,
            next_operation_id: Some(Uuid::new_v4()),
            cursor: None,
            removed: 0,
            tombstones_removed: 0,
            edges_removed: 0,
        };
        save(&tx, &p)?;
        self.purge_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.commit_purge(tx, &receipt)?;
        Ok(p)
    }

    fn commit_purge(
        &self,
        tx: redb::WriteTransaction,
        receipt: &WriteReceipt,
    ) -> Result<(), StorageError> {
        super::effects::crash_point("before_purge_commit");
        let result = tx.commit();
        #[cfg(test)]
        let result = result.and_then(|()| {
            if super::effects::FAIL_AFTER_RECORD_COMMIT.with(|f| f.replace(false)) {
                Err(redb::CommitError::Storage(redb::StorageError::Io(
                    std::io::Error::other("injected unknown purge commit"),
                )))
            } else {
                Ok(())
            }
        });
        result.map_err(|source| {
            self.mark_unavailable();
            StorageError::CommitOutcomeUnknown {
                operation_id: receipt.operation_id,
                source,
            }
        })?;
        super::effects::crash_point("after_purge_commit");
        Ok(())
    }
}

/// Validate the durable reservation, consume it once, and reserve its successor
/// in the SAME transaction as the child mutation. No caller-selected ID API.
pub(super) fn record_next(
    tx: &redb::WriteTransaction,
    expected: &PurgeProgress,
    stage: PurgeStage,
    removed: &[(MemoryId, DiskRecord)],
    edges_removed: usize,
) -> Result<WriteReceipt, StorageError> {
    let mut p = load(tx, expected.start_receipt.operation_id)?;
    if &p != expected || p.stage == PurgeStage::Complete {
        return Err(invalid("stale progress reservation"));
    }
    let operation = p
        .next_operation_id
        .ok_or_else(|| invalid("unreserved child"))?;
    match (p.stage, stage) {
        (PurgeStage::Deleting, PurgeStage::Deleting)
            if !removed.is_empty() && removed.len() <= CHUNK_LIMIT => {}
        (PurgeStage::Deleting, PurgeStage::Cleanup)
        | (PurgeStage::Cleanup, PurgeStage::Complete)
            if removed.is_empty() && edges_removed == 0 => {}
        _ => return Err(invalid("invalid child transition")),
    }
    let last_id = removed.last().map(|(id, _)| *id);
    if removed.windows(2).any(|w| w[0].0 >= w[1].0)
        || removed.iter().any(|(id, r)| {
            r.namespace_id != p.namespace_id.get()
                || r.id != *id.as_bytes()
                || p.cursor.is_some_and(|old| *id <= old)
        })
    {
        return Err(invalid("child records disagree with reservation"));
    }
    let tombstones = removed
        .iter()
        .filter(|(_, r)| r.phase == DecayPhase::Tombstone)
        .count() as u64;
    let sequence = p
        .sequence
        .checked_add(1)
        .ok_or(StorageError::GenerationExhausted)?;
    let outcome = MutationOutcome::NamespacePurgeAdvanced {
        parent_operation_id: p.start_receipt.operation_id,
        namespace_id: p.namespace_id,
        sequence,
        stage,
        removed: removed.len() as u64,
        tombstones_removed: tombstones,
        edges_removed: edges_removed as u64,
        last_id,
    };
    let ctx = MutationContext::unkeyed(p.namespace_id);
    let hash = control::request_hash(
        &ctx,
        "namespace_purge_child_v1",
        &[
            p.start_receipt.operation_id.as_bytes(),
            &sequence.to_be_bytes(),
        ],
    );
    let receipt = control::record_reserved_receipt(tx, &ctx, hash, outcome, operation)?;
    p.sequence = sequence;
    p.stage = stage;
    p.last_receipt = receipt.clone();
    p.removed = p
        .removed
        .checked_add(removed.len() as u64)
        .ok_or(StorageError::GenerationExhausted)?;
    p.tombstones_removed = p
        .tombstones_removed
        .checked_add(tombstones)
        .ok_or(StorageError::GenerationExhausted)?;
    p.edges_removed = p
        .edges_removed
        .checked_add(edges_removed as u64)
        .ok_or(StorageError::GenerationExhausted)?;
    if last_id.is_some() {
        p.cursor = last_id;
    }
    p.next_operation_id = if stage == PurgeStage::Complete {
        None
    } else {
        Some(Uuid::new_v4())
    };
    save(tx, &p)?;
    Ok(receipt)
}

impl MetadataStore {
    pub fn lookup_namespace_purge(
        &self,
        store_id: Uuid,
        namespace: NamespaceId,
        parent: Uuid,
    ) -> Result<Option<PurgeProgress>, StorageError> {
        // Lookup is evidence, not data admission; permitted while recovery blocks
        // the serving handle, just like operation lookup after an unknown commit.
        let tx = self.db.begin_read()?;
        let state = ControlState::decode(
            tx.open_table(control::CONTROL)?
                .get("state")?
                .ok_or_else(|| invalid("missing control"))?
                .value(),
        )?;
        if store_id != state.store_id {
            return Err(StorageError::StoreLineageMismatch);
        }
        Ok(self
            .purge_progress()?
            .into_iter()
            .find(|p| p.namespace_id == namespace && p.start_receipt.operation_id == parent))
    }

    pub(crate) fn advance_purge(
        &self,
        guard: &MutationGuard<'_>,
        p: &PurgeProgress,
        limit: usize,
        edges: &[crate::storage::PersistedEdge],
    ) -> Result<Option<super::effects::EffectResult>, StorageError> {
        if !(1..=CHUNK_LIMIT).contains(&limit) {
            return Err(StorageError::InvalidMutation(
                "purge chunk limit must be 1..=1000".into(),
            ));
        }
        guard.check(self)?;
        let tx = self.db.begin_write()?;
        if load(&tx, p.start_receipt.operation_id)? != *p || p.stage != PurgeStage::Deleting {
            return Err(invalid("stale deletion progress"));
        }
        let mut ids = Vec::new();
        for row in tx.open_table(META_TABLE)?.iter()? {
            let (key, bytes) = row?;
            let r = DiskRecord::from_bytes(bytes.value())?;
            if r.namespace_id != p.namespace_id.get() {
                continue;
            }
            if key.value() != r.id.as_slice() {
                return Err(invalid("record key disagrees with identity"));
            }
            let id = MemoryId::from_bytes(r.id);
            if p.cursor.is_some_and(|old| id <= old) {
                return Err(invalid("record behind deletion cursor"));
            }
            ids.push(id);
            if ids.len() == limit {
                break;
            }
        }
        if !ids.is_empty() {
            #[cfg(test)]
            if self
                .fail_purge_child_commit
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                super::effects::FAIL_AFTER_RECORD_COMMIT.with(|f| f.set(true));
            }
            let ctx = MutationContext::unkeyed(p.namespace_id);
            let result = self.finish_record_effects(
                guard,
                tx,
                &ctx,
                [0; 32],
                edges,
                &[],
                &ids,
                true,
                false,
                super::effects::RecordMutation::PurgeChunk(p),
            )?;
            return Ok(Some(result));
        }
        // Canonical emptiness and config removal are decided under this same
        // writer guard/transaction, never from the preceding chunk's snapshot.
        let mut namespaces = tx.open_table(NAMESPACE_TABLE)?;
        let config = namespaces
            .remove(p.namespace_id.get())?
            .ok_or_else(|| invalid("missing deleting namespace"))?;
        if NamespaceConfig::decode(config.value())?.name != p.name {
            return Err(invalid("deleting namespace name changed"));
        }
        drop(config);
        drop(namespaces);
        let receipt = record_next(&tx, p, PurgeStage::Cleanup, &[], 0)?;
        self.commit_purge(tx, &receipt)?;
        Ok(None)
    }

    pub(crate) fn finish_purge_cleanup(
        &self,
        guard: &MutationGuard<'_>,
        p: &PurgeProgress,
        cleanup: impl FnOnce() -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        guard.check(self)?;
        let tx = self.db.begin_write()?;
        if load(&tx, p.start_receipt.operation_id)? != *p || p.stage != PurgeStage::Cleanup {
            return Err(invalid("stale directory cleanup reservation"));
        }
        if tx
            .open_table(NAMESPACE_TABLE)?
            .get(p.namespace_id.get())?
            .is_some()
        {
            return Err(invalid("cleanup still has a namespace"));
        }
        // The name lease stays durable if cleanup or its acknowledgement fails.
        cleanup()?;
        let receipt = record_next(&tx, p, PurgeStage::Complete, &[], 0)?;
        let pending = super::effects::PendingEffects {
            version: 1,
            receipt: receipt.clone(),
            scope: MutationScope::Namespace(p.namespace_id),
            puts: vec![],
            deletes: vec![],
            releases: vec![],
            invalidated_ids: vec![],
            removed_ids: vec![],
            peer_counts: vec![],
            canonical_applied: true,
        };
        tx.open_table(super::effects::EFFECTS)?.insert(
            receipt.operation_id.as_bytes().as_slice(),
            control::encode(&pending)?.as_slice(),
        )?;
        let epoch = self
            .effects_epoch
            .load(std::sync::atomic::Ordering::SeqCst)
            .checked_add(1)
            .ok_or(StorageError::GenerationExhausted)?;
        self.derived_dirty
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.effects_epoch
            .store(epoch, std::sync::atomic::Ordering::SeqCst);
        self.commit_purge(tx, &receipt)?;
        self.purge_pending
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
