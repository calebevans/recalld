//! Receipt-bearing legacy catalog mutations. Filesystem preparation belongs to
//! the engine; replay and all preconditions precede that callback.
use super::*;
use crate::storage::catalog::NamespacePolicy;

impl MetadataStore {
    pub(crate) fn create_catalog(
        &self,
        ctx: &MutationContext,
        config: &NamespaceConfig,
        prepare: impl FnOnce(NamespaceId) -> Result<(), StorageError>,
    ) -> Result<WriteReceipt, StorageError> {
        if ctx.scope != MutationScope::Catalog {
            return Err(StorageError::InvalidMutationContext(
                "create requires catalog scope".into(),
            ));
        }
        crate::storage::catalog::validate_create(config)?;
        let policy = NamespacePolicy::from(config);
        let fields = control::encode(&(&config.name, config.embedding_dim, &policy))?;
        let hash = control::request_hash(ctx, "namespace_create_v1", &[&fields]);
        let _guard = self.mutation_guard()?;
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok(receipt);
        }
        purge::require_no_active_write(&tx)?;
        control::read_control(&tx)?.committed_generation.next()?;
        let id;
        {
            let mut table = tx.open_table(NAMESPACE_TABLE)?;
            for row in table.iter()? {
                let (_, bytes) = row?;
                if NamespaceConfig::decode(bytes.value())?
                    .name
                    .eq_ignore_ascii_case(&config.name)
                {
                    return Err(StorageError::DuplicateName(config.name.clone()));
                }
            }
            let mut counter = tx.open_table(NAMESPACE_COUNTER)?;
            let previous = counter.get("max_id")?.map(|v| v.value()).unwrap_or(0);
            id =
                NamespaceId::new(previous.checked_add(1).ok_or_else(|| {
                    StorageError::InvalidControl("namespace ID exhausted".into())
                })?);
            counter.insert("max_id", id.get())?;
            let mut stored = config.clone();
            stored.id = id;
            table.insert(id.get(), control::encode(&stored)?.as_slice())?;
        }
        let receipt = control::record_receipt(
            &tx,
            ctx,
            hash,
            MutationOutcome::NamespaceCreated {
                namespace_id: id,
                created_at: config.created_at,
            },
        )?;
        prepare(id)?;
        self.commit_catalog(tx, &receipt)?;
        Ok(receipt)
    }

    pub fn update_namespace_policy_with_receipt(
        &self,
        ctx: &MutationContext,
        policy: &NamespacePolicy,
    ) -> Result<WriteReceipt, StorageError> {
        let MutationScope::Namespace(id) = ctx.scope else {
            return Err(StorageError::InvalidMutationContext(
                "policy update requires namespace scope".into(),
            ));
        };
        policy.validate()?;
        let fields = control::encode(policy)?;
        let hash = control::request_hash(ctx, "namespace_policy_update_v1", &[&fields]);
        let _guard = self.mutation_guard()?;
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok(receipt);
        }
        {
            let mut table = tx.open_table(NAMESPACE_TABLE)?;
            let mut config = namespace_state::require_namespace_active(&tx, id)?;
            if config.schema.is_some() {
                return Err(StorageError::InvalidMutation(
                    "v2 policy and profile changes require a separate migration".into(),
                ));
            }
            policy.apply(&mut config);
            table.insert(id.get(), control::encode(&config)?.as_slice())?;
        }
        let receipt = control::record_receipt(
            &tx,
            ctx,
            hash,
            MutationOutcome::NamespacePolicyUpdated { namespace_id: id },
        )?;
        self.commit_catalog(tx, &receipt)?;
        Ok(receipt)
    }

    fn commit_catalog(
        &self,
        tx: redb::WriteTransaction,
        receipt: &WriteReceipt,
    ) -> Result<(), StorageError> {
        crate::storage::catalog::boundary("before_metadata_commit")?;
        let result = tx.commit();
        #[cfg(test)]
        let result = if result.is_ok() && crate::storage::catalog::injected("commit_unknown") {
            Err(redb::CommitError::Storage(redb::StorageError::Io(
                std::io::Error::other("injected uncertain commit acknowledgement"),
            )))
        } else {
            result
        };
        result.map_err(|source| {
            self.mark_unavailable();
            StorageError::CommitOutcomeUnknown {
                operation_id: receipt.operation_id,
                source,
            }
        })?;
        crate::storage::catalog::boundary("after_metadata_commit").map_err(|source| {
            self.mark_unavailable();
            StorageError::CommittedEffectsPending {
                operation_id: receipt.operation_id,
                source: Box::new(source),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> NamespaceConfig {
        let mut c = NamespaceConfig::default_namespace(1);
        c.name = "golden".into();
        c.embedding_dim = 4;
        c.initial_stability = 1.;
        c
    }
    #[test]
    fn catalog_hash_fixture_and_capacity_checks_precede_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let state = store.control_state().unwrap();
        let ctx = MutationContext {
            store_id: Some(state.store_id),
            scope: MutationScope::Catalog,
            idempotency_key: Some("golden".into()),
            expected_generation: None,
            contract_version: 1,
        };
        let c = config();
        let receipt = store.create_catalog(&ctx, &c, |_| Ok(())).unwrap();
        {
            let read = store.db.begin_read().unwrap();
            let table = read.open_table(control::RECEIPTS).unwrap();
            let key =
                control::scoped_key(receipt.store_id, ctx.scope, receipt.operation_id.as_bytes());
            let stored =
                control::validate_receipt(table.get(key.as_slice()).unwrap().unwrap().value())
                    .unwrap();
            let actual: String = stored
                .request_hash
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(
                actual,
                "b948ebbb7f670929052e83e326bbd9b6c4a0388dff4d3ec7fd081f19786941da"
            );
        }
        {
            let tx = store.db.begin_write().unwrap();
            let mut state = control::read_control(&tx).unwrap();
            state.committed_generation = control::Generation::MAX;
            tx.open_table(control::CONTROL)
                .unwrap()
                .insert("state", control::encode(&state).unwrap().as_slice())
                .unwrap();
            tx.commit().unwrap();
        }
        let mut retry = c.clone();
        retry.created_at = 2;
        assert!(
            store
                .create_catalog(&ctx, &retry, |_| panic!("replay must not prepare"))
                .unwrap()
                .replayed
        );
        let next = MutationContext {
            idempotency_key: Some("next".into()),
            ..ctx.clone()
        };
        assert!(matches!(
            store.create_catalog(&next, &c, |_| panic!(
                "exhausted generation must not prepare"
            )),
            Err(StorageError::GenerationExhausted)
        ));
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        {
            let tx = store.db.begin_write().unwrap();
            tx.open_table(NAMESPACE_COUNTER)
                .unwrap()
                .insert("max_id", u32::MAX)
                .unwrap();
            tx.commit().unwrap();
        }
        let mut ctx = ctx;
        ctx.store_id = Some(store.control_state().unwrap().store_id);
        let before = store.control_state().unwrap();
        assert!(matches!(
            store.create_catalog(&ctx, &c, |_| panic!("exhausted ID must not prepare")),
            Err(StorageError::InvalidControl(_))
        ));
        assert_eq!(store.control_state().unwrap(), before);
    }

    #[test]
    fn catalog_receipts_reject_invalid_outcomes_and_policy_unknown_commit_is_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let id = store.create_namespace(&config()).unwrap();
        let ctx = MutationContext {
            store_id: Some(store.control_state().unwrap().store_id),
            scope: MutationScope::Namespace(id),
            idempotency_key: Some("policy".into()),
            expected_generation: None,
            contract_version: 1,
        };
        let mut policy = NamespacePolicy::from(&config());
        policy.initial_stability = 7.;
        crate::storage::catalog::FAILURE.with(|v| v.set(Some("commit_unknown")));
        let result = store.update_namespace_policy_with_receipt(&ctx, &policy);
        crate::storage::catalog::FAILURE.with(|v| v.set(None));
        assert!(matches!(
            result,
            Err(StorageError::CommitOutcomeUnknown { .. })
        ));
        assert!(store.control_state().is_err());
        drop(store);
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let replay = store
            .update_namespace_policy_with_receipt(&ctx, &policy)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(
            store.get_namespace(id).unwrap().unwrap().initial_stability,
            7.
        );
        let stored = control::StoredReceipt {
            request_hash_version: 1,
            request_hash: [0; 32],
            receipt: WriteReceipt {
                replayed: false,
                outcome: MutationOutcome::NamespacePolicyUpdated {
                    namespace_id: NamespaceId::new(0),
                },
                ..replay.clone()
            },
        };
        assert!(control::validate_receipt(&control::encode(&stored).unwrap()).is_err());
    }
    /// The migration case, and the reason the refusal is by name rather
    /// than by lookup: a database created before the validator reached
    /// this layer can already hold a row named `""`, whose directory
    /// **is** the data directory. Planting the row directly is the only
    /// way to reach that state now.
    #[tokio::test]
    async fn w47_a_legacy_empty_named_namespace_cannot_destroy_the_database() {
        use crate::mcp::bridge::{self, NamespaceRegistry as _};
        use crate::test_support::Fixture;
        let fx = Fixture::new();

        {
            let mut config = crate::model::NamespaceConfig::default_namespace(0);
            config.name = String::new();
            config.embedding_dim = crate::test_support::DIM as u32;
            // Raw legacy fixture: current catalog APIs must reject this name.
            config.id = NamespaceId::new(2);
            let storage = fx.storage.write().unwrap();
            let tx = storage.meta_store().db.begin_write().unwrap();
            tx.open_table(NAMESPACE_TABLE)
                .unwrap()
                .insert(2, control::encode(&config).unwrap().as_slice())
                .unwrap();
            tx.open_table(NAMESPACE_COUNTER)
                .unwrap()
                .insert("max_id", 2)
                .unwrap();
            tx.commit().unwrap();
        }

        let err = fx
            .namespace_adapter()
            .delete_namespace(bridge::DeleteNamespaceInput {
                name: String::new(),
                force: true,
            })
            .await
            .expect_err("an empty namespace name must be refused");
        assert!(
            matches!(err, bridge::BridgeError::InvalidInput(_)),
            "{err:?}"
        );

        assert!(
            fx.path().join("meta.db").exists()
                && fx.path().join("edges.db").exists()
                && fx.path().join("fulltext.dat").exists(),
            "the database was destroyed"
        );
        assert!(
            fx.namespace_dir("default").join("vectors.dat").exists(),
            "the default namespace's vectors were destroyed"
        );
    }
}
