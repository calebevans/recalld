use super::*;
use crate::model::EdgeType;
use crate::storage::edges::PersistedEdge;
use crate::storage::insertion::InsertMemory;

#[cfg(test)]
mod profile_boundary_tests {
    use super::*;
    use crate::embedding::runtime::tests::{bound, profile};
    use crate::storage::StorageEngine as _;

    #[test]
    fn native_profile_legacy_insertion_remains_unverified_and_replay_precedes_profile() {
        let fixture = crate::test_support::Fixture::new();
        let mut engine = fixture.storage.write().unwrap();
        let input = InsertMemory {
            id: None,
            summary: "legacy generated".into(),
            full_text: None,
            tags: vec![],
            initial_stability: None,
            created_at: None,
            embedding: None,
            parent_id: None,
            supersedes: None,
        };
        let ctx = MutationContext {
            store_id: Some(engine.control_state().unwrap().store_id),
            idempotency_key: Some("profile-replay".into()),
            ..MutationContext::unkeyed(NamespaceId::new(1))
        };
        let generation = engine.control_state().unwrap().committed_generation;
        let mut wrong = profile();
        wrong.dimension = 8;
        assert!(matches!(
            engine.insert_memory_with_profile(&ctx, &input, &[1., 0., 0., 0.], Some(&wrong), 123),
            Err(StorageError::ProviderVectorUnavailable(_))
        ));
        assert_eq!(
            engine.control_state().unwrap().committed_generation,
            generation
        );
        let receipt = engine
            .insert_memory_with_profile(&ctx, &input, &[1., 0., 0., 0.], Some(&profile()), 123)
            .unwrap();
        let id = crate::storage::insertion::inserted_id(&receipt).unwrap();
        assert!(engine.get_record(id).unwrap().unwrap().schema.is_none());
        assert!(
            engine
                .get_namespace_by_name("default")
                .unwrap()
                .unwrap()
                .schema
                .is_none()
        );
        wrong.identity_version = 99;
        let replay = engine
            .insert_memory_with_profile(&ctx, &input, &[f32::NAN], Some(&wrong), i64::MAX)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(engine.scan_all().unwrap().len(), 1);
    }

    #[test]
    fn native_profile_final_transaction_rechecks_canonical_binding_before_allocation() {
        let fixture = crate::test_support::Fixture::new();
        let mut engine = fixture.storage.write().unwrap();
        let input = InsertMemory {
            id: None,
            summary: "profile boundary".into(),
            full_text: Some("must not allocate".into()),
            tags: vec![],
            initial_stability: None,
            created_at: None,
            embedding: None,
            parent_id: None,
            supersedes: None,
        };
        let ctx = MutationContext::unkeyed(NamespaceId::new(1));
        // Isolated fixture replaces the canonical row after an adapter could
        // have resolved it. This is deliberately not an ordinary v2 writer API.
        let tx = engine.meta_store().db.begin_write().unwrap();
        tx.open_table(NAMESPACE_TABLE)
            .unwrap()
            .insert(1, serde_json::to_vec(&bound()).unwrap().as_slice())
            .unwrap();
        tx.commit().unwrap();
        let snapshot = || {
            ["meta.db", "fulltext.dat", "default/vectors.dat"]
                .map(|p| std::fs::read(fixture.path().join(p)).unwrap())
        };
        let before = snapshot();
        let generation = engine.control_state().unwrap().committed_generation;
        let mut wrong = profile();
        wrong.query_preprocessing.prefix = "same dimension, different space".into();
        for (descriptor, message) in [
            (Some(&wrong), "profile differs"),
            (Some(&profile()), "serving remains disabled"),
            (None, "provenance is unknown"),
        ] {
            let error = engine
                .insert_memory_with_profile(&ctx, &input, &[1., 0., 0., 0.], descriptor, 123)
                .unwrap_err();
            assert!(
                matches!(error, StorageError::RecoveryRequired(_)),
                "{error}"
            );
            assert!(error.to_string().contains(message), "{error}");
            assert_eq!(
                engine.control_state().unwrap().committed_generation,
                generation
            );
            assert!(engine.scan_all().unwrap().is_empty());
            assert_eq!(snapshot(), before);
        }
    }
}

impl MetadataStore {
    /// Shared owned-core allocation point for legacy/native receipt writers.
    /// Still nondeterministic until the real scoped ID/RNG prerequisite lands.
    pub(super) fn allocate_memory_id(&self, supplied: Option<MemoryId>) -> MemoryId {
        supplied.unwrap_or_else(MemoryId::new)
    }

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
        guard: &MutationGuard<'_>,
        ctx: &MutationContext,
        input: &InsertMemory,
        sampled_at: i64,
        embedding: &[f32],
        profile: Option<&crate::model::schema::EmbeddingProfile>,
        existing_edges: &[PersistedEdge],
        stage: impl FnOnce(&NamespaceConfig, &mut DiskRecord, &[f32]) -> Result<(), StorageError>,
    ) -> Result<WriteReceipt, StorageError> {
        guard.check(self)?;
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
        let config = super::namespace_state::require_namespace_active(&tx, namespace)?;
        if config.id != namespace {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        crate::embedding::runtime::check_namespace(&config, profile).map_err(|e| {
            if config.schema.is_some() {
                // Preserve the existing schema writer-gate error classification.
                StorageError::RecoveryRequired(e.to_string())
            } else {
                StorageError::ProviderVectorUnavailable(e.to_string())
            }
        })?;
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
        let id = self.allocate_memory_id(input.id);
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
        validate_insert_targets(&tx, input, namespace, id, existing_edges)?;
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
        let puts = stage_insert(
            &tx,
            input,
            &config,
            &mut record,
            &normalized,
            sampled_at,
            stage,
        )?;
        self.finish_record_effects(
            guard,
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
                &store.mutation_guard().unwrap(),
                &MutationContext::unkeyed(config.id),
                &input,
                123,
                &[],
                None,
                &[],
                |_, _, _| panic!("payload staged")
            ),
            Err(StorageError::RecoveryRequired(_))
        ));
        assert_eq!(store.control_state().unwrap(), before);
        assert!(store.scan_all().unwrap().is_empty());
    }
}

pub(super) fn validate_insert_targets(
    tx: &redb::WriteTransaction,
    input: &InsertMemory,
    namespace: NamespaceId,
    id: MemoryId,
    existing_edges: &[PersistedEdge],
) -> Result<(), StorageError> {
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
    Ok(())
}
pub(super) fn stage_insert(
    tx: &redb::WriteTransaction,
    input: &InsertMemory,
    config: &NamespaceConfig,
    record: &mut DiskRecord,
    normalized: &[f32],
    sampled_at: i64,
    stage: impl FnOnce(&NamespaceConfig, &mut DiskRecord, &[f32]) -> Result<(), StorageError>,
) -> Result<Vec<PersistedEdge>, StorageError> {
    let id = MemoryId::from_bytes(record.id);
    let namespace = config.id;
    record.encode()?;
    stage(config, record, normalized)?;
    if let Some(reference) = record.schema.as_mut().and_then(|s| s.vector.as_mut()) {
        reference.slot = record.vector_slot;
    }
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
    Ok(puts)
}
