//! Adapter implementations connecting QueryEngine traits to real subsystems.
//!
//! Each adapter struct wraps a concrete subsystem type and implements
//! the corresponding DI trait from `pipeline.rs`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::CacheManager;
use crate::embedding::EmbeddingProvider;
use crate::graph::SharedGraph;
use crate::model::{AccessKind, CachedRecord, MemoryId, NamespaceConfig, NamespaceId};
use crate::rif::{NeighborInfo, RifEngine};
// Import the real StorageEngine trait so its methods are in scope
// for RedbStorageEngine via the RwLockReadGuard deref.
use crate::storage::RedbStorageEngine;
use crate::storage::StorageEngine as _;

use super::error::{Result, SearchError};
use super::pipeline::{
    AccessRecorder, EmbeddingProviderRegistry, EntityIndexReader, EntityRecallResult,
    FtsIndexRegistry, FtsResult, GraphReader, MetadataStore, NamespaceResolver, RecordCache,
    RifProcessor, RifSuppression, ScoredResult, VectorIndexRegistry,
};
use super::{EntityIndex, FlatVectorIndex, FtsIndex, SearchFilter, VectorIndex};

// ── NamespaceResolver ──────────────────────────────────────────────

/// Adapts `RedbStorageEngine` to the `NamespaceResolver` trait.
///
/// Delegates `resolve(name)` to `StorageEngine::get_namespace_by_name`.
pub struct StorageNamespaceResolver {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl StorageNamespaceResolver {
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl NamespaceResolver for StorageNamespaceResolver {
    async fn resolve(&self, name: &str) -> Result<NamespaceConfig> {
        let storage = self.storage.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| SearchError::Internal(format!("storage lock poisoned: {e}")))?;
            storage_r
                .get_namespace_by_name(&name)
                .map_err(|e| SearchError::Internal(e.to_string()))?
                .ok_or_else(|| SearchError::NamespaceNotFound(name))
        })
        .await
        .map_err(|e| SearchError::Internal(format!("blocking task join error: {e}")))?
    }
}

// ── EmbeddingProviderRegistry ──────────────────────────────────────

/// Adapts a single `EmbeddingProvider` to `EmbeddingProviderRegistry`.
///
/// In the current design all namespaces share one provider. If
/// per-namespace providers are needed later, this adapter can hold
/// a `HashMap<String, Arc<dyn EmbeddingProvider>>`.
pub struct SingleEmbeddingRegistry {
    provider: Arc<dyn EmbeddingProvider>,
}

impl SingleEmbeddingRegistry {
    pub fn new(provider: Arc<dyn EmbeddingProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl EmbeddingProviderRegistry for SingleEmbeddingRegistry {
    async fn embed(&self, _namespace: &str, text: &str) -> Result<Vec<f32>> {
        self.provider
            .embed(text)
            .await
            .map_err(|e| SearchError::EmbeddingFailed(e.to_string()))
    }

    async fn embed_query(&self, _namespace: &str, text: &str) -> Result<Vec<f32>> {
        self.provider
            .embed_query(text)
            .await
            .map_err(|e| SearchError::EmbeddingFailed(e.to_string()))
    }
}

// ── VectorIndexRegistry ────────────────────────────────────────────

/// Adapts a shared `FlatVectorIndex` to `VectorIndexRegistry`.
///
/// All namespaces share one index; the namespace_id is passed through
/// the search filter to the underlying `VectorIndex::search`.
pub struct SharedVectorIndexRegistry {
    index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
}

impl SharedVectorIndexRegistry {
    pub fn new(index: Arc<tokio::sync::RwLock<FlatVectorIndex>>) -> Self {
        Self { index }
    }
}

impl VectorIndexRegistry for SharedVectorIndexRegistry {
    fn search(
        &self,
        namespace_id: NamespaceId,
        query_vec: &[f32],
        k: usize,
    ) -> Result<Vec<ScoredResult>> {
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.index.read())
        });
        let filter = SearchFilter {
            namespace_id: Some(namespace_id),
            ..Default::default()
        };
        let results = index
            .search(query_vec, k, &filter)
            .map_err(|e| SearchError::VectorIndexError(e.to_string()))?;
        Ok(results
            .into_iter()
            .map(|r| ScoredResult {
                memory_id: r.id,
                score: r.score,
            })
            .collect())
    }

    fn get_vector(&self, id: MemoryId) -> Option<Vec<f32>> {
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.index.read())
        });
        index.get_vector(id)
    }
}

// ── FtsIndexRegistry ──────────────────────────────────────────────

/// Adapts a shared `FtsIndex` to `FtsIndexRegistry`.
///
/// Wraps a single `FtsIndex` behind a `tokio::sync::Mutex`.
/// Namespace filtering is handled inside `FtsIndex::search` via SQL.
pub struct SharedFtsIndexRegistry {
    index: Arc<tokio::sync::Mutex<FtsIndex>>,
}

impl SharedFtsIndexRegistry {
    pub fn new(index: Arc<tokio::sync::Mutex<FtsIndex>>) -> Self {
        Self { index }
    }
}

impl FtsIndexRegistry for SharedFtsIndexRegistry {
    fn search(&self, namespace_id: NamespaceId, query: &str, k: usize) -> Result<Vec<FtsResult>> {
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.index.lock())
        });
        match index.search(namespace_id, query, k) {
            Ok(results) => Ok(results
                .into_iter()
                .map(|(id, score)| FtsResult {
                    memory_id: id,
                    score,
                })
                .collect()),
            Err(e) => {
                tracing::warn!(%e, "FTS search failed, returning empty results");
                Ok(Vec::new())
            }
        }
    }
}

// ── RecordCache ────────────────────────────────────────────────────

/// Adapts `CacheManager` to `RecordCache`.
///
/// `CacheManager::get()` is async and returns `Option<Arc<CachedRecord>>`;
/// the `RecordCache` trait has a sync `get`. We use
/// `tokio::runtime::Handle::current().block_on()` to bridge the gap,
/// which is safe because the underlying moka cache operations are
/// non-blocking in practice.
pub struct CacheManagerAdapter {
    cache: Arc<CacheManager>,
}

impl CacheManagerAdapter {
    pub fn new(cache: Arc<CacheManager>) -> Self {
        Self { cache }
    }
}

impl RecordCache for CacheManagerAdapter {
    fn get(&self, id: &MemoryId) -> Option<CachedRecord> {
        // Fast path: sync check if the key exists at all.
        if !self.cache.contains(id) {
            return None;
        }
        // Bridge async get to sync context.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.cache.get(*id))
        })
        .map(|arc| (*arc).clone())
    }
}

// ── MetadataStore ──────────────────────────────────────────────────

/// Adapts `RedbStorageEngine` to `MetadataStore`.
///
/// `StorageEngine::get_record` returns `DiskRecord`; the adapter
/// converts it to `CachedRecord` via `From<&DiskRecord>`.
pub struct StorageMetadataAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl StorageMetadataAdapter {
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl MetadataStore for StorageMetadataAdapter {
    async fn get(&self, id: &MemoryId) -> Result<Option<CachedRecord>> {
        let storage = self.storage.clone();
        let id = *id;
        tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| SearchError::MetadataError(format!("storage lock poisoned: {e}")))?;
            storage_r
                .get_record(id)
                .map(|opt| opt.map(|disk| CachedRecord::from(&disk)))
                .map_err(|e| SearchError::MetadataError(e.to_string()))
        })
        .await
        .map_err(|e| SearchError::Internal(format!("blocking task join error: {e}")))?
    }

    async fn get_batch(&self, ids: &[MemoryId]) -> Result<Vec<CachedRecord>> {
        let storage = self.storage.clone();
        let ids = ids.to_vec();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| SearchError::MetadataError(format!("storage lock poisoned: {e}")))?;
            let mut results = Vec::with_capacity(ids.len());
            for id in &ids {
                match storage_r.get_record(*id) {
                    Ok(Some(disk)) => results.push(CachedRecord::from(&disk)),
                    Ok(None) => {}
                    Err(e) => return Err(SearchError::MetadataError(e.to_string())),
                }
            }
            Ok(results)
        })
        .await
        .map_err(|e| SearchError::Internal(format!("blocking task join error: {e}")))?
    }

    async fn scan_all(&self) -> Result<Vec<CachedRecord>> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| SearchError::MetadataError(format!("storage lock poisoned: {e}")))?;
            let all = storage_r
                .scan_all()
                .map_err(|e| SearchError::MetadataError(e.to_string()))?;
            Ok(all
                .iter()
                .map(|(_id, disk)| CachedRecord::from(disk))
                .collect())
        })
        .await
        .map_err(|e| SearchError::Internal(format!("blocking task join error: {e}")))?
    }
}

// ── RifProcessor ───────────────────────────────────────────────────

/// Adapts `RifEngine` to `RifProcessor`.
///
/// The real `RifEngine::compute_effects` takes `NeighborInfo` structs;
/// the `RifProcessor` trait takes raw `MemoryId` slices. This adapter
/// constructs minimal `NeighborInfo` entries from the graph.
pub struct RifProcessorAdapter {
    engine: Arc<RifEngine>,
    graph: SharedGraph,
}

impl RifProcessorAdapter {
    pub fn new(engine: Arc<RifEngine>, graph: SharedGraph) -> Self {
        Self { engine, graph }
    }
}

impl RifProcessor for RifProcessorAdapter {
    fn compute_suppressions(
        &self,
        retrieved: &[MemoryId],
        neighbor_ids: &[MemoryId],
    ) -> Vec<RifSuppression> {
        let graph = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.graph.read())
        });
        let mut suppressions = Vec::new();
        for &retrieved_id in retrieved {
            // Build NeighborInfo from graph data for each neighbor.
            let neighbors: Vec<NeighborInfo> = neighbor_ids
                .iter()
                .filter_map(|&nid| {
                    let node = graph.get_node(&nid)?;
                    // Find the edge between retrieved_id and nid.
                    let edges = graph.edges_for(&retrieved_id);
                    let edge = edges.iter().find(|e| {
                        let source_node = graph.nodes.get(e.source);
                        let target_node = graph.nodes.get(e.target);
                        matches!(
                            (source_node, target_node),
                            (Some(s), Some(t))
                            if (s.memory_id == retrieved_id && t.memory_id == nid)
                               || (s.memory_id == nid && t.memory_id == retrieved_id)
                        )
                    });
                    let (edge_weight, edge_type, distance) = match edge {
                        Some(e) => (e.weight, e.edge_type, 1),
                        None => return None, // not actually a neighbor
                    };
                    Some(NeighborInfo {
                        memory_id: nid,
                        edge_weight,
                        edge_type,
                        graph_distance: distance,
                        retrievability: node.strength,
                        stability: 1.0, // not available from graph node
                        similarity: None,
                    })
                })
                .collect();
            let effects = self.engine.compute_effects(retrieved_id, &neighbors);
            for update in effects {
                // Convert StabilityUpdate to RifSuppression.
                if update.multiplier < 1.0 {
                    suppressions.push(RifSuppression {
                        target: update.memory_id,
                        suppression_factor: update.multiplier,
                    });
                }
            }
        }
        suppressions
    }
}

// ── AccessRecorder ─────────────────────────────────────────────────

/// Adapts `RedbStorageEngine` to `AccessRecorder`.
pub struct StorageAccessRecorder {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl StorageAccessRecorder {
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl AccessRecorder for StorageAccessRecorder {
    async fn record_access(&self, id: MemoryId, kind: AccessKind) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| SearchError::Internal(format!("storage lock poisoned: {e}")))?;
            storage_r
                .update_access(id, now, kind)
                .map_err(|e| SearchError::Internal(e.to_string()))
        })
        .await
        .map_err(|e| SearchError::Internal(format!("blocking task join error: {e}")))?
    }

    async fn record_access_batch(&self, accesses: &[(MemoryId, AccessKind)]) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let storage = self.storage.clone();
        let accesses = accesses.to_vec();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| SearchError::Internal(format!("storage lock poisoned: {e}")))?;
            for (id, kind) in &accesses {
                storage_r
                    .update_access(*id, now, *kind)
                    .map_err(|e| SearchError::Internal(e.to_string()))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| SearchError::Internal(format!("blocking task join error: {e}")))?
    }
}

// ── GraphReader ────────────────────────────────────────────────────

/// Adapts `SharedGraph` (`Arc<RwLock<RelationshipGraph>>`) to `GraphReader`.
pub struct SharedGraphReader {
    graph: SharedGraph,
}

impl SharedGraphReader {
    pub fn new(graph: SharedGraph) -> Self {
        Self { graph }
    }
}

impl GraphReader for SharedGraphReader {
    fn neighbors(&self, id: &MemoryId) -> Vec<MemoryId> {
        let graph = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.graph.read())
        });
        graph
            .outgoing_edges(id)
            .iter()
            .filter_map(|edge| graph.nodes.get(edge.target).map(|node| node.memory_id))
            .chain(
                graph
                    .incoming_edges(id)
                    .iter()
                    .filter_map(|edge| graph.nodes.get(edge.source).map(|node| node.memory_id)),
            )
            .collect()
    }

    fn spreading_activation(
        &self,
        seeds: &[(MemoryId, f32)],
        namespace_id: NamespaceId,
        graph_depth: u8,
    ) -> Vec<(MemoryId, f32)> {
        let graph = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.graph.read())
        });
        let mut config = crate::graph::activation::SpreadingActivationConfig::default();
        match graph_depth {
            2 => {
                config.max_budget = 200;
                config.firing_threshold = 0.02;
                config.output_threshold = 0.015;
            }
            3 => {
                config.max_budget = 400;
                config.hop_decay = 0.85;
                config.firing_threshold = 0.015;
                config.output_threshold = 0.01;
            }
            _ => {}
        }
        crate::graph::activation::spreading_activation(&graph, seeds, namespace_id, &config)
    }

    fn superseded_by(&self, id: &MemoryId) -> Option<MemoryId> {
        use crate::model::EdgeType;
        let graph = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.graph.read())
        });
        // Follow the supersedes chain to the latest version.
        // Supersedes edge: source (new) → target (old).
        // So if `id` is the target of a Supersedes edge, the source is the replacement.
        let mut current = *id;
        for _ in 0..10 {
            let edges = graph.incoming_edges(&current);
            let replacement = edges.iter().find_map(|edge| {
                if edge.edge_type == EdgeType::Supersedes {
                    graph.nodes.get(edge.source).map(|n| n.memory_id)
                } else {
                    None
                }
            });
            match replacement {
                Some(next) => current = next,
                None => break,
            }
        }
        if current == *id { None } else { Some(current) }
    }
}

// ── EntityIndexReader ─────────────────────────────────────────────

/// Adapts a shared `EntityIndex` to `EntityIndexReader`.
///
/// The EntityIndex is shared across all namespaces. The adapter
/// accepts a `namespace_id` parameter but does not filter by it
/// because the entity index does not track which namespace a
/// memory belongs to. Namespace filtering happens in the pipeline's
/// `passes_filters` stage, which checks each candidate's
/// `CachedRecord::namespace_id` against the query namespace.
pub struct SharedEntityIndexReader {
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
}

impl SharedEntityIndexReader {
    pub fn new(entity_index: Arc<tokio::sync::RwLock<EntityIndex>>) -> Self {
        Self { entity_index }
    }
}

impl EntityIndexReader for SharedEntityIndexReader {
    fn find_by_entities(
        &self,
        _namespace_id: NamespaceId,
        entities: &[String],
        exclude_id: MemoryId,
        k: usize,
    ) -> Result<Vec<EntityRecallResult>> {
        if entities.is_empty() {
            return Ok(Vec::new());
        }
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.entity_index.read())
        });
        let results = index.find_by_entities(entities, exclude_id);
        Ok(results
            .into_iter()
            .take(k)
            .map(|(memory_id, shared_count)| EntityRecallResult {
                memory_id,
                shared_count,
            })
            .collect())
    }
}
