use super::*;
use crate::model::EdgeType;
use crate::storage::edges::PersistedEdge;
use crate::storage::insertion::InsertMemory;

impl MetadataStore {
    pub(crate) fn lookup_insert_replay(
        &self,
        ctx: &MutationContext,
        input: &InsertMemory,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        let _guard = self.mutation_guard()?;
        let tx = self.db.begin_write()?;
        replay_insert(&tx, ctx, input)
    }

    /// Replay and all fresh canonical checks precede payload allocation. The
    /// caller stages payloads through the closure while this publication guard
    /// and redb transaction are held; no metadata writer can acquire ownership
    /// of that reserved slot before this transaction resolves.
    pub(crate) fn insert_with_effects(
        &self,
        ctx: &MutationContext,
        input: &InsertMemory,
        sampled_at: i64,
        embedding: &[f32],
        existing_edges: &[PersistedEdge],
        stage: impl FnOnce(&NamespaceConfig, &mut DiskRecord, &[f32]) -> Result<(), StorageError>,
    ) -> Result<WriteReceipt, StorageError> {
        let guard = self.mutation_guard()?;
        ctx.validate()?;
        if ctx.contract_version != 1 {
            return Err(StorageError::InvalidMutationContext(
                "explicit-v2 insertion is not enabled".into(),
            ));
        }
        let MutationScope::Namespace(namespace) = ctx.scope else {
            return Err(StorageError::InvalidMutationContext(
                "insertion requires namespace scope".into(),
            ));
        };
        let tx = self.db.begin_write()?;
        if let Some(receipt) = replay_insert(&tx, ctx, input)? {
            return Ok(receipt);
        }
        // Reserve a generation before writing external bytes (without advancing
        // it yet). Exhaustion must leave payload files untouched.
        control::read_control(&tx)?.committed_generation.next()?;
        let config = NamespaceConfig::decode(
            tx.open_table(NAMESPACE_TABLE)?
                .get(namespace.get())?
                .ok_or(StorageError::NamespaceNotFound(namespace.get()))?
                .value(),
        )?;
        if config.id != namespace {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        if config.schema.is_some() || control::read_control(&tx)?.control_schema_version != 1 {
            return Err(StorageError::RecoveryRequired(
                "schema-v2 insertion requires participating writers; serving remains disabled"
                    .into(),
            ));
        }
        let stability = input.initial_stability.unwrap_or(config.initial_stability);
        let hash = input.request_hash(ctx, stability, config.embedding_dim)?;
        control::replay_or_check(&tx, ctx, hash)?;
        let timestamp = input.created_at.unwrap_or(sampled_at);
        if chrono::DateTime::from_timestamp_millis(timestamp).is_none()
            || chrono::DateTime::from_timestamp_millis(sampled_at).is_none()
        {
            return Err(StorageError::InvalidMutation(
                "unrepresentable creation timestamp".into(),
            ));
        }
        if !stability.is_finite() || stability <= 0.0 {
            return Err(StorageError::InvalidMutation(
                "initial stability must be finite and positive".into(),
            ));
        }
        crate::model::validation::validate_memory_input(crate::model::validation::MemoryInputRef {
            summary: &input.summary,
            full_text: input.full_text.as_deref(),
            tags: &input
                .tags
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            entities: &[],
            topics: &[],
            emotions: &[],
        })
        .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
        if input.full_text.as_deref() == Some("") {
            return Err(StorageError::EmptyText);
        }
        // Caller identity was hashed from the original bytes above. Generated
        // output never enters that hash; replay returns before this validation.
        let mut normalized = embedding.to_vec();
        crate::storage::insertion::normalize_embedding(
            &mut normalized,
            config.embedding_dim,
            input.embedding.is_some(),
        )?;
        let id = input.id.unwrap_or_else(MemoryId::new);
        if id == MemoryId::nil() {
            return Err(StorageError::InvalidMutation("nil memory ID".into()));
        }
        removal_ledger::ensure_insertable(&tx, id)?;
        if tx
            .open_table(META_TABLE)?
            .get(id.as_bytes().as_slice())?
            .is_some()
        {
            return Err(StorageError::DuplicateId(id.into_inner()));
        }
        // Validate explicit targets and branches before staging. The common
        // effect tail repeats endpoint checks inside this same transaction.
        for target in [input.parent_id, input.supersedes].into_iter().flatten() {
            if target == id {
                return Err(StorageError::InvalidMutation("self link".into()));
            }
            let row = tx.open_table(META_TABLE)?;
            let value = row
                .get(target.as_bytes().as_slice())?
                .ok_or(StorageError::NotFound(target.into_inner()))?;
            let record = DiskRecord::from_bytes(value.value())?;
            if record.id != *target.as_bytes() || record.namespace_id != namespace.get() {
                return Err(StorageError::MutationNamespaceMismatch);
            }
        }
        if input.supersedes.is_some_and(|target| {
            existing_edges
                .iter()
                .any(|edge| edge.edge_type == EdgeType::Supersedes && edge.target == target)
        }) {
            return Err(StorageError::EdgeConflict("supersedes branch".into()));
        }
        let mut record = DiskRecord {
            schema: None,
            version: 1,
            id: *id.as_bytes(),
            namespace_id: namespace.get(),
            created_at: timestamp,
            last_accessed_at: timestamp,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability,
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: input.summary.clone(),
            tags: input.tags.clone(),
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        };
        record.encode()?;
        stage(&config, &mut record, &normalized)?;
        tx.open_table(META_TABLE)?
            .insert(id.as_bytes().as_slice(), record.encode()?.as_slice())?;
        for tag in &record.tags {
            tx.open_multimap_table(TAG_INDEX)?
                .insert(tag.as_str(), id.as_bytes().as_slice())?;
        }
        tx.open_multimap_table(NAMESPACE_INDEX)?
            .insert(namespace.get(), id.as_bytes().as_slice())?;
        let mut puts = Vec::new();
        for (source, target, edge_type) in input
            .parent_id
            .map(|parent| (parent, id, EdgeType::ParentChild))
            .into_iter()
            .chain(input.supersedes.map(|old| (id, old, EdgeType::Supersedes)))
        {
            puts.push(PersistedEdge {
                source,
                target,
                edge_type,
                weight: 1.0,
                auto_created: false,
                created_at: sampled_at.max(0) as u64,
            });
        }
        self.finish_record_effects(
            &guard,
            tx,
            ctx,
            hash,
            existing_edges,
            &puts,
            &[],
            false,
            false,
            effects::RecordMutation::Inserted(&record, config.embedding_dim),
        )
        .map(|result| result.receipt)
    }
}

fn replay_insert(
    tx: &redb::WriteTransaction,
    ctx: &MutationContext,
    input: &InsertMemory,
) -> Result<Option<WriteReceipt>, StorageError> {
    let Some(stored) = control::keyed_receipt(tx, ctx)? else {
        return Ok(None);
    };
    let MutationOutcome::MemoryInserted {
        initial_stability_bits,
        embedding_dim,
        ..
    } = stored.receipt.outcome
    else {
        return Err(StorageError::IdempotencyConflict);
    };
    let hash = input.request_hash(ctx, f32::from_bits(initial_stability_bits), embedding_dim)?;
    control::replay_or_check(tx, ctx, hash)
}

#[cfg(test)]
mod schema_tests {
    use super::*;
    #[test]
    fn insertion_independently_rejects_schema_policy_before_payload_stage() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let mut config = NamespaceConfig::default_namespace(0);
        config.embedding_dim = 4;
        config.id = store.create_namespace(&config).unwrap();
        config.schema = Some(crate::model::schema::NamespaceSchemaV2::from_legacy());
        // Synthetic legacy schema row, bypassing the separately tested catalog
        // admission solely to prove the final insertion boundary independently.
        let tx = store.db.begin_write().unwrap();
        tx.open_table(NAMESPACE_TABLE)
            .unwrap()
            .insert(
                config.id.get(),
                control::encode(&config).unwrap().as_slice(),
            )
            .unwrap();
        tx.commit().unwrap();
        let before = store.control_state().unwrap();
        let input = InsertMemory {
            id: None,
            summary: "schema probe".into(),
            full_text: None,
            tags: vec![],
            initial_stability: None,
            created_at: None,
            embedding: Some(vec![1., 0., 0., 0.]),
            parent_id: None,
            supersedes: None,
        };
        assert!(matches!(
            store.insert_with_effects(
                &MutationContext::unkeyed(config.id),
                &input,
                123,
                &[],
                &[],
                |_, _, _| panic!("payload staged")
            ),
            Err(StorageError::RecoveryRequired(_))
        ));
        assert_eq!(store.control_state().unwrap(), before);
        assert!(store.scan_all().unwrap().is_empty());
    }
}
