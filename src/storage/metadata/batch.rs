//! Catalog reservation and child completion in the existing canonical redb DB.
use super::*;
use crate::storage::batch::{
    self, BatchChild, BatchDefaults, BatchError, BatchItem, BatchItemPlan, BatchItemState,
    BatchKind, BatchProgress,
};
use redb::{MultimapTableHandle, TableHandle};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::sync::atomic::Ordering;
use uuid::Uuid;

pub(crate) const SCHEMA: TableDefinition<&str, u32> = TableDefinition::new("batch_schema");
pub(crate) const BATCHES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mutation_batches");
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidControl(format!("batch manifest: {message}"))
}
fn policy_hash(config: &NamespaceConfig) -> Result<[u8; 32], StorageError> {
    Ok(Sha256::digest(control::encode(config)?).into())
}
fn catalog_context() -> MutationContext {
    MutationContext {
        scope: MutationScope::Catalog,
        ..MutationContext::unkeyed(NamespaceId::new(1))
    }
}
fn load(tx: &redb::WriteTransaction, id: Uuid) -> Result<BatchProgress, StorageError> {
    let table = tx.open_table(BATCHES)?;
    let bytes = table
        .get(id.as_bytes().as_slice())?
        .ok_or_else(|| invalid("unknown reservation"))?;
    control::decode(bytes.value())
}
fn save(tx: &redb::WriteTransaction, p: &BatchProgress) -> Result<(), StorageError> {
    tx.open_table(BATCHES)?.insert(
        p.reservation_receipt.operation_id.as_bytes().as_slice(),
        control::encode(p)?.as_slice(),
    )?;
    Ok(())
}

pub(crate) fn validate_existing(
    tx: &redb::ReadTransaction,
    state: Option<&ControlState>,
) -> Result<Vec<BatchProgress>, StorageError> {
    let reserved = [SCHEMA.name(), BATCHES.name()];
    if tx
        .list_multimap_tables()?
        .any(|t| reserved.contains(&t.name()))
    {
        return Err(invalid("reserved table is a multimap"));
    }
    let names: Vec<_> = tx.list_tables()?.map(|t| t.name().to_owned()).collect();
    let present = reserved.map(|n| names.iter().any(|s| s == n));
    let mut receipts = std::collections::HashMap::new();
    let mut operation_keys: std::collections::HashMap<Uuid, Vec<Vec<u8>>> =
        std::collections::HashMap::new();
    let mut batch_receipts = std::collections::HashSet::new();
    if names.iter().any(|n| n == control::RECEIPTS.name()) {
        for row in tx.open_table(control::RECEIPTS)?.iter()? {
            let (key, value) = row?;
            let r = control::validate_receipt(value.value())?;
            if matches!(
                r.receipt.outcome,
                MutationOutcome::BatchReserved { .. } | MutationOutcome::BatchItemRejected { .. }
            ) {
                batch_receipts.insert(r.receipt.operation_id);
            }
            operation_keys
                .entry(r.receipt.operation_id)
                .or_default()
                .push(key.value().to_vec());
            receipts.insert(key.value().to_vec(), r);
        }
    }
    if !present.iter().any(|v| *v) {
        if !batch_receipts.is_empty() {
            return Err(invalid("receipts without manifest tables"));
        }
        return Ok(vec![]);
    }
    if !present.iter().all(|v| *v) {
        return Err(invalid("partial schema"));
    }
    let state = state.ok_or_else(|| invalid("manifest without lineage"))?;
    let schema = tx.open_table(SCHEMA)?;
    if schema.len()? != 1 || schema.get("version")?.is_none_or(|v| v.value() != 1) {
        return Err(invalid("unsupported schema"));
    }
    let mut manifests = Vec::new();
    let mut claimed = std::collections::HashSet::new();
    let mut reserved_ids = std::collections::HashSet::new();
    if names.iter().any(|n| n == super::purge::PROGRESS.name()) {
        for row in tx.open_table(super::purge::PROGRESS)?.iter()? {
            let (_, bytes) = row?;
            let p: crate::storage::purge::PurgeProgress = control::decode(bytes.value())?;
            if let Some(id) = p.next_operation_id {
                reserved_ids.insert(id);
            }
        }
    }
    for row in tx.open_table(BATCHES)?.iter()? {
        let (key, bytes) = row?;
        let p: BatchProgress = control::decode(bytes.value())?;
        let parent = &p.reservation_receipt;
        let latest = p
            .bookkeeping_receipts
            .iter()
            .map(|r| r.committed_generation)
            .chain(p.items.iter().filter_map(|item| match &item.state {
                BatchItemState::Ok { receipt } => Some(receipt.committed_generation),
                _ => None,
            }))
            .fold(parent.committed_generation, std::cmp::max);
        let permitted_origin = crate::storage::lineage::permits_origin(
            tx,
            state,
            control::GenerationRef {
                store_id: parent.store_id,
                generation: latest,
            },
        )? && (parent.store_id == state.store_id || p.unresolved() == 0);
        if p.version != 1
            || !permitted_origin
            || parent.replayed
            || key.value() != parent.operation_id.as_bytes()
            || p.items.is_empty()
            || p.items.len() > crate::model::constants::MAX_BATCH_MEMORIES
        {
            return Err(invalid("identity/version/item count"));
        }
        let parent_key = control::scoped_key(
            parent.store_id,
            MutationScope::Catalog,
            parent.operation_id.as_bytes(),
        );
        if receipts
            .get(&parent_key)
            .is_none_or(|r| r.receipt != *parent)
        {
            return Err(invalid("missing reservation receipt"));
        }
        match parent.outcome {
            MutationOutcome::BatchReserved {
                batch_kind: kind,
                item_count,
                caller_hash,
                resolution_hash,
            } if kind == p.kind
                && item_count as usize == p.items.len()
                && caller_hash == p.caller_hash
                && caller_hash
                    == batch::caller_hash(
                        p.kind,
                        &p.items.iter().map(|i| i.request_hash).collect::<Vec<_>>(),
                    )?
                && resolution_hash == batch::resolution_hash(&p.items)? => {}
            _ => return Err(invalid("reservation disagrees with ordered manifest")),
        }
        claimed.insert(parent.operation_id);
        let mut rejections = std::collections::HashMap::new();
        for r in &p.bookkeeping_receipts {
            let key = control::scoped_key(
                parent.store_id,
                MutationScope::Catalog,
                r.operation_id.as_bytes(),
            );
            if r.replayed
                || r.committed_generation <= parent.committed_generation
                || receipts.get(&key).is_none_or(|stored| stored.receipt != *r)
            {
                return Err(invalid("bookkeeping receipt identity"));
            }
            let MutationOutcome::BatchItemRejected {
                batch_operation_id,
                index,
                ref error,
            } = r.outcome
            else {
                return Err(invalid("wrong bookkeeping kind"));
            };
            if batch_operation_id != parent.operation_id
                || index as usize >= p.items.len()
                || rejections.insert(index, error).is_some()
            {
                return Err(invalid("duplicate or foreign rejection"));
            }
            claimed.insert(r.operation_id);
        }
        let mut pending_prefix = false;
        let mut last_child_generation = parent.committed_generation;
        for (index, item) in p.items.iter().enumerate() {
            if item.index as usize != index
                || item.operation_id.is_nil()
                || !reserved_ids.insert(item.operation_id)
            {
                return Err(invalid("child order or reused identity"));
            }
            let child_key = item.defaults.as_ref().map(|d| {
                control::scoped_key(
                    parent.store_id,
                    MutationScope::Namespace(d.namespace_id),
                    item.operation_id.as_bytes(),
                )
            });
            let child = child_key.as_ref().and_then(|k| receipts.get(k));
            if operation_keys
                .get(&item.operation_id)
                .is_some_and(|keys| keys.iter().any(|key| Some(key) != child_key.as_ref()))
            {
                return Err(invalid(
                    "reserved child identity already used in another scope",
                ));
            }
            if let Some(d) = &item.defaults {
                crate::model::validation::validate_namespace_name(&d.namespace_name)
                    .map_err(|_| invalid("invalid resolved namespace name"))?;
                if d.namespace_id.is_unset()
                    || !(1..=8192).contains(&d.embedding_dim)
                    || !f32::from_bits(d.initial_stability_bits).is_finite()
                    || f32::from_bits(d.initial_stability_bits) <= 0.0
                    || item.canonical_hash.is_none()
                    || item.resolution_error.is_some()
                {
                    return Err(invalid("invalid resolved defaults"));
                }
            } else if item.canonical_hash.is_some() || item.resolution_error.is_none() {
                return Err(invalid("unresolved item without final error"));
            }
            match &item.state {
                BatchItemState::Ok { receipt } => {
                    if pending_prefix
                        || receipt.committed_generation <= last_child_generation
                        || item.resolution_error.is_some()
                        || rejections.contains_key(&item.index)
                        || receipt.replayed
                        || receipt.operation_id != item.operation_id
                        || receipt.committed_generation <= parent.committed_generation
                        || child.is_none_or(|r| {
                            r.receipt != *receipt || Some(r.request_hash) != item.canonical_hash
                        })
                    {
                        return Err(invalid("child receipt mismatch"));
                    }
                    if !matches!(
                        (p.kind, &receipt.outcome),
                        (BatchKind::Insert, MutationOutcome::MemoryInserted { .. })
                            | (BatchKind::Forget, MutationOutcome::MemoryTombstoned { .. })
                    ) {
                        return Err(invalid("child kind mismatch"));
                    }
                    last_child_generation = receipt.committed_generation;
                }
                BatchItemState::Error { error } => {
                    if pending_prefix && item.resolution_error.is_none()
                        || child.is_some()
                        || item.resolution_error.is_some() && rejections.contains_key(&item.index)
                        || !(item.resolution_error.as_ref() == Some(error)
                            || rejections.get(&item.index).is_some_and(|e| *e == error))
                    {
                        return Err(invalid("final error without matching bookkeeping"));
                    }
                    validate_error(error)?;
                }
                BatchItemState::NotAttempted {} => {
                    pending_prefix = true;
                    if child.is_some()
                        || item.resolution_error.is_some()
                        || rejections.contains_key(&item.index)
                    {
                        return Err(invalid("pending item already has a final outcome"));
                    }
                }
            }
        }
        manifests.push(p);
    }
    if batch_receipts != claimed {
        return Err(invalid("orphan reservation/bookkeeping receipt"));
    }
    Ok(manifests)
}

fn validate_error(error: &BatchError) -> Result<(), StorageError> {
    // Closed diagnostic vocabulary: payload/provider messages cannot leak into
    // manifests, even through an internal adapter's error mapping.
    if ![
        "INVALID_INPUT",
        "NAMESPACE_NOT_FOUND",
        "NOT_FOUND",
        "POLICY_CHANGED",
        "NAMESPACE_MISMATCH",
        "EDGE_CONFLICT",
        "CONFLICT",
    ]
    .contains(&error.code.as_str())
        || error.field.as_ref().is_some_and(|s| {
            s.len() > 128
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._[]".contains(&b))
        })
    {
        return Err(invalid("unsupported diagnostic"));
    }
    Ok(())
}

impl MetadataStore {
    pub(crate) fn pending_batch_items(&self) -> Result<usize, StorageError> {
        let tx = self.db.begin_read()?;
        if !tx.list_tables()?.any(|t| t.name() == BATCHES.name()) {
            return Ok(0);
        }
        let mut pending = 0usize;
        for row in tx.open_table(BATCHES)?.iter()? {
            let (_, bytes) = row?;
            let p: BatchProgress = control::decode(bytes.value())?;
            pending = pending
                .checked_add(p.unresolved())
                .ok_or(StorageError::GenerationExhausted)?;
        }
        Ok(pending)
    }
    pub(crate) fn reserve_batch(
        &self,
        ctx: &MutationContext,
        kind: BatchKind,
        plans: &[BatchItemPlan],
    ) -> Result<BatchProgress, StorageError> {
        self.reserve_or_resume_batch(ctx, kind, plans, None)
    }
    pub(crate) fn reserve_or_resume_batch(
        &self,
        ctx: &MutationContext,
        kind: BatchKind,
        plans: &[BatchItemPlan],
        parent: Option<Uuid>,
    ) -> Result<BatchProgress, StorageError> {
        ctx.validate()?;
        if ctx.scope != MutationScope::Catalog
            || ctx.contract_version != 1
            || plans.is_empty()
            || plans.len() > crate::model::constants::MAX_BATCH_MEMORIES
        {
            return Err(StorageError::InvalidMutationContext(
                "legacy batch requires catalog scope and bounded nonempty items".into(),
            ));
        }
        let caller_hash = batch::caller_hash(
            kind,
            &plans
                .iter()
                .map(BatchItemPlan::request_hash)
                .collect::<Vec<_>>(),
        )?;
        let hash = control::request_hash(ctx, "batch_reserve_v1", &[&caller_hash]);
        let _guard = self.mutation_guard()?;
        let tx = self.db.begin_write()?;
        if let Some(parent) = parent {
            if parent.is_nil() || ctx.store_id.is_none() || ctx.idempotency_key.is_some() {
                return Err(StorageError::InvalidMutationContext(
                    "operation resume requires lineage and no new key".into(),
                ));
            }
            let state = control::read_control(&tx)?;
            if ctx.store_id != Some(state.store_id) {
                return Err(StorageError::StoreLineageMismatch);
            }
            let mut p = load(&tx, parent)?;
            let receipts = tx.open_table(control::RECEIPTS)?;
            let key =
                control::scoped_key(state.store_id, MutationScope::Catalog, parent.as_bytes());
            let receipt = receipts
                .get(key.as_slice())?
                .ok_or_else(|| invalid("reservation receipt missing"))?;
            if p.kind != kind
                || p.caller_hash != caller_hash
                || control::validate_receipt(receipt.value())?.request_hash != hash
            {
                return Err(StorageError::IdempotencyConflict);
            }
            p.reservation_receipt.replayed = true;
            return Ok(p);
        }
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            let mut p = load(&tx, receipt.operation_id)?;
            if p.kind != kind || p.caller_hash != caller_hash {
                return Err(StorageError::IdempotencyConflict);
            }
            p.reservation_receipt.replayed = true;
            return Ok(p);
        }
        let mut items = Vec::with_capacity(plans.len());
        for (index, plan) in plans.iter().enumerate() {
            let mut item = BatchItem {
                index: index as u32,
                request_hash: plan.request_hash(),
                operation_id: Uuid::new_v4(),
                defaults: None,
                canonical_hash: None,
                resolution_error: None,
                state: BatchItemState::NotAttempted {},
            };
            match self.resolve_batch_item(&tx, kind, plan)? {
                Ok((defaults, hash)) => {
                    item.defaults = Some(defaults);
                    item.canonical_hash = Some(hash);
                }
                Err(error) => {
                    validate_error(&error)?;
                    item.resolution_error = Some(error.clone());
                    item.state = BatchItemState::Error { error };
                }
            }
            items.push(item);
        }
        let receipt = control::record_receipt(
            &tx,
            ctx,
            hash,
            MutationOutcome::BatchReserved {
                batch_kind: kind,
                item_count: items.len() as u32,
                caller_hash,
                resolution_hash: batch::resolution_hash(&items)?,
            },
        )?;
        tx.open_table(SCHEMA)?.insert("version", 1)?;
        let p = BatchProgress {
            version: 1,
            kind,
            caller_hash,
            reservation_receipt: receipt.clone(),
            bookkeeping_receipts: vec![],
            items,
        };
        save(&tx, &p)?;
        self.commit_batch(tx, &receipt)?;
        Ok(p)
    }

    fn resolve_batch_item(
        &self,
        tx: &redb::WriteTransaction,
        kind: BatchKind,
        plan: &BatchItemPlan,
    ) -> Result<Result<(BatchDefaults, [u8; 32]), BatchError>, StorageError> {
        (|| -> Result<_, StorageError> {
            let config = match (kind, plan) {
                (_, BatchItemPlan::Invalid { error, .. }) => return Ok(Err(error.clone())),
                (BatchKind::Insert, BatchItemPlan::Insert { namespace, .. }) => {
                    let mut found = None;
                    for row in tx.open_table(NAMESPACE_TABLE)?.iter()? {
                        let (_, value) = row?;
                        let c = NamespaceConfig::decode(value.value())?;
                        if c.name.eq_ignore_ascii_case(namespace) {
                            found = Some(c);
                            break;
                        }
                    }
                    let Some(c) = found else {
                        return Ok(Err(BatchError::new("NAMESPACE_NOT_FOUND")));
                    };
                    super::namespace_state::require_namespace_active(tx, c.id)?
                }
                (BatchKind::Forget, BatchItemPlan::Forget { id, .. }) => {
                    let records = tx.open_table(META_TABLE)?;
                    let Some(row) = records.get(id.as_bytes().as_slice())? else {
                        return Ok(Err(BatchError::new("NOT_FOUND")));
                    };
                    let record = DiskRecord::from_bytes(row.value())?;
                    super::namespace_state::require_namespace_active(
                        tx,
                        NamespaceId::new(record.namespace_id),
                    )?
                }
                _ => return Ok(Err(BatchError::new("INVALID_INPUT"))),
            };
            if config.schema.is_some() {
                return Ok(Err(BatchError::new("INVALID_INPUT")));
            }
            let ctx = MutationContext::unkeyed(config.id);
            let stability = match plan {
                BatchItemPlan::Insert { input, .. } => {
                    input.initial_stability.unwrap_or(config.initial_stability)
                }
                _ => config.initial_stability,
            };
            if !stability.is_finite() || stability <= 0.0 {
                return Ok(Err(BatchError::new("INVALID_INPUT")));
            }
            let hash = match plan {
                BatchItemPlan::Insert { input, .. } => {
                    input.request_hash(&ctx, stability, config.embedding_dim)?
                }
                BatchItemPlan::Forget { id, .. } => {
                    crate::storage::tombstone::request_hash(&ctx, *id)
                }
                _ => unreachable!(),
            };
            Ok(Ok((
                BatchDefaults {
                    namespace_id: config.id,
                    namespace_name: config.name.clone(),
                    namespace_policy_hash: policy_hash(&config)?,
                    initial_stability_bits: stability.to_bits(),
                    embedding_dim: config.embedding_dim,
                },
                hash,
            )))
        })()
    }

    fn commit_batch(
        &self,
        tx: redb::WriteTransaction,
        receipt: &WriteReceipt,
    ) -> Result<(), StorageError> {
        super::effects::crash_point("before_batch_commit");
        let result = tx.commit();
        #[cfg(test)]
        let result = result.and_then(|()| {
            if super::effects::FAIL_AFTER_RECORD_COMMIT.with(|f| f.replace(false))
                || self
                    .fail_batch_commit
                    .compare_exchange(
                        if matches!(receipt.outcome, MutationOutcome::BatchReserved { .. }) {
                            1
                        } else {
                            2
                        },
                        0,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
            {
                Err(redb::CommitError::Storage(redb::StorageError::Io(
                    std::io::Error::other("injected unknown batch commit"),
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
        super::effects::crash_point("after_batch_commit");
        Ok(())
    }

    pub(crate) fn lookup_batch(
        &self,
        store_id: Uuid,
        parent: Uuid,
    ) -> Result<Option<BatchProgress>, StorageError> {
        self.ensure_handle_available()?;
        let tx = self.db.begin_read()?;
        let state = ControlState::decode(
            tx.open_table(control::CONTROL)?
                .get("state")?
                .ok_or_else(|| invalid("missing control"))?
                .value(),
        )?;
        if state.store_id != store_id {
            return Err(StorageError::StoreLineageMismatch);
        }
        if !tx.list_tables()?.any(|t| t.name() == BATCHES.name()) {
            return Ok(None);
        }
        tx.open_table(BATCHES)?
            .get(parent.as_bytes().as_slice())?
            .map(|v| control::decode(v.value()))
            .transpose()
    }

    pub(crate) fn finalize_batch_error(
        &self,
        child: &BatchChild,
        error: BatchError,
    ) -> Result<BatchProgress, StorageError> {
        validate_error(&error)?;
        let _guard = self.mutation_guard()?;
        let tx = self.db.begin_write()?;
        let mut p = load_child(&tx, child)?;
        require_turn(&p, child.index)?;
        let item = &mut p.items[child.index as usize];
        if !matches!(item.state, BatchItemState::NotAttempted {}) {
            return Ok(p);
        }
        let ctx = catalog_context();
        let hash = control::request_hash(
            &ctx,
            "batch_item_error_v1",
            &[
                child.batch_operation_id.as_bytes(),
                &child.index.to_be_bytes(),
            ],
        );
        let receipt = control::record_receipt(
            &tx,
            &ctx,
            hash,
            MutationOutcome::BatchItemRejected {
                batch_operation_id: child.batch_operation_id,
                index: child.index,
                error: error.clone(),
            },
        )?;
        item.state = BatchItemState::Error { error };
        p.bookkeeping_receipts.push(receipt.clone());
        save(&tx, &p)?;
        self.commit_batch(tx, &receipt)?;
        Ok(p)
    }
}

fn load_child(
    tx: &redb::WriteTransaction,
    child: &BatchChild,
) -> Result<BatchProgress, StorageError> {
    let p = load(tx, child.batch_operation_id)?;
    let state = control::read_control(tx)?;
    if state.store_id != child.store_id || p.reservation_receipt.store_id != child.store_id {
        return Err(StorageError::StoreLineageMismatch);
    }
    let item = p
        .items
        .get(child.index as usize)
        .ok_or_else(|| invalid("child index"))?;
    if item.operation_id != child.operation_id
        || item.request_hash != child.request_hash
        || item.defaults.as_ref() != Some(&child.defaults)
        || item.canonical_hash != Some(child.canonical_hash)
    {
        return Err(StorageError::IdempotencyConflict);
    }
    Ok(p)
}

/// Matching receipt replay precedes current policy/existence/staging checks.
pub(super) fn replay_child(
    tx: &redb::WriteTransaction,
    child: &BatchChild,
    actual_hash: [u8; 32],
) -> Result<Option<WriteReceipt>, StorageError> {
    let p = load_child(tx, child)?;
    if actual_hash != child.canonical_hash {
        return Err(StorageError::IdempotencyConflict);
    }
    match &p.items[child.index as usize].state {
        BatchItemState::Ok { receipt } => {
            let mut receipt = receipt.clone();
            receipt.replayed = true;
            Ok(Some(receipt))
        }
        BatchItemState::Error { .. } => Err(StorageError::InvalidMutation(
            "batch item has a finalized error; use a new key".into(),
        )),
        BatchItemState::NotAttempted {} => {
            require_turn(&p, child.index)?;
            Ok(None)
        }
    }
}

fn require_turn(p: &BatchProgress, index: u32) -> Result<(), StorageError> {
    if p.items[..index as usize]
        .iter()
        .any(|item| matches!(item.state, BatchItemState::NotAttempted {}))
    {
        return Err(StorageError::InvalidMutation(
            "earlier batch items must finish before this child".into(),
        ));
    }
    Ok(())
}

pub(super) fn check_policy(
    tx: &redb::WriteTransaction,
    child: &BatchChild,
) -> Result<(), StorageError> {
    let current = super::namespace_state::require_namespace_active(tx, child.namespace())?;
    if current.id != child.namespace()
        || policy_hash(&current)? != child.defaults.namespace_policy_hash
    {
        return Err(StorageError::InvalidMutation(
            "batch namespace policy changed since reservation".into(),
        ));
    }
    Ok(())
}

/// Called only from the ordinary record/effects tail before its one commit.
pub(super) fn record_child(
    tx: &redb::WriteTransaction,
    child: &BatchChild,
    ctx: &MutationContext,
    hash: [u8; 32],
    outcome: MutationOutcome,
) -> Result<WriteReceipt, StorageError> {
    let mut p = load_child(tx, child)?;
    if replay_child(tx, child, hash)?.is_some()
        || ctx.scope != MutationScope::Namespace(child.namespace())
        || ctx.store_id != Some(child.store_id)
        || ctx.idempotency_key.is_some()
        || ctx.expected_generation.is_some()
        || ctx.contract_version != 1
    {
        return Err(invalid(
            "completed or foreign child attempted a fresh commit",
        ));
    }
    if !matches!(
        (p.kind, &outcome),
        (BatchKind::Insert, MutationOutcome::MemoryInserted { .. })
            | (BatchKind::Forget, MutationOutcome::MemoryTombstoned { .. })
    ) {
        return Err(invalid("wrong reserved child mutation kind"));
    }
    let receipt = control::record_reserved_receipt(tx, ctx, hash, outcome, child.operation_id)?;
    p.items[child.index as usize].state = BatchItemState::Ok {
        receipt: receipt.clone(),
    };
    save(tx, &p)?;
    Ok(receipt)
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
