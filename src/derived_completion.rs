//! Online completion of committed effects. Lock order is graph -> indexes ->
//! storage; never hold storage while awaiting a derived lock.
use std::sync::{Arc, RwLock};

use crate::cache::CacheManager;
use crate::graph::{RelationshipGraph, SharedGraph};
use crate::model::{DecayPhase, NamespaceId};
use crate::search::{EntityIndex, FlatVectorIndex, FtsIndex, VectorIndex, VectorMetadata};
use crate::storage::{RedbStorageEngine, StorageEngine, StorageError, TextRef};

#[derive(Clone)]
pub(crate) struct DerivedCompletion {
    pub storage: Arc<RwLock<RedbStorageEngine>>,
    pub cache: Arc<CacheManager>,
    pub graph: SharedGraph,
    pub vectors: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    pub fts: Arc<tokio::sync::Mutex<FtsIndex>>,
    pub entities: Arc<tokio::sync::RwLock<EntityIndex>>,
}

impl DerivedCompletion {
    /// Actual insertion consumer: canonical commit under the store lease, then
    /// drain leases and rebuild every derived view before ready success.
    pub async fn insert(
        &self,
        ctx: crate::storage::control::MutationContext,
        input: crate::storage::insertion::InsertMemory,
        generated_embedding: crate::embedding::runtime::GeneratedEmbedding,
        sampled_at: i64,
    ) -> Result<crate::storage::insertion::InsertResult, StorageError> {
        let gate = publication_gate(&self.storage).await?;
        let lease = gate.read_owned().await;
        let storage = self.storage.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut storage = storage.write().map_err(|e| failed(e.to_string()))?;
            let receipt = storage.insert_memory_with_profile(
                &ctx,
                &input,
                &generated_embedding.values,
                generated_embedding.profile.as_ref(),
                sampled_at,
            )?;
            let id = crate::storage::insertion::inserted_id(&receipt)?;
            let record = storage
                .get_record(id)
                .map_err(|source| StorageError::CommittedInsertion {
                    receipt: Box::new(receipt.clone()),
                    source: Box::new(source),
                })?
                .map(|r| crate::model::CachedRecord::from(&r));
            Ok::<_, StorageError>(crate::storage::insertion::InsertResult { receipt, record })
        })
        .await
        .map_err(|e| {
            failed(format!(
                "insertion worker outcome unknown; reconcile before retry: {e}"
            ))
        })?;
        drop(lease); // Complete takes this gate exclusively; never upgrade in place.
        let result = result?;
        self.complete()
            .await
            .map_err(|source| StorageError::CommittedInsertion {
                receipt: Box::new(result.receipt.clone()),
                source: Box::new(source),
            })?;
        Ok(result)
    }

    /// Retrying is safe: no receipt is acknowledged until every view is ready.
    /// Canonical/uncertain-commit failures still require reopening the engine.
    pub async fn complete(&self) -> Result<(), StorageError> {
        self.cache.bind_storage(&self.storage);
        let gate = publication_gate(&self.storage).await?;
        let _publication = gate.write_owned().await;
        let mut graph = self.graph.clone().write_owned().await;
        let mut vectors = self.vectors.clone().write_owned().await;
        let fts = self.fts.clone().lock_owned().await;
        let mut entities = self.entities.clone().write_owned().await;
        let storage = self.storage.clone();
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            let storage = storage.write().map_err(|e| failed(e.to_string()))?;
            storage.ensure_available()?;
            if !storage.meta_store().needs_effects_rebuild() {
                return Ok(());
            }
            let pending = storage.meta_store().pending_effects()?;
            let operation_id = pending
                .first()
                .ok_or_else(|| failed("dirty state without durable effects".into()))?
                .receipt
                .operation_id;
            let result = (|| {
                // ponytail: O(records + edges) rebuild under the publication locks.
                // Replace with exact effect application when throughput warrants it.
                let records = storage.scan_all()?;
                let mut next_graph = RelationshipGraph::with_capacity(records.len());
                let mut next_vectors = FlatVectorIndex::new(vectors.dimensions());
                let next_entities = EntityIndex::from_records(&records);
                fts.clear().map_err(|e| failed(e.to_string()))?;
                for (id, record) in &records {
                    let ns = NamespaceId::new(record.namespace_id);
                    next_graph
                        .add_node(
                            *id,
                            ns,
                            record.phase,
                            record.decay_strength,
                            record.vector_slot,
                        )
                        .map_err(|e| failed(e.to_string()))?;
                    if record.phase == DecayPhase::Tombstone {
                        continue;
                    }
                    let vector = storage
                        .get_vector(ns, record.vector_slot)?
                        .ok_or_else(|| failed(format!("missing vector for {id}")))?;
                    let tags: Vec<String> = record.tags.iter().map(ToString::to_string).collect();
                    next_vectors
                        .add(
                            *id,
                            &vector,
                            VectorMetadata {
                                namespace_id: ns,
                                decay_phase: record.phase.as_u8(),
                                tags: tags.clone(),
                            },
                        )
                        .map_err(|e| failed(e.to_string()))?;
                    let text = if record.text_length > 0 {
                        Some(
                            storage
                                .get_text(TextRef {
                                    file_offset: record.text_offset,
                                    length: record.text_length,
                                })?
                                .ok_or_else(|| failed(format!("missing text for {id}")))?,
                        )
                    } else {
                        None
                    };
                    fts.add(ns, *id, &record.summary, text.as_deref(), &tags)
                        .map_err(|e| failed(e.to_string()))?;
                }
                let edges = storage.load_all_edges()?;
                let count = edges.len();
                if crate::graph::rebuild_from_storage(&mut next_graph, edges.into_iter()) != count {
                    return Err(failed("graph rejected canonical edges".into()));
                }
                *graph = next_graph;
                *vectors = next_vectors;
                *entities = next_entities;
                // Preserve unrelated cached candidates used by legacy temporal
                // linking. Cache invalidation itself never acquires storage or
                // graph locks; it is safe to finish its future on this blocking
                // worker while publication is closed. Bound cache hits still
                // reject any late stale refill against canonical metadata.
                let invalidated: Vec<_> = pending
                    .iter()
                    .flat_map(|effect| effect.invalidated_ids.iter().copied())
                    .collect();
                tokio::runtime::Handle::current().block_on(cache.batch_invalidate(&invalidated));
                crate::storage::metadata::effects::crash_point("before_online_derived_ack");
                storage.meta_store().acknowledge_rebuilt_effects()
            })();
            result.map_err(|source| StorageError::CommittedEffectsPending {
                operation_id,
                source: Box::new(source),
            })
        })
        .await
        .map_err(|e| failed(e.to_string()))?
    }
}

fn failed(message: String) -> StorageError {
    StorageError::RecoveryRequired(format!(
        "derived completion failed; retry completion or reopen: {message}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::bridge::{NamespaceRegistry, StorageEngine as McpStorage};
    use crate::model::{CachedRecord, MemoryId};
    use crate::test_support::{DIM, Fixture, MemorySpec};

    fn publisher(fx: &Fixture) -> DerivedCompletion {
        DerivedCompletion {
            storage: fx.storage.clone(),
            cache: fx.cache.clone(),
            graph: fx.graph.clone(),
            vectors: fx.vector_index.clone(),
            fts: fx.fts_index.clone(),
            entities: fx.entity_index.clone(),
        }
    }

    #[tokio::test]
    async fn entity_rebuild_online_completion_replaces_stale_namespace_postings() {
        let fx = Fixture::new();
        let local = NamespaceId::new(1);
        let foreign = fx.create_namespace("foreign");
        let live = fx.insert_memory_in(local, &["entity/alice"]).await;
        let peer = fx
            .insert_memory_in(foreign, &["entity/alice", "entity/bob"])
            .await;
        let dead = fx.insert_memory_in(local, &["entity/dead"]).await;
        {
            let mut index = fx.entity_index.write().await;
            index.add(local, live, &["stale".into()]);
            index.add(local, dead, &["dead".into()]);
        }
        // Real canonical deletion dirties every view and requires completion.
        fx.storage.write().unwrap().delete_memory(dead).unwrap();
        let query = vec!["Alice".into(), "Bob".into()];
        for _ in 0..2 {
            publisher(&fx).complete().await.unwrap();
            let index = fx.entity_index.read().await;
            assert_eq!(index.len(), 2);
            assert_eq!(
                index.find_by_entities(local, &query, MemoryId::new()),
                vec![(live, 1)]
            );
            assert_eq!(
                index.find_by_entities(foreign, &query, MemoryId::new()),
                vec![(peer, 2)]
            );
            assert!(
                index
                    .find_by_entities(local, &["stale".into(), "dead".into()], MemoryId::new())
                    .is_empty()
            );
            assert!(
                fx.storage
                    .read()
                    .unwrap()
                    .meta_store()
                    .effects_stamp()
                    .is_ok()
            );
        }
        fx.storage.write().unwrap().delete_memory(live).unwrap();
        publisher(&fx).complete().await.unwrap();
        let index = fx.entity_index.read().await;
        assert_eq!(index.len(), 1);
        assert!(
            index
                .find_by_entities(local, &query, MemoryId::new())
                .is_empty()
        );
        assert_eq!(
            index.find_by_entities(foreign, &query, MemoryId::new()),
            vec![(peer, 2)]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insertion_completion_returns_receipt_on_failure_then_retries_all_views() {
        use crate::storage::control::{MutationContext, MutationOutcome, MutationScope};
        let fx = Fixture::new();
        let unrelated = fx.insert_memory().await;
        fx.cache
            .insert(
                unrelated,
                CachedRecord::from(&fx.record(unrelated).unwrap()),
            )
            .await;
        let input = crate::storage::insertion::InsertMemory {
            id: None,
            summary: "durable insertion proof".into(),
            full_text: Some("text proof".into()),
            tags: vec![crate::model::Tag::new("entity/proof").unwrap()],
            initial_stability: None,
            created_at: None,
            embedding: Some(vec![1., 0., 0., 0.]),
            parent_id: None,
            supersedes: None,
        };
        let ctx = MutationContext {
            store_id: Some(fx.storage.read().unwrap().control_state().unwrap().store_id),
            idempotency_key: Some("completion".into()),
            ..MutationContext::unkeyed(NamespaceId::new(1))
        };
        let before = fx
            .storage
            .read()
            .unwrap()
            .control_state()
            .unwrap()
            .committed_generation;
        *fx.vector_index.write().await = FlatVectorIndex::new(DIM + 1);
        let error = match publisher(&fx)
            .insert(ctx.clone(), input.clone(), vec![].into(), 123)
            .await
        {
            Ok(_) => panic!("wrong-width derived rebuild succeeded"),
            Err(error) => error,
        };
        let StorageError::CommittedInsertion { receipt, .. } = error else {
            panic!("{error}")
        };
        let MutationOutcome::MemoryInserted { memory_id, .. } = receipt.outcome else {
            panic!()
        };
        assert_eq!(receipt.committed_generation, before.next().unwrap());
        assert!(
            fx.storage_adapter()
                .get_memory(memory_id, false, None)
                .await
                .is_err()
        );
        assert_eq!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .pending_effects()
                .unwrap()[0]
                .receipt,
            *receipt
        );
        *fx.vector_index.write().await = FlatVectorIndex::new(DIM);
        let replay = publisher(&fx)
            .insert(ctx, input, vec![f32::NAN].into(), i64::MAX)
            .await
            .unwrap();
        assert!(replay.receipt.replayed);
        assert_eq!(replay.receipt.operation_id, receipt.operation_id);
        assert_eq!(
            fx.storage
                .read()
                .unwrap()
                .control_state()
                .unwrap()
                .committed_generation,
            receipt.committed_generation
        );
        assert!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .pending_effects()
                .unwrap()
                .is_empty()
        );
        assert!(fx.vector_index.read().await.get_vector(memory_id).is_some());
        assert!(fx.graph.read().await.contains(&memory_id));
        assert_eq!(fx.entity_index.read().await.len(), 1);
        assert_eq!(
            fx.fts_index
                .lock()
                .await
                .search(NamespaceId::new(1), "durable", 10)
                .unwrap()[0]
                .0,
            memory_id
        );
        assert!(
            fx.cache.get(unrelated).await.is_some(),
            "insertion must retain unrelated cached temporal candidates"
        );
        assert_eq!(
            fx.storage
                .read()
                .unwrap()
                .lookup_operation(
                    receipt.store_id,
                    MutationScope::Namespace(NamespaceId::new(1)),
                    receipt.operation_id
                )
                .unwrap()
                .unwrap(),
            *receipt
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_completion_retries_failure_and_rejects_late_stale_cache_refill() {
        let fx = Fixture::new();
        let live = fx
            .insert_memory_in(NamespaceId::new(1), &["entity/live"])
            .await;
        let dead = fx
            .insert_memory_in(NamespaceId::new(1), &["entity/dead"])
            .await;
        let stale = CachedRecord::from(&fx.record(dead).unwrap());
        let adapter = fx.storage_adapter();
        fx.cache.insert(dead, stale.clone()).await;
        fx.storage.write().unwrap().delete_memory(dead).unwrap();
        let pending = fx
            .storage
            .read()
            .unwrap()
            .meta_store()
            .pending_effects()
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert!(adapter.get_memory(dead, false, None).await.is_err());
        assert!(adapter.get_memory(live, false, None).await.is_err());
        assert!(
            adapter
                .get_memory(MemoryId::new(), false, None)
                .await
                .is_err(),
            "no false miss while blocked"
        );

        // A real vector rebuild failure after FTS clear must retain the receipt
        // and keep all public reads closed. A retry uses the same operation.
        *fx.vector_index.write().await = FlatVectorIndex::new(DIM + 1);
        assert!(matches!(publisher(&fx).complete().await,
            Err(StorageError::CommittedEffectsPending {operation_id, ..}) if operation_id == pending[0].receipt.operation_id));
        assert!(adapter.get_memory(live, false, None).await.is_err());
        assert_eq!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .pending_effects()
                .unwrap()[0]
                .receipt,
            pending[0].receipt
        );
        *fx.vector_index.write().await = FlatVectorIndex::new(DIM);
        publisher(&fx).complete().await.unwrap();
        assert!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .pending_effects()
                .unwrap()
                .is_empty()
        );
        assert!(
            adapter
                .get_memory(dead, false, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            adapter
                .get_memory(live, false, None)
                .await
                .unwrap()
                .is_some()
        );
        assert!(fx.vector_index.read().await.get_vector(dead).is_none());
        assert!(fx.vector_index.read().await.get_vector(live).is_some());
        assert!(!fx.graph.read().await.contains(&dead));
        assert_eq!(fx.entity_index.read().await.len(), 1);
        let hits = fx
            .fts_index
            .lock()
            .await
            .search(NamespaceId::new(1), "fixture", 10)
            .unwrap();
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), vec![live]);

        // Reproduce a load started before deletion that publishes after repair.
        fx.cache.insert(dead, stale.clone()).await;
        assert!(fx.cache.get(dead).await.is_none());
        assert!(
            fx.cache
                .get_or_load(dead, || async { Ok(Some(stale)) })
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            adapter
                .get_memory(dead, false, None)
                .await
                .unwrap()
                .is_none()
        );
        publisher(&fx).complete().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_completion_drains_store_publication_before_graph_lock() {
        let fx = Fixture::new();
        let dead = fx.insert_memory().await;
        fx.storage.write().unwrap().delete_memory(dead).unwrap();
        let gate = publication_gate(&fx.storage).await.unwrap();
        let store = gate.read_owned().await;
        let completion = publisher(&fx);
        let repair = tokio::spawn(async move { completion.complete().await });
        // Completion waits on the store lease, without owning graph/storage.
        let graph = tokio::time::timeout(std::time::Duration::from_secs(2), fx.graph.write())
            .await
            .unwrap();
        assert!(fx.storage.try_write().is_ok());
        assert!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .needs_effects_rebuild()
        );
        drop(graph);
        drop(store);
        tokio::time::timeout(std::time::Duration::from_secs(5), repair)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .effects_stamp()
                .is_ok()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_background_ghost_sweep_completes_without_reopen() {
        let fx = Fixture::new();
        let dead = fx
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Ghost,
                decay_strength: 0.0,
                ..MemorySpec::new(NamespaceId::new(1))
            })
            .await;
        let runner = crate::decay::sweep::DecaySweepRunner::new(
            crate::decay::sweep::SweepConfig::default(),
            Arc::new(crate::decay::DecayConfig::default()),
            crate::graph::ActivationConfig::default(),
            fx.storage.clone(),
            fx.graph.clone(),
            fx.cache.clone(),
            1.0,
        )
        .unwrap()
        .with_completion(publisher(&fx));
        let handle = runner.start();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let complete = {
                    let storage = fx.storage.read().unwrap();
                    storage.get_record(dead).unwrap().is_none()
                        && storage.meta_store().effects_stamp().is_ok()
                };
                if complete {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        runner.shutdown();
        handle.await.unwrap();
        assert!(!fx.graph.read().await.contains(&dead));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_manual_sweep_completes_ghost_deletion() {
        use crate::api::FsrsEngine;
        let fx = Fixture::new();
        let dead = fx
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Ghost,
                decay_strength: 0.0,
                ..MemorySpec::new(NamespaceId::new(1))
            })
            .await;
        let adapter = crate::api::adapters::FsrsEngineAdapter::new(
            fx.storage.clone(),
            fx.cache.clone(),
            fx.graph.clone(),
            Arc::new(crate::config::RecalldConfig::default()),
            false,
        )
        .with_indexes(
            fx.vector_index.clone(),
            fx.fts_index.clone(),
            fx.entity_index.clone(),
        );
        adapter.trigger_sweep(None).await.unwrap();
        assert!(fx.record(dead).is_none());
        assert!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .effects_stamp()
                .is_ok()
        );
        assert!(!fx.graph.read().await.contains(&dead));
    }

    #[tokio::test]
    async fn actual_namespace_purge_returns_with_admission_open() {
        let fx = Fixture::new();
        let ns = fx.create_namespace("purge");
        let dead = fx.insert_memory_in(ns, &["entity/dead"]).await;
        let result = fx
            .namespace_adapter()
            .delete_namespace(crate::mcp::bridge::DeleteNamespaceInput {
                name: "purge".into(),
                force: true,
            })
            .await
            .unwrap();
        assert!(result.deleted);
        assert!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .effects_stamp()
                .is_ok()
        );
        assert!(
            fx.storage_adapter()
                .get_memory(dead, false, None)
                .await
                .unwrap()
                .is_none()
        );
    }
}

/// Obtain the pipeline gate without holding storage across an await. Store
/// consumers hold a shared lease through their last derived write; completion
/// drains them before graph/index/storage acquisition.
pub(crate) async fn publication_gate(
    storage: &Arc<RwLock<RedbStorageEngine>>,
) -> Result<Arc<tokio::sync::RwLock<()>>, StorageError> {
    let storage = storage.clone();
    tokio::task::spawn_blocking(move || {
        Ok(storage
            .read()
            .map_err(|e| failed(e.to_string()))?
            .meta_store()
            .derived_publication
            .clone())
    })
    .await
    .map_err(|e| failed(e.to_string()))?
}
