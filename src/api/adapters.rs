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
use crate::config::RecalldConfig;
use crate::embedding::EmbeddingProvider;
use crate::graph::SharedGraph;
use crate::model::{
    AccessKind, CachedRecord, DecayPhase, DiskRecord, MemoryId, NamespaceConfig, NamespaceId,
    Tag, parse_structured_tags,
};
use crate::search::{EntityIndex, FlatVectorIndex, FtsIndex};
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
    fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
}

impl SearchPipelineAdapter {
    /// Creates a new adapter from the real search subsystem components.
    pub fn new(
        query_engine: Arc<crate::search::QueryEngine>,
        embedding: Arc<dyn EmbeddingProvider>,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
        entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
    ) -> Self {
        Self {
            query_engine,
            embedding,
            vector_index,
            fts_index,
            entity_index,
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

        // Build proper filter from query tags/entities (Issue 2)
        let mut require_tags: Vec<Tag> = query
            .include_tags
            .iter()
            .filter_map(|t| Tag::new(t).ok())
            .collect();
        // Convert entities to entity/ tags (Issue 4)
        for e in &query.entities {
            if let Ok(tag) = Tag::new(&format!("entity/{}", e.to_lowercase())) {
                require_tags.push(tag);
            }
        }
        let exclude_tags: Vec<Tag> = query
            .exclude_tags
            .iter()
            .filter_map(|t| Tag::new(t).ok())
            .collect();
        let phases: Vec<DecayPhase> = query
            .decay_phases
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter_map(|&p| match p {
                        0 => Some(DecayPhase::Full),
                        1 => Some(DecayPhase::Summary),
                        2 => Some(DecayPhase::Ghost),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let pipeline_query = crate::search::SearchQuery {
            text,
            fts_query: None,
            namespace: query.namespace_name.clone(), // Issue 1: pass actual namespace name
            filter: crate::search::PipelineSearchFilter {
                require_tags,
                exclude_tags,
                phases,
                min_strength: query.min_score, // Issue 2: pass min_strength
                ..Default::default()
            },
            limit: query.k,
            min_score: 0.0,
            include_ghosts: false,
            query_mode: crate::search::QueryMode::default(),
            graph_depth: (query.graph_depth as u8).min(3),
            time_range_start: query.time_range_start, // Issue 3: pass time range
            time_range_end: query.time_range_end,      // Issue 3: pass time range
            entities: query.entities.clone(),           // Issue 4: pass entities
        };
        let response = self.query_engine.search(pipeline_query).await?;
        Ok(response
            .results
            .into_iter()
            .map(|r| {
                // Issue 5: Parse entities from tags
                let metadata = parse_structured_tags(&r.tags);
                state::ResolvedSearchResult {
                    memory: CachedRecord {
                        id: r.memory_id,
                        namespace_id: query.namespace_id, // Issue 6: use actual namespace_id
                        created_at: r.created_at,
                        last_accessed_at: r.last_accessed_at,
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
                        entities: metadata.entities,
                    },
                    score: r.score.unwrap_or(0.0),
                }
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

    async fn fts_add(
        &self,
        namespace_id: NamespaceId,
        memory_id: MemoryId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
    ) {
        let fts = self.fts_index.lock().await;
        if let Err(e) = fts.add(namespace_id, memory_id, summary, full_text, tags) {
            tracing::warn!(
                memory_id = %memory_id,
                %e,
                "FTS5 indexing failed (non-fatal)"
            );
        }
    }

    async fn fts_remove(&self, memory_id: MemoryId) {
        let fts = self.fts_index.lock().await;
        if let Err(e) = fts.remove(memory_id) {
            tracing::warn!(
                memory_id = %memory_id,
                %e,
                "FTS5 removal failed (non-fatal)"
            );
        }
    }

    async fn entity_index_add(&self, memory_id: MemoryId, entities: &[String]) {
        if !entities.is_empty() {
            let mut idx = self.entity_index.write().await;
            idx.add(memory_id, entities);
        }
    }

    async fn entity_index_remove(&self, memory_id: MemoryId, entities: &[String]) {
        if !entities.is_empty() {
            let mut idx = self.entity_index.write().await;
            idx.remove(memory_id, entities);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// StorageEngineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` to the API `StorageEngine` trait.
pub struct StorageEngineAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
}

impl StorageEngineAdapter {
    /// Creates a new adapter wrapping the storage engine and cache.
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
    ) -> Self {
        Self { storage, cache }
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
        created_at: Option<i64>,
    ) -> Result<CachedRecord, crate::storage::StorageError> {
        use crate::model::Tag;
        use crate::model::record::DiskRecord;

        let id = MemoryId::new();
        let now = chrono::Utc::now().timestamp_millis();
        let ts = created_at.unwrap_or(now);
        let parsed_tags: Vec<Tag> = tags.iter().filter_map(|t| Tag::new(t).ok()).collect();

        let mut record = DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: namespace_id.get(),
            created_at: ts,
            last_accessed_at: ts,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability: initial_stability.unwrap_or(3.7145),
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

        let storage = self.storage.clone();
        let embedding_vec = embedding.to_vec();
        let full_text_owned = full_text.map(|s| s.to_string());

        let record = tokio::task::spawn_blocking(move || {
            let mut storage_w = storage.write().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_w.insert_memory(
                id,
                namespace_id,
                &mut record,
                &embedding_vec,
                full_text_owned.as_deref(),
            )?;
            Ok::<_, crate::storage::StorageError>(record)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })??;

        let cached = CachedRecord::from(&record);
        self.cache.insert(id, cached.clone()).await;
        Ok(cached)
    }

    async fn get_record(&self, id: MemoryId) -> Option<DiskRecord> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().ok()?;
            storage_r.get_record(id).ok().flatten()
        })
        .await
        .ok()
        .flatten()
    }

    async fn delete_memory(&self, id: MemoryId) -> Result<bool, crate::storage::StorageError> {
        let storage = self.storage.clone();

        // Perform read, tombstone check, tombstone, and free_vector_slot
        // atomically under a single write lock to prevent TOCTOU races.
        // Without this, two concurrent deletes could both read the record
        // as non-tombstoned, both tombstone it, and both free the same
        // vector slot -- corrupting the free list into a self-referential
        // cycle.
        let deleted = tokio::task::spawn_blocking(move || {
            let mut storage_w = storage.write().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;

            // Check if the record exists and is not already tombstoned.
            let existing_record = match storage_w.get_record(id)? {
                Some(r) => r,
                None => return Ok::<bool, crate::storage::StorageError>(false),
            };

            if existing_record.phase == DecayPhase::Tombstone {
                return Ok(false);
            }

            // Tombstone the record.
            storage_w.tombstone_memory(id)?;

            // Free the vector slot.
            let ns_id = NamespaceId::new(existing_record.namespace_id);
            if let Err(e) = storage_w.free_vector_slot(ns_id, existing_record.vector_slot) {
                tracing::warn!(
                    memory_id = %id,
                    vector_slot = existing_record.vector_slot,
                    %e,
                    "vector slot free failed (non-fatal)"
                );
            }

            Ok(true)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })??;

        if deleted {
            self.cache.invalidate(id).await;
        }
        Ok(deleted)
    }

    async fn namespace_stats(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<state::NamespaceStats, crate::storage::StorageError> {
        let storage = self.storage.clone();

        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
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
                    DecayPhase::Tombstone => {}
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
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn list_memories(
        &self,
        filter: &crate::api::models::ListFilter,
    ) -> Result<Vec<CachedRecord>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        let filter = filter.clone();

        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;

            let all_records = storage_r.scan_all()?;

            let filtered: Vec<CachedRecord> = all_records
                .into_iter()
                .filter(|(_id, record)| {
                    if record.phase == DecayPhase::Tombstone {
                        return false;
                    }
                    if let Some(ns_id) = filter.namespace_id {
                        if NamespaceId::new(record.namespace_id) != ns_id {
                            return false;
                        }
                    }
                    if let Some(phase) = filter.phase {
                        if record.phase != phase {
                            return false;
                        }
                    }
                    if !filter.tags.is_empty()
                        && !filter
                            .tags
                            .iter()
                            .all(|tag| record.tags.iter().any(|t| t.as_str() == tag))
                    {
                        return false;
                    }
                    if let Some(start) = filter.time_range_start {
                        if record.created_at < start {
                            return false;
                        }
                    }
                    if let Some(end) = filter.time_range_end {
                        if record.created_at > end {
                            return false;
                        }
                    }
                    true
                })
                .map(|(_id, disk_record)| CachedRecord::from(&disk_record))
                .collect();

            Ok(filtered)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn get_full_text(
        &self,
        id: MemoryId,
    ) -> Result<Option<String>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            let record = match storage_r.get_record(id)? {
                Some(r) => r,
                None => return Ok(None),
            };
            if record.text_length == 0 {
                return Ok(None);
            }
            let text_ref = crate::storage::TextRef {
                file_offset: record.text_offset,
                length: record.text_length,
            };
            storage_r.get_text(text_ref)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn list_memories_filtered(
        &self,
        namespace_id: NamespaceId,
        require_tags: &[Tag],
        time_range_start: Option<i64>,
        time_range_end: Option<i64>,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<(MemoryId, DiskRecord)>, u64), crate::storage::StorageError> {
        let storage = self.storage.clone();
        let tags_owned: Vec<Tag> = require_tags.to_vec();

        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r
                .meta_store()
                .list_memories_filtered(
                    namespace_id,
                    &tags_owned,
                    time_range_start,
                    time_range_end,
                    offset,
                    limit,
                )
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn ping(&self) -> bool {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.read().is_ok())
            .await
            .unwrap_or(false)
    }

    async fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.scan_all()
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.scan_phase_records(phase)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn list_tags(&self) -> Result<Vec<(String, u64)>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.list_tags()
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    fn storage_path(&self) -> Result<PathBuf, crate::storage::StorageError> {
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            Ok(storage_r.db_path().to_path_buf())
        })
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
    vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    config: Arc<RecalldConfig>,
}

impl RelationshipGraphAdapter {
    /// Creates a new adapter wrapping the shared relationship graph and
    /// subsystems needed for post-creation linking.
    pub fn new(
        graph: SharedGraph,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        config: Arc<RecalldConfig>,
    ) -> Self {
        Self {
            graph,
            vector_index,
            entity_index,
            storage,
            cache,
            config,
        }
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
            "parent" | "parent_child" | "ParentChild" => EdgeType::ParentChild,
            "supersedes" | "Supersedes" => EdgeType::Supersedes,
            "associative" | "Associative" => EdgeType::Associative,
            "causal" | "Causal" => EdgeType::Causal,
            "contradicts" | "Contradicts" => EdgeType::Contradicts,
            _ => EdgeType::Associative,
        };
        let mut graph = self.graph.write().await;
        graph.add_edge(from, to, etype, 1.0, false)?;

        // Persist edge to storage.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let persisted = crate::storage::PersistedEdge {
            source: from,
            target: to,
            edge_type: etype,
            weight: 1.0,
            auto_created: false,
            created_at: now_ms,
        };
        drop(graph); // Release graph lock before storage

        let storage = self.storage.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(storage_r) = storage.read() {
                let _ = storage_r.batch_add_edges(&[persisted]);
            }
        })
        .await;

        Ok(())
    }

    async fn tombstone_node(&self, id: MemoryId) -> Result<(), crate::graph::GraphError> {
        let mut graph = self.graph.write().await;
        graph.update_node_state(id, DecayPhase::Tombstone, 0.0)?;
        Ok(())
    }

    async fn add_node(
        &self,
        id: MemoryId,
        namespace_id: NamespaceId,
        phase: DecayPhase,
        strength: f32,
        vector_slot: u32,
    ) -> Result<(), crate::graph::GraphError> {
        let mut graph = self.graph.write().await;
        let _ = graph.add_node(id, namespace_id, phase, strength, vector_slot);
        Ok(())
    }

    async fn perform_post_creation_links(
        &self,
        memory_id: MemoryId,
        namespace_id: NamespaceId,
        embedding: &[f32],
        tags: &[String],
        entities: &[String],
        created_at: i64,
    ) {
        if !self.config.graph.autolink_enabled {
            return;
        }

        let threshold = self.config.graph.auto_link_threshold as f32;
        let max_links = self.config.graph.max_auto_links;

        // Autolink: discover and create edges to similar existing memories.
        if let Err(e) = crate::graph::perform_autolink(
            memory_id,
            namespace_id,
            embedding,
            tags,
            threshold,
            max_links,
            &self.vector_index,
            &self.graph,
            &self.storage,
            &self.cache,
        )
        .await
        {
            tracing::warn!(
                memory_id = %memory_id,
                %e,
                "autolink failed (non-fatal)"
            );
        }

        // Entity linking: connect memories sharing named entities.
        if !entities.is_empty() {
            let max_entity_links = self.config.graph.max_entity_links;
            let _ = crate::graph::perform_entity_link(
                memory_id,
                namespace_id,
                entities,
                max_entity_links,
                &self.entity_index,
                &self.graph,
                &self.storage,
                &self.cache,
            )
            .await;
        }

        // Temporal linking: connect to recently-stored memories.
        let temporal_window_ms = self.config.graph.temporal_window_ms;
        let max_temporal_links = self.config.graph.max_temporal_links;
        if temporal_window_ms > 0 {
            let recent_memories: Vec<(MemoryId, i64)> = self
                .cache
                .iter()
                .filter_map(|(mid, record)| {
                    if mid == memory_id {
                        return None;
                    }
                    if record.namespace_id != namespace_id {
                        return None;
                    }
                    let delta = (created_at - record.created_at).unsigned_abs();
                    if delta <= temporal_window_ms {
                        Some((mid, record.created_at))
                    } else {
                        None
                    }
                })
                .collect();

            if !recent_memories.is_empty() {
                if let Err(e) = crate::graph::perform_temporal_link(
                    memory_id,
                    namespace_id,
                    created_at,
                    temporal_window_ms,
                    max_temporal_links,
                    &recent_memories,
                    &self.graph,
                    &self.storage,
                    &self.cache,
                )
                .await
                {
                    tracing::warn!(
                        memory_id = %memory_id,
                        %e,
                        "temporal link failed (non-fatal)"
                    );
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FsrsEngineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts the decay subsystem to the API `FsrsEngine` trait.
pub struct FsrsEngineAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    graph: crate::graph::SharedGraph,
    config: Arc<RecalldConfig>,
    sweep_alive: bool,
}

impl FsrsEngineAdapter {
    /// Creates a new adapter with the storage engine, cache, and sweep-thread liveness flag.
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        graph: crate::graph::SharedGraph,
        config: Arc<RecalldConfig>,
        has_sweep: bool,
    ) -> Self {
        Self {
            storage,
            cache,
            graph,
            config,
            sweep_alive: has_sweep,
        }
    }
}

#[async_trait]
impl state::FsrsEngine for FsrsEngineAdapter {
    async fn record_access(&self, id: MemoryId, kind: AccessKind) {
        let now = chrono::Utc::now().timestamp_millis();
        let storage = self.storage.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(storage_r) = storage.read() {
                let _ = storage_r.update_access(id, now, kind);
            }
        })
        .await;
    }

    async fn reinforce(
        &self,
        id: MemoryId,
        quality: u8,
    ) -> Result<state::ReinforceResult, Box<dyn std::error::Error + Send + Sync>> {
        // Step 1: Read the current record.
        let record = {
            let storage = self.storage.clone();
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    format!("storage lock poisoned: {e}")
                })?;
                storage_r
                    .get_record(id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("memory {id} not found"))
            })
            .await
            .map_err(|e| format!("blocking task join error: {e}"))??
        };

        // Tombstoned memories cannot be reinforced.
        if record.phase == DecayPhase::Tombstone {
            return Err(format!("memory {id} has been deleted (tombstoned)").into());
        }

        let phase = record.phase;

        // Step 2: Look up the namespace's decay rate multiplier.
        let ns_decay_multiplier = {
            let storage = self.storage.clone();
            let ns_id = record.namespace_id;
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    format!("storage lock poisoned: {e}")
                })?;
                let multiplier = storage_r
                    .get_namespace(NamespaceId::new(ns_id))
                    .ok()
                    .flatten()
                    .and_then(|ns| ns.decay_rate_multiplier)
                    .unwrap_or(1.0);
                Ok::<f32, String>(multiplier)
            })
            .await
            .map_err(|e| format!("blocking task join error: {e}"))??
        };

        // Step 3: Compute elapsed days since last access.
        let now = chrono::Utc::now().timestamp_millis();
        let elapsed_millis = (now - record.last_accessed_at).max(0) as f64;
        let elapsed_days = (elapsed_millis / 86_400_000.0) as f32;

        // Step 4: Use FSRS to compute new stability based on quality rating.
        let decay_config = crate::decay::DecayConfig::default();
        let engine = crate::decay::FsrsEngine::new(&decay_config);
        let new_stability = engine.review_stability(
            record.stability,
            elapsed_days,
            quality,
            ns_decay_multiplier,
        );

        // Step 5: Compute new retrievability (just reinforced = 1.0).
        let new_strength = 1.0_f32;
        let is_permastore =
            record.is_permastore != 0 || new_stability >= decay_config.permastore_threshold;

        // Step 6: Persist the updated decay state and access event.
        {
            let storage = self.storage.clone();
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    format!("storage lock poisoned: {e}")
                })?;
                storage_r
                    .update_decay_state(id, phase, new_strength, new_strength, new_stability, is_permastore)
                    .map_err(|e| e.to_string())?;
                storage_r
                    .update_access(id, now, crate::model::AccessKind::ManualReinforcement)
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| format!("blocking task join error: {e}"))??;
        }

        // Step 7: Update cache.
        if let Some(existing) = self.cache.get(id).await {
            let mut updated = (*existing).clone();
            updated.stability = new_stability;
            updated.strength = new_strength;
            updated.decay_strength = new_strength;
            updated.is_permastore = is_permastore;
            updated.last_accessed_at = now;
            self.cache.insert(id, updated).await;
        }

        Ok(state::ReinforceResult {
            strength: new_strength,
            stability: new_stability,
            phase,
            is_permastore,
        })
    }

    fn sweep_thread_alive(&self) -> bool {
        self.sweep_alive
    }

    fn last_sweep_time(&self) -> Option<std::time::Instant> {
        None
    }

    async fn trigger_sweep(
        &self,
        as_of_millis: Option<i64>,
    ) -> Result<crate::decay::SweepResult, Box<dyn std::error::Error + Send + Sync>> {
        use crate::decay::sweep::{DecaySweepRunner, SweepConfig};
        use crate::graph::ActivationConfig;

        let sweep_config = SweepConfig::default();
        let decay_config = crate::decay::DecayConfig::default();
        let activation_config = ActivationConfig::default();
        let global_multiplier = self.config.decay.decay_rate_multiplier as f64;

        let result = DecaySweepRunner::execute_sweep_at(
            &sweep_config,
            &decay_config,
            &activation_config,
            &self.storage,
            &self.graph,
            &self.cache,
            global_multiplier,
            as_of_millis,
        )
        .await;

        Ok(result)
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
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().ok()?;
            storage_r.get_namespace_by_name(name).ok().flatten()
        })
    }

    fn get_by_id(&self, id: u32) -> Option<NamespaceConfig> {
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().ok()?;
            storage_r.get_namespace(NamespaceId::new(id)).ok().flatten()
        })
    }

    fn name_for(&self, id: NamespaceId) -> Option<String> {
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().ok()?;
            storage_r
                .get_namespace(id)
                .ok()
                .flatten()
                .map(|ns| ns.name.clone())
        })
    }

    async fn list_all(&self) -> Vec<state::NamespaceListInfo> {
        let storage = self.storage.clone();

        tokio::task::spawn_blocking(move || {
            let storage_r = match storage.read() {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };

            let namespaces = match storage_r.list_namespaces() {
                Ok(ns) => ns,
                Err(_) => return Vec::new(),
            };

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
        })
        .await
        .unwrap_or_default()
    }

    async fn create(
        &self,
        name: &str,
        embedding_dim: Option<u32>,
        initial_stability: f32,
        desired_retention: f32,
        decay_rate_multiplier: Option<f32>,
    ) -> Result<NamespaceConfig, Box<dyn std::error::Error + Send + Sync>> {
        let now = chrono::Utc::now().timestamp_millis();
        let storage = self.storage.clone();
        let name_owned = name.to_string();

        tokio::task::spawn_blocking(move || {
            // Resolve embedding_dim: use provided value, or inherit from
            // the 'default' namespace, or fall back to 1536.
            let dim = embedding_dim.unwrap_or_else(|| {
                storage
                    .read()
                    .ok()
                    .and_then(|s| s.get_namespace_by_name("default").ok().flatten())
                    .map(|ns| ns.embedding_dim)
                    .unwrap_or(1536)
            });

            let ns_config = NamespaceConfig {
                id: NamespaceId::UNSET,
                name: name_owned,
                embedding_dim: dim,
                initial_stability,
                default_difficulty: 5.0,
                phase_thresholds: crate::model::namespace::PhaseThresholds::default(),
                permastore_threshold: 1500.0,
                created_at: now,
                desired_retention,
                decay_rate_multiplier,
            };

            let mut storage_w = storage.write().map_err(|e| {
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
        })
        .await
        .map_err(|e| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            )) as Box<dyn std::error::Error + Send + Sync>
        })?
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
