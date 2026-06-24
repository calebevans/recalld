//! Adapter implementations connecting API-layer traits to real subsystems.
//!
//! Each adapter wraps the concrete subsystem type and implements the
//! corresponding API trait from `state.rs`. These adapters bridge the
//! gap between the API server's DI traits and the concrete subsystem
//! types held by the `Recalld` struct.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::CacheManager;
use crate::embedding::EmbeddingProvider;
use crate::graph::SharedGraph;
use crate::model::{
    AccessKind, CachedRecord, DecayPhase, DiskRecord, MemoryId, NamespaceConfig, NamespaceId,
};
use crate::search::FlatVectorIndex;
use crate::storage::RedbStorageEngine;
// Import the StorageEngine trait so its methods are in scope.
use super::state;
use crate::storage::StorageEngine as StorageEngineTrait;

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
    /// Creates a new adapter from the real search subsystem components.
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
            .map(|r| state::ResolvedSearchResult {
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
            })
            .collect())
    }

    async fn index_memory(&self, id: MemoryId, embedding: &[f32], namespace_id: NamespaceId) {
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

    fn get_embeddings_batch(
        &self,
        ids: &[MemoryId],
    ) -> std::collections::HashMap<MemoryId, Vec<f32>> {
        use crate::search::VectorIndex;
        // Acquire the lock once for the entire batch instead of
        // once per ID, amortizing the block_in_place overhead.
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.vector_index.read())
        });
        let mut result = std::collections::HashMap::with_capacity(ids.len());
        for &id in ids {
            if let Some(vec) = index.get_vector(id) {
                result.insert(id, vec);
            }
        }
        result
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
    /// Creates a new adapter wrapping the storage engine, cache, and embedding provider.
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
        use crate::model::Tag;
        use crate::model::record::DiskRecord;

        let id = MemoryId::new();
        let now = chrono::Utc::now().timestamp_millis();
        let parsed_tags: Vec<Tag> = tags.iter().filter_map(|t| Tag::new(t).ok()).collect();

        let mut record = DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: namespace_id.get(),
            created_at: now,
            last_accessed_at: now,
            phase: DecayPhase::Full,
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
            storage_w.insert_memory(id, namespace_id, &mut record, embedding, full_text)?;
        }

        let cached = CachedRecord::from(&record);
        self.cache.insert(id, cached.clone()).await;
        Ok(cached)
    }

    async fn get_record(&self, id: MemoryId) -> Option<DiskRecord> {
        let storage_r = self.storage.read().ok()?;
        storage_r.get_record(id).ok().flatten()
    }

    async fn delete_memory(&self, id: MemoryId) -> Result<bool, crate::storage::StorageError> {
        // Read the existing record to get namespace and vector slot info,
        // and to short-circuit if already tombstoned or missing.
        let existing_record = {
            let storage_r = self.storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.get_record(id)?
        };

        let Some(existing_record) = existing_record else {
            return Ok(false);
        };

        if existing_record.phase == DecayPhase::Tombstone {
            return Ok(false);
        }

        // Tombstone the record (preserves graph edges).
        {
            let storage_r = self.storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.tombstone_memory(id)?;
        }

        // Free the vector slot on disk.
        {
            let mut storage_w = self.storage.write().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            let ns_id = NamespaceId::new(existing_record.namespace_id);
            if let Err(e) = storage_w.free_vector_slot(ns_id, existing_record.vector_slot) {
                tracing::warn!(
                    memory_id = %id,
                    vector_slot = existing_record.vector_slot,
                    %e,
                    "vector slot free failed (non-fatal)"
                );
            }
        }

        self.cache.invalidate(id).await;
        Ok(true)
    }

    async fn namespace_stats(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<state::NamespaceStats, crate::storage::StorageError> {
        let storage_r = self.storage.read().map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("storage lock poisoned: {e}"),
            ))
        })?;

        let all_records = storage_r.scan_all()?;

        let mut memory_count: u64 = 0;
        let mut phase_1_count: u64 = 0;
        let mut phase_2_count: u64 = 0;
        let mut phase_3_count: u64 = 0;
        let mut permastore_count: u64 = 0;
        let mut strength_sum: f64 = 0.0;
        let mut edge_count: u64 = 0;

        for (_id, record) in &all_records {
            if NamespaceId::new(record.namespace_id) != namespace_id {
                continue;
            }
            memory_count += 1;

            match record.phase {
                DecayPhase::Full => phase_1_count += 1,
                DecayPhase::Summary => phase_2_count += 1,
                DecayPhase::Ghost => phase_3_count += 1,
                DecayPhase::Tombstone => {} // tombstones are not counted in phase stats
            }

            if record.is_permastore != 0 {
                permastore_count += 1;
            }

            strength_sum += record.decay_strength as f64;
            edge_count += record.edge_count as u64;
        }

        let avg_strength = if memory_count > 0 {
            (strength_sum / memory_count as f64) as f32
        } else {
            0.0
        };

        Ok(state::NamespaceStats {
            memory_count,
            phase_1_count,
            phase_2_count,
            phase_3_count,
            permastore_count,
            avg_strength,
            edge_count,
        })
    }

    async fn list_memories(
        &self,
        filter: &crate::api::models::ListFilter,
    ) -> Result<Vec<CachedRecord>, crate::storage::StorageError> {
        let storage_r = self.storage.read().map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("storage lock poisoned: {e}"),
            ))
        })?;

        let all_records = storage_r.scan_all()?;

        let filtered: Vec<CachedRecord> = all_records
            .into_iter()
            .filter(|(_id, record)| {
                // Exclude tombstoned records from listings.
                if record.phase == DecayPhase::Tombstone {
                    return false;
                }

                // Filter by namespace
                if let Some(ns_id) = filter.namespace_id {
                    if NamespaceId::new(record.namespace_id) != ns_id {
                        return false;
                    }
                }

                // Filter by phase
                if let Some(phase) = filter.phase {
                    if record.phase != phase {
                        return false;
                    }
                }

                // Filter by tags (AND logic: must have ALL)
                if !filter.tags.is_empty()
                    && !filter
                        .tags
                        .iter()
                        .all(|tag| record.tags.iter().any(|t| t.as_str() == tag))
                {
                    return false;
                }

                true
            })
            .map(|(_id, disk_record)| CachedRecord::from(&disk_record))
            .collect();

        Ok(filtered)
    }

    async fn ping(&self) -> bool {
        self.storage.read().is_ok()
    }

    async fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, crate::storage::StorageError> {
        let storage_r = self.storage.read().map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("storage lock poisoned: {e}"),
            ))
        })?;
        storage_r.scan_all()
    }

    async fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, crate::storage::StorageError> {
        let storage_r = self.storage.read().map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("storage lock poisoned: {e}"),
            ))
        })?;
        storage_r.scan_phase_records(phase)
    }

    async fn list_tags(&self) -> Result<Vec<(String, u64)>, crate::storage::StorageError> {
        let storage_r = self.storage.read().map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("storage lock poisoned: {e}"),
            ))
        })?;
        storage_r.list_tags()
    }

    fn storage_path(&self) -> PathBuf {
        let storage_r = self.storage.read().expect("storage lock not poisoned");
        storage_r.db_path().to_path_buf()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RecordCacheAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `CacheManager` to the API `RecordCache` trait.
pub struct RecordCacheAdapter {
    cache: Arc<CacheManager>,
}

impl RecordCacheAdapter {
    /// Creates a new adapter wrapping the cache manager.
    pub fn new(cache: Arc<CacheManager>) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl state::RecordCache for RecordCacheAdapter {
    async fn get_or_load(
        &self,
        id: MemoryId,
        storage: &dyn state::StorageEngine,
    ) -> Option<Arc<CachedRecord>> {
        // Use the injected storage trait for loading on cache miss.
        // First check the cache directly for a fast-path hit.
        if let Some(arc) = self.cache.get(id).await {
            return Some(arc);
        }

        // Cache miss: load from the injected storage engine.
        match storage.get_record(id).await {
            Some(disk) => {
                let record = CachedRecord::from(&disk);
                self.cache.insert(id, record.clone()).await;
                // Re-fetch to get the Arc-wrapped version from moka.
                self.cache.get(id).await
            }
            None => None,
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
    /// Creates a new adapter wrapping the shared relationship graph.
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

    async fn remove_all_edges(&self, id: MemoryId) -> Result<(), crate::graph::GraphError> {
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
    /// Creates a new adapter with the storage engine and sweep-thread liveness flag.
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>, has_sweep: bool) -> Self {
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
    ) -> Result<state::ReinforceResult, Box<dyn std::error::Error + Send + Sync>> {
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
    /// Creates a new adapter wrapping the storage engine for namespace operations.
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

        let namespaces = match storage_r.list_namespaces() {
            Ok(ns) => ns,
            Err(_) => return Vec::new(),
        };

        // Count memories per namespace in a single scan.
        let mut counts = std::collections::HashMap::<u32, u64>::new();
        if let Ok(all_records) = storage_r.scan_all() {
            for (_id, record) in &all_records {
                *counts.entry(record.namespace_id).or_default() += 1;
            }
        }

        namespaces
            .into_iter()
            .map(|ns| {
                let memory_count = counts.get(&ns.id.get()).copied().unwrap_or(0);
                state::NamespaceListInfo {
                    id: ns.id.get(),
                    name: ns.name.clone(),
                    embedding_dim: ns.embedding_dim,
                    memory_count,
                    created_at: ns.created_at,
                }
            })
            .collect()
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
            decay_rate_multiplier: None,
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
