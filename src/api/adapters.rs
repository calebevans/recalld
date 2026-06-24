//! Adapter implementations connecting API-layer traits to real subsystems.
//!
//! Each adapter wraps the concrete subsystem type and implements the
//! corresponding API trait from `state.rs`. These adapters bridge the
//! gap between the API server's DI traits and the concrete subsystem
//! types held by the `Recalld` struct.

use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::CacheManager;
use crate::embedding::EmbeddingProvider;
use crate::graph::SharedGraph;
use crate::model::{
    AccessKind, CachedRecord, DecayPhase, MemoryId, NamespaceConfig, NamespaceId,
};
use crate::search::FlatVectorIndex;
use crate::storage::RedbStorageEngine;
// Import the StorageEngine trait so its methods are in scope.
use crate::storage::StorageEngine as StorageEngineTrait;

use super::state;

// ═══════════════════════════════════════════════════════════════════════
// SearchPipelineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts the real search subsystems to the API `SearchPipeline` trait.
pub struct SearchPipelineAdapter {
    query_engine: Arc<crate::search::QueryEngine>,
    embedding: Arc<dyn EmbeddingProvider>,
    vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
}

impl SearchPipelineAdapter {
    pub fn new(
        query_engine: Arc<crate::search::QueryEngine>,
        embedding: Arc<dyn EmbeddingProvider>,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    ) -> Self {
        Self {
            query_engine,
            embedding,
            vector_index,
        }
    }
}

#[async_trait]
impl state::SearchPipeline for SearchPipelineAdapter {
    async fn embed_text(
        &self,
        text: &str,
        _namespace_id: NamespaceId,
    ) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
        self.embedding.embed(text).await
    }

    async fn search(
        &self,
        query: state::SearchQuery,
    ) -> Result<Vec<state::ResolvedSearchResult>, crate::search::SearchError> {
        // Convert API-layer SearchQuery to the pipeline's SearchQuery.
        let text = match query.query {
            state::QueryInput::Text(t) => Some(t),
            state::QueryInput::Vector(_) => None,
        };
        let pipeline_query = crate::search::SearchQuery {
            text,
            fts_query: None,
            namespace: String::new(),
            filter: crate::search::PipelineSearchFilter::default(),
            limit: query.k,
            min_score: query.min_score.unwrap_or(0.0),
            include_ghosts: false,
            query_mode: crate::search::QueryMode::default(),
            graph_depth: query.graph_depth as u8,
            time_range_start: None,
            time_range_end: None,
            entities: Vec::new(),
        };
        let response = self.query_engine.search(pipeline_query).await?;
        Ok(response
            .results
            .into_iter()
            .map(|r| {
                state::ResolvedSearchResult {
                    memory: CachedRecord {
                        id: r.memory_id,
                        namespace_id: NamespaceId::UNSET,
                        created_at: r.created_at,
                        last_accessed_at: 0,
                        phase: r.phase,
                        strength: r.retrievability,
                        decay_strength: r.effective_r,
                        stability: r.stability,
                        difficulty: 5.0,
                        is_permastore: r.is_permastore,
                        summary: r.summary.unwrap_or_default(),
                        tags: r.tags,
                        edge_count: r.edge_count,
                        vector_slot: 0,
                        entities: Vec::new(),
                    },
                    score: r.score.unwrap_or(0.0),
                }
            })
            .collect())
    }

    async fn index_memory(
        &self,
        id: MemoryId,
        embedding: &[f32],
        namespace_id: NamespaceId,
    ) {
        use crate::search::{VectorIndex, VectorMetadata};
        let mut index = self.vector_index.write().await;
        let metadata = VectorMetadata {
            namespace_id,
            decay_phase: DecayPhase::Full.as_u8(),
            tags: Vec::new(),
        };
        let _ = index.add(id, embedding, metadata);
    }

    async fn remove_from_index(&self, id: MemoryId) {
        use crate::search::VectorIndex;
        let mut index = self.vector_index.write().await;
        let _ = index.remove(id);
    }

    fn get_embedding(&self, id: MemoryId) -> Option<Vec<f32>> {
        use crate::search::VectorIndex;
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.vector_index.read())
        });
        index.get_vector(id)
    }

    fn indexed_count(&self) -> usize {
        use crate::search::VectorIndex;
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.vector_index.read())
        });
        index.len()
    }

    async fn embedding_provider_healthy(&self) -> bool {
        // A simple health check: try to embed a trivial string.
        self.embedding.embed("health check").await.is_ok()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// StorageEngineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` to the API `StorageEngine` trait.
pub struct StorageEngineAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    #[allow(dead_code)]
    embedding: Arc<dyn EmbeddingProvider>,
}

impl StorageEngineAdapter {
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        embedding: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            storage,
            cache,
            embedding,
        }
    }
}

#[async_trait]
impl state::StorageEngine for StorageEngineAdapter {
    async fn create_memory(
        &self,
        namespace_id: NamespaceId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
        embedding: &[f32],
        initial_stability: Option<f32>,
    ) -> Result<CachedRecord, crate::storage::StorageError> {
        use crate::model::record::DiskRecord;
        use crate::model::Tag;

        let id = MemoryId::new();
        let now = chrono::Utc::now().timestamp_millis();
        let parsed_tags: Vec<Tag> = tags
            .iter()
            .filter_map(|t| Tag::new(t).ok())
            .collect();

        let mut record = DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: namespace_id.get(),
            created_at: now,
            last_accessed_at: now,
            phase: DecayPhase::Full.as_u8(),
            strength: 1.0,
            decay_strength: 1.0,
            stability: initial_stability.unwrap_or(3.7),
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: summary.to_string(),
            tags: parsed_tags,
            access_history: Vec::new(),
            text_offset: 0,
            text_length: 0,
        };

        {
            let mut storage_w = self.storage.write().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_w.insert_memory(
                id,
                namespace_id,
                &mut record,
                embedding,
                full_text,
            )?;
        }

        let cached = CachedRecord::from(&record);
        self.cache.insert(id, cached.clone()).await;
        Ok(cached)
    }

    async fn delete_memory(
        &self,
        id: MemoryId,
    ) -> Result<bool, crate::storage::StorageError> {
        let deleted = {
            let mut storage_w = self.storage.write().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_w.delete_memory(id)?
        };
        if deleted.is_some() {
            self.cache.invalidate(id).await;
        }
        Ok(deleted.is_some())
    }

    async fn namespace_stats(
        &self,
        _namespace_id: NamespaceId,
    ) -> Result<state::NamespaceStats, crate::storage::StorageError> {
        // Full namespace stats would require iterating records.
        // Return a basic placeholder until a dedicated stats query exists.
        Ok(state::NamespaceStats {
            memory_count: 0,
            phase_1_count: 0,
            phase_2_count: 0,
            phase_3_count: 0,
            permastore_count: 0,
            avg_strength: 0.0,
            edge_count: 0,
        })
    }

    async fn ping(&self) -> bool {
        self.storage.read().is_ok()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RecordCacheAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `CacheManager` to the API `RecordCache` trait.
pub struct RecordCacheAdapter {
    cache: Arc<CacheManager>,
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl RecordCacheAdapter {
    pub fn new(
        cache: Arc<CacheManager>,
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    ) -> Self {
        Self { cache, storage }
    }
}

#[async_trait]
impl state::RecordCache for RecordCacheAdapter {
    async fn get_or_load(
        &self,
        id: MemoryId,
        _storage: &dyn state::StorageEngine,
    ) -> Option<CachedRecord> {
        // Use the real storage, not the trait object, for loading.
        let storage_clone = self.storage.clone();
        let result = self
            .cache
            .get_or_load(id, || async {
                let storage_r = storage_clone.read().map_err(|e| {
                    anyhow::anyhow!("storage lock poisoned: {e}")
                })?;
                match storage_r.get_record(id) {
                    Ok(Some(disk)) => Ok(Some(CachedRecord::from(&disk))),
                    Ok(None) => Ok(None),
                    Err(e) => Err(anyhow::anyhow!("{e}")),
                }
            })
            .await;
        match result {
            Ok(Some(arc)) => Some((*arc).clone()),
            _ => None,
        }
    }

    async fn insert(&self, record: &CachedRecord) {
        self.cache.insert(record.id, record.clone()).await;
    }

    async fn remove(&self, id: MemoryId) {
        self.cache.invalidate(id).await;
    }

    fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    fn hit_rate(&self) -> f64 {
        // moka does not expose hit rate directly; return 0.0.
        0.0
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RelationshipGraphAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `SharedGraph` to the API `RelationshipGraph` trait.
pub struct RelationshipGraphAdapter {
    graph: SharedGraph,
}

impl RelationshipGraphAdapter {
    pub fn new(graph: SharedGraph) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl state::RelationshipGraph for RelationshipGraphAdapter {
    async fn add_edge(
        &self,
        from: MemoryId,
        to: MemoryId,
        edge_type: &str,
    ) -> Result<(), crate::graph::GraphError> {
        use crate::model::EdgeType;
        let etype = match edge_type {
            "parent_child" | "ParentChild" => EdgeType::ParentChild,
            "associative" | "Associative" => EdgeType::Associative,
            "causal" | "Causal" => EdgeType::Causal,
            "contradicts" | "Contradicts" => EdgeType::Contradicts,
            _ => EdgeType::Associative,
        };
        let mut graph = self.graph.write().await;
        graph.add_edge(from, to, etype, 1.0, false)?;
        Ok(())
    }

    async fn remove_all_edges(
        &self,
        id: MemoryId,
    ) -> Result<(), crate::graph::GraphError> {
        let mut graph = self.graph.write().await;
        graph.remove_node(id)?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FsrsEngineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts the decay subsystem to the API `FsrsEngine` trait.
pub struct FsrsEngineAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    sweep_alive: bool,
}

impl FsrsEngineAdapter {
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        has_sweep: bool,
    ) -> Self {
        Self {
            storage,
            sweep_alive: has_sweep,
        }
    }
}

#[async_trait]
impl state::FsrsEngine for FsrsEngineAdapter {
    async fn record_access(&self, id: MemoryId, kind: AccessKind) {
        let now = chrono::Utc::now().timestamp_millis();
        if let Ok(storage_r) = self.storage.read() {
            let _ = storage_r.update_access(id, now, kind);
        }
    }

    async fn reinforce(
        &self,
        _id: MemoryId,
        _quality: u8,
    ) -> Result<state::ReinforceResult, Box<dyn std::error::Error + Send + Sync>>
    {
        // Reinforcement requires FSRS stability recalculation.
        // Return a placeholder result reflecting current state.
        Ok(state::ReinforceResult {
            strength: 1.0,
            stability: 3.7,
            phase: DecayPhase::Full,
            is_permastore: false,
        })
    }

    fn sweep_thread_alive(&self) -> bool {
        self.sweep_alive
    }

    fn last_sweep_time(&self) -> Option<std::time::Instant> {
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════
// NamespaceRegistryAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` to the API `NamespaceRegistry` trait.
pub struct NamespaceRegistryAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl NamespaceRegistryAdapter {
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl state::NamespaceRegistry for NamespaceRegistryAdapter {
    fn resolve(&self, name: &str) -> Option<NamespaceConfig> {
        let storage_r = self.storage.read().ok()?;
        storage_r.get_namespace_by_name(name).ok().flatten()
    }

    fn get_by_id(&self, id: u32) -> Option<NamespaceConfig> {
        let storage_r = self.storage.read().ok()?;
        storage_r.get_namespace(NamespaceId::new(id)).ok().flatten()
    }

    fn name_for(&self, id: NamespaceId) -> Option<String> {
        let storage_r = self.storage.read().ok()?;
        storage_r
            .get_namespace(id)
            .ok()
            .flatten()
            .map(|ns| ns.name.clone())
    }

    async fn list_all(&self) -> Vec<state::NamespaceListInfo> {
        let storage_r = match self.storage.read() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        match storage_r.list_namespaces() {
            Ok(namespaces) => namespaces
                .into_iter()
                .map(|ns| state::NamespaceListInfo {
                    id: ns.id.get(),
                    name: ns.name.clone(),
                    embedding_dim: ns.embedding_dim,
                    memory_count: 0,
                    created_at: ns.created_at,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn create(
        &self,
        name: &str,
        embedding_dim: u32,
        initial_stability: f32,
        desired_retention: f32,
    ) -> Result<NamespaceConfig, Box<dyn std::error::Error + Send + Sync>> {
        let now = chrono::Utc::now().timestamp_millis();
        let ns_config = NamespaceConfig {
            id: NamespaceId::UNSET, // will be assigned by storage
            name: name.to_string(),
            embedding_dim,
            initial_stability,
            default_difficulty: 5.0,
            phase_thresholds: crate::model::namespace::PhaseThresholds::default(),
            permastore_threshold: 1500.0,
            created_at: now,
            desired_retention,
        };
        let mut storage_w = self.storage.write().map_err(|e| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("storage lock poisoned: {e}"),
            )) as Box<dyn std::error::Error + Send + Sync>
        })?;
        let assigned_id = storage_w.create_namespace(&ns_config)?;
        Ok(NamespaceConfig {
            id: assigned_id,
            ..ns_config
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
// NoopMetricsCollector
// ═══════════════════════════════════════════════════════════════════════

/// No-op metrics collector until a real metrics module is implemented.
pub struct NoopMetricsCollector;

#[async_trait]
impl state::MetricsCollector for NoopMetricsCollector {
    async fn render_prometheus(&self) -> String {
        String::new()
    }
}
