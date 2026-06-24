//! Adapter implementations connecting MCP bridge traits to real subsystems.
//!
//! Each adapter wraps the concrete subsystem type and implements the
//! corresponding MCP bridge trait from `bridge.rs`. These adapters
//! bridge between the MCP protocol layer and Recalld subsystem types.

use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::CacheManager;
use crate::config::RecalldConfig;
use crate::embedding::EmbeddingProvider;
use crate::graph::SharedGraph;
use crate::model::{DecayPhase, MemoryId, NamespaceId};
use crate::search::{EntityIndex, FlatVectorIndex, FtsIndex, QueryEngine};
use crate::storage::RedbStorageEngine;
use crate::time::format_timestamp;
// Import the StorageEngine trait so its methods are in scope.
use super::bridge;
use crate::storage::StorageEngine as StorageEngineTrait;

// ═══════════════════════════════════════════════════════════════════════
// McpSearchAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `QueryEngine` + subsystems to the MCP `SearchPipeline` trait.
pub struct McpSearchAdapter {
    query_engine: Arc<QueryEngine>,
    #[allow(dead_code)]
    embedding: Arc<dyn EmbeddingProvider>,
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    graph: SharedGraph,
    timezone: chrono_tz::Tz,
}

impl McpSearchAdapter {
    /// Create a new search adapter from the given subsystems.
    pub fn new(
        query_engine: Arc<QueryEngine>,
        embedding: Arc<dyn EmbeddingProvider>,
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: SharedGraph,
        timezone: chrono_tz::Tz,
    ) -> Self {
        Self {
            query_engine,
            embedding,
            storage,
            graph,
            timezone,
        }
    }
}

#[async_trait]
impl bridge::SearchPipeline for McpSearchAdapter {
    async fn search(
        &self,
        query: bridge::SearchInput,
    ) -> Result<bridge::SearchResponse, bridge::BridgeError> {
        // Resolve namespace name to verify it exists.
        {
            let storage_r = self.storage.read().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            storage_r
                .get_namespace_by_name(&query.namespace)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    bridge::BridgeError::NotFound(format!(
                        "namespace '{}' not found",
                        query.namespace,
                    ))
                })?;
        };

        let mut require_tags: Vec<crate::model::Tag> = query
            .tags
            .iter()
            .filter_map(|t| crate::model::Tag::new(t).ok())
            .collect();
        for e in &query.entities {
            if let Ok(tag) = crate::model::Tag::new(&format!("entity/{}", e.to_lowercase())) {
                require_tags.push(tag);
            }
        }
        for t in &query.topics {
            if let Ok(tag) = crate::model::Tag::new(&format!("topic/{}", t.to_lowercase())) {
                require_tags.push(tag);
            }
        }
        for em in &query.emotions {
            if let Ok(tag) = crate::model::Tag::new(&format!("emotion/{}", em.to_lowercase())) {
                require_tags.push(tag);
            }
        }

        let pipeline_query = crate::search::SearchQuery {
            text: Some(query.query),
            fts_query: None,
            namespace: query.namespace.clone(),
            filter: crate::search::PipelineSearchFilter {
                require_tags,
                ..Default::default()
            },
            limit: query.limit,
            min_score: query.min_strength.unwrap_or(0.0),
            include_ghosts: false,
            query_mode: crate::search::QueryMode::default(),
            graph_depth: query.depth.min(3) as u8,
            time_range_start: query.time_range_start,
            time_range_end: query.time_range_end,
            entities: query.entities.clone(),
        };

        let response = self
            .query_engine
            .search(pipeline_query)
            .await
            .map_err(|e| bridge::BridgeError::Search(e.to_string()))?;

        let full_texts: std::collections::HashMap<crate::model::MemoryId, String> = {
            let mut storage_w = self.storage.write().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            let mut map = std::collections::HashMap::new();
            for r in &response.results {
                if let Ok(Some(disk)) = storage_w.get_record(r.memory_id) {
                    if disk.text_length > 0 {
                        let text_ref = crate::storage::TextRef {
                            file_offset: disk.text_offset,
                            length: disk.text_length,
                        };
                        if let Ok(Some(text)) = storage_w.get_text(text_ref) {
                            map.insert(r.memory_id, text);
                        }
                    }
                }
            }
            map
        };

        let result_ids: std::collections::HashSet<crate::model::MemoryId> =
            response.results.iter().map(|r| r.memory_id).collect();

        let graph_r = self.graph.read().await;

        let hits: Vec<bridge::SearchHit> = response
            .results
            .into_iter()
            .map(|r| {
                let full_text = full_texts.get(&r.memory_id).cloned();

                let related = graph_r
                    .edges_for(&r.memory_id)
                    .iter()
                    .filter_map(|edge| {
                        let src_key = graph_r.resolve(&r.memory_id)?;
                        let other_key = if edge.source == src_key {
                            edge.target
                        } else {
                            edge.source
                        };
                        let neighbor = graph_r.get_node_by_key(other_key)?;
                        if result_ids.contains(&neighbor.memory_id) {
                            Some(bridge::RelatedMemory {
                                id: neighbor.memory_id.to_string(),
                                edge_type: format!("{:?}", edge.edge_type).to_lowercase(),
                                weight: edge.weight,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();

                let metadata = crate::model::parse_structured_tags(&r.tags);

                let tz = self.timezone;
                bridge::SearchHit {
                    id: r.memory_id.to_string(),
                    summary: r.summary.unwrap_or_default(),
                    full_text,
                    score: r.score.unwrap_or(0.0),
                    namespace: query.namespace.clone(),
                    tags: r.tags.iter().map(|t| t.to_string()).collect(),
                    entities: metadata.entities,
                    topics: metadata.topics,
                    emotions: metadata.emotions,
                    phase: format!("{:?}", r.phase),
                    strength: r.retrievability,
                    created_at: r.created_at,
                    created_at_formatted: Some(format_timestamp(r.created_at, tz)),
                    last_accessed_at: r.created_at,
                    last_accessed_at_formatted: Some(format_timestamp(r.created_at, tz)),
                    related,
                }
            })
            .collect();

        const MAX_NEIGHBORS: usize = 10;
        const FULL_TEXT_NEIGHBORS: usize = 5;

        let mut neighbor_weights: std::collections::HashMap<
            crate::model::MemoryId,
            (f32, String, String),
        > = std::collections::HashMap::new();

        for &mid in &result_ids {
            let Some(src_key) = graph_r.resolve(&mid) else {
                continue;
            };
            for edge in graph_r.edges_for(&mid) {
                let other_key = if edge.source == src_key {
                    edge.target
                } else {
                    edge.source
                };
                if let Some(neighbor_node) = graph_r.get_node_by_key(other_key) {
                    if !result_ids.contains(&neighbor_node.memory_id) {
                        let entry = neighbor_weights.entry(neighbor_node.memory_id).or_insert((
                            0.0,
                            format!("{:?}", edge.edge_type).to_lowercase(),
                            mid.to_string(),
                        ));
                        if edge.weight > entry.0 {
                            *entry = (
                                edge.weight,
                                format!("{:?}", edge.edge_type).to_lowercase(),
                                mid.to_string(),
                            );
                        }
                    }
                }
            }
        }
        drop(graph_r);

        let mut sorted: Vec<(crate::model::MemoryId, f32, String, String)> = neighbor_weights
            .into_iter()
            .map(|(id, (w, et, ct))| (id, w, et, ct))
            .collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        sorted.truncate(MAX_NEIGHBORS);

        let top_ft_ids: std::collections::HashSet<crate::model::MemoryId> = sorted
            .iter()
            .take(FULL_TEXT_NEIGHBORS)
            .map(|(id, _, _, _)| *id)
            .collect();

        let neighbors: Vec<bridge::NeighborMemory> = {
            let mut storage_w = self.storage.write().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;

            sorted
                .into_iter()
                .filter_map(|(mid, weight, edge_type, connected_to)| {
                    let disk = storage_w.get_record(mid).ok()??;
                    if disk.summary.is_empty() {
                        return None;
                    }
                    let full_text = if top_ft_ids.contains(&mid) && disk.text_length > 0 {
                        let text_ref = crate::storage::TextRef {
                            file_offset: disk.text_offset,
                            length: disk.text_length,
                        };
                        storage_w.get_text(text_ref).ok().flatten()
                    } else {
                        None
                    };
                    let metadata = crate::model::parse_structured_tags(&disk.tags);

                    Some(bridge::NeighborMemory {
                        id: mid.to_string(),
                        summary: disk.summary.clone(),
                        full_text,
                        topics: metadata.topics,
                        emotions: metadata.emotions,
                        edge_type,
                        weight,
                        connected_to,
                    })
                })
                .collect()
        };

        Ok(bridge::SearchResponse { hits, neighbors })
    }

    async fn find_similar(
        &self,
        id: MemoryId,
        limit: usize,
        min_score: Option<f32>,
        _same_namespace: bool,
    ) -> Result<Vec<bridge::SearchHit>, bridge::BridgeError> {
        let results = self
            .query_engine
            .similar(id, limit)
            .await
            .map_err(|e| bridge::BridgeError::Search(e.to_string()))?;

        Ok(results
            .into_iter()
            .filter(|r| min_score.map_or(true, |ms| r.score.unwrap_or(0.0) >= ms))
            .map(|r| {
                let metadata = crate::model::parse_structured_tags(&r.tags);

                let tz = self.timezone;
                bridge::SearchHit {
                    id: r.memory_id.to_string(),
                    summary: r.summary.unwrap_or_default(),
                    full_text: r.full_text,
                    score: r.score.unwrap_or(0.0),
                    namespace: r.namespace,
                    tags: r.tags.iter().map(|t| t.to_string()).collect(),
                    entities: metadata.entities,
                    topics: metadata.topics,
                    emotions: metadata.emotions,
                    phase: format!("{:?}", r.phase),
                    strength: r.retrievability,
                    created_at: r.created_at,
                    created_at_formatted: Some(format_timestamp(r.created_at, tz)),
                    last_accessed_at: r.created_at,
                    last_accessed_at_formatted: Some(format_timestamp(r.created_at, tz)),
                    related: Vec::new(),
                }
            })
            .collect())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// McpStorageAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` + subsystems to the MCP `StorageEngine` trait.
pub struct McpStorageAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    embedding: Arc<dyn EmbeddingProvider>,
    vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
    graph: SharedGraph,
    config: Arc<RecalldConfig>,
    timezone: chrono_tz::Tz,
}

impl McpStorageAdapter {
    /// Create a new storage adapter from the given subsystems.
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        embedding: Arc<dyn EmbeddingProvider>,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
        entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
        graph: SharedGraph,
        config: Arc<RecalldConfig>,
        timezone: chrono_tz::Tz,
    ) -> Self {
        Self {
            storage,
            cache,
            embedding,
            vector_index,
            fts_index,
            entity_index,
            graph,
            config,
            timezone,
        }
    }
}

#[async_trait]
impl bridge::StorageEngine for McpStorageAdapter {
    async fn store_memory(
        &self,
        input: bridge::StoreInput,
    ) -> Result<bridge::StoredMemory, bridge::BridgeError> {
        use crate::model::Tag;
        use crate::model::record::DiskRecord;

        // Resolve namespace.
        let ns_config = {
            let storage_r = self.storage.read().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            storage_r
                .get_namespace_by_name(&input.namespace)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    bridge::BridgeError::NotFound(format!(
                        "namespace '{}' not found",
                        input.namespace,
                    ))
                })?
        };

        // Convert structured metadata fields to tags and merge with explicit tags.
        let mut merged_tags = input.tags.clone();
        for entity in &input.entities {
            let tag = format!("entity/{}", entity.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }
        for topic in &input.topics {
            let tag = format!("topic/{}", topic.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }
        for emotion in &input.emotions {
            let tag = format!("emotion/{}", emotion.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }

        // Generate embedding if not provided (summary + full_text + tags for max surface).
        let embedding = match input.embedding {
            Some(emb) => emb,
            None => {
                let mut embed_text = match &input.full_text {
                    Some(ft) => format!("{}\n\n{}", input.summary, ft),
                    None => input.summary.clone(),
                };
                if !merged_tags.is_empty() {
                    embed_text = format!("{} {}", embed_text, merged_tags.join(" "));
                }
                self.embedding
                    .embed(&embed_text)
                    .await
                    .map_err(|e| bridge::BridgeError::Internal(e.to_string()))?
            }
        };

        let memory_id = MemoryId::new();
        let now = chrono::Utc::now().timestamp_millis();
        let parsed_tags: Vec<Tag> = merged_tags
            .iter()
            .filter_map(|t| Tag::new(t).ok())
            .collect();

        let mut record = DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: *memory_id.as_bytes(),
            namespace_id: ns_config.id.get(),
            created_at: now,
            last_accessed_at: now,
            phase: DecayPhase::Full.as_u8(),
            strength: 1.0,
            decay_strength: 1.0,
            stability: input
                .initial_stability
                .unwrap_or(ns_config.initial_stability),
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: input.summary.clone(),
            tags: parsed_tags,
            access_history: Vec::new(),
            text_offset: 0,
            text_length: 0,
        };

        // Insert into storage.
        {
            let mut storage_w = self.storage.write().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            storage_w
                .insert_memory(
                    memory_id,
                    ns_config.id,
                    &mut record,
                    &embedding,
                    input.full_text.as_deref(),
                )
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
        }

        // Insert into cache.
        let cached = crate::model::CachedRecord::from(&record);
        self.cache.insert(memory_id, cached).await;

        // Index the embedding in the vector index.
        {
            use crate::search::{VectorIndex, VectorMetadata};
            let mut index = self.vector_index.write().await;
            let metadata = VectorMetadata {
                namespace_id: ns_config.id,
                decay_phase: DecayPhase::Full.as_u8(),
                tags: merged_tags.clone(),
            };
            let _ = index.add(memory_id, &embedding, metadata);
        }

        // Index in FTS5 — summary, full_text, and tags as separate columns.
        {
            let fts = self.fts_index.lock().await;
            if let Err(e) = fts.add(
                ns_config.id,
                memory_id,
                &input.summary,
                input.full_text.as_deref(),
                &merged_tags,
            ) {
                tracing::warn!(
                    memory_id = %memory_id,
                    %e,
                    "FTS5 indexing failed (non-fatal)"
                );
            }
        }

        // Add node to the relationship graph (must happen BEFORE autolink).
        {
            let mut graph_w = self.graph.write().await;
            // Silently ignore DuplicateNode (should not happen for a new memory).
            let _ = graph_w.add_node(
                memory_id,
                ns_config.id,
                DecayPhase::Full,
                1.0,
                record.vector_slot,
            );

            // Create supersedes edge: new memory → old memory.
            if let Some(old_id) = input.supersedes {
                if let Err(e) = graph_w.add_edge(
                    memory_id,
                    old_id,
                    crate::model::EdgeType::Supersedes,
                    1.0,
                    false,
                ) {
                    tracing::warn!(
                        memory_id = %memory_id,
                        superseded = %old_id,
                        %e,
                        "supersedes edge failed (non-fatal)"
                    );
                }
            }
        }

        // Auto-link: discover and create edges to similar existing memories.
        if self.config.graph.autolink_enabled {
            let tag_strings: Vec<String> = merged_tags.clone();
            let threshold = self.config.graph.auto_link_threshold as f32;
            let max_links = self.config.graph.max_auto_links;

            if let Err(e) = crate::graph::perform_autolink(
                memory_id,
                ns_config.id,
                &embedding,
                &tag_strings,
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

            let entities = input.entities.clone();
            if !entities.is_empty() {
                let max_entity_links = self.config.graph.max_entity_links;
                let _ = crate::graph::perform_entity_link(
                    memory_id,
                    ns_config.id,
                    &entities,
                    max_entity_links,
                    &self.entity_index,
                    &self.graph,
                    &self.storage,
                    &self.cache,
                )
                .await;

                // Update entity index.
                let mut idx = self.entity_index.write().await;
                idx.add(memory_id, &entities);
            }

            // Temporal edges: link to recently-stored memories in the same namespace.
            let temporal_window_ms = self.config.graph.temporal_window_ms;
            let max_temporal_links = self.config.graph.max_temporal_links;
            if temporal_window_ms > 0 {
                let recent_memories: Vec<(MemoryId, i64)> = self
                    .cache
                    .iter()
                    .filter_map(|(mid, record)| {
                        if mid == memory_id {
                            return None; // skip self
                        }
                        if record.namespace_id != ns_config.id {
                            return None; // wrong namespace
                        }
                        let delta = (now - record.created_at).unsigned_abs();
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
                        ns_config.id,
                        now,
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

        Ok(bridge::StoredMemory {
            id: memory_id.to_string(),
            namespace: input.namespace,
            phase: "Full".to_string(),
            strength: record.strength,
            stability: record.stability,
            created_at: record.created_at,
            created_at_formatted: Some(format_timestamp(record.created_at, self.timezone)),
        })
    }

    async fn get_memory(
        &self,
        id: MemoryId,
    ) -> Result<Option<bridge::MemoryRecord>, bridge::BridgeError> {
        let mut storage_w = self
            .storage
            .write()
            .map_err(|e| bridge::BridgeError::Internal(format!("storage lock poisoned: {e}")))?;

        let disk_record = storage_w
            .get_record(id)
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

        match disk_record {
            Some(record) => {
                let ns_name = storage_w
                    .get_namespace(NamespaceId::new(record.namespace_id))
                    .ok()
                    .flatten()
                    .map(|ns| ns.name.clone())
                    .unwrap_or_default();

                let phase = DecayPhase::from_u8(record.phase).unwrap_or(DecayPhase::Full);

                let full_text = if record.text_length > 0 {
                    let text_ref = crate::storage::TextRef {
                        file_offset: record.text_offset,
                        length: record.text_length,
                    };
                    storage_w.get_text(text_ref).ok().flatten()
                } else {
                    None
                };

                let tz = self.timezone;
                Ok(Some(bridge::MemoryRecord {
                    id: id.to_string(),
                    namespace: ns_name,
                    summary: record.summary.clone(),
                    full_text,
                    tags: record.tags.iter().map(|t| t.to_string()).collect(),
                    phase: format!("{:?}", phase),
                    strength: record.strength,
                    stability: record.stability,
                    created_at: record.created_at,
                    created_at_formatted: Some(format_timestamp(record.created_at, tz)),
                    last_accessed_at: record.last_accessed_at,
                    last_accessed_at_formatted: Some(format_timestamp(
                        record.last_accessed_at,
                        tz,
                    )),
                    is_permastore: record.is_permastore != 0,
                    edge_count: record.edge_count,
                }))
            }
            None => Ok(None),
        }
    }

    async fn delete_memory(&self, id: MemoryId) -> Result<bool, bridge::BridgeError> {
        let deleted = {
            let mut storage_w = self.storage.write().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            storage_w
                .delete_memory(id)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
        };
        if deleted.is_some() {
            self.cache.invalidate(id).await;
            // Remove from FTS5 index. Single call — memory_id is globally
            // unique so namespace doesn't matter.
            let fts = self.fts_index.lock().await;
            if let Err(e) = fts.remove(id) {
                tracing::warn!(
                    memory_id = %id,
                    %e,
                    "FTS5 removal failed (non-fatal)"
                );
            }
        }
        Ok(deleted.is_some())
    }

    async fn reinforce_memory(
        &self,
        id: MemoryId,
        _quality: u8,
    ) -> Result<bridge::ReinforceResult, bridge::BridgeError> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| bridge::BridgeError::Internal(format!("storage lock poisoned: {e}")))?;
        let record = storage_r
            .get_record(id)
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
            .ok_or_else(|| bridge::BridgeError::NotFound(format!("memory {id} not found")))?;

        let phase = DecayPhase::from_u8(record.phase).unwrap_or(DecayPhase::Full);

        Ok(bridge::ReinforceResult {
            id: id.to_string(),
            strength: record.strength,
            stability: record.stability,
            phase: format!("{:?}", phase),
            is_permastore: record.is_permastore != 0,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
// McpNamespaceAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` to the MCP `NamespaceRegistry` trait.
pub struct McpNamespaceAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    timezone: chrono_tz::Tz,
}

impl McpNamespaceAdapter {
    /// Create a new namespace adapter wrapping the storage engine.
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>, timezone: chrono_tz::Tz) -> Self {
        Self { storage, timezone }
    }
}

#[async_trait]
impl bridge::NamespaceRegistry for McpNamespaceAdapter {
    async fn list_namespaces(&self) -> Result<Vec<bridge::NamespaceInfo>, bridge::BridgeError> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| bridge::BridgeError::Internal(format!("storage lock poisoned: {e}")))?;
        let namespaces = storage_r
            .list_namespaces()
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
        let tz = self.timezone;
        Ok(namespaces
            .into_iter()
            .map(|ns| bridge::NamespaceInfo {
                id: ns.id.get(),
                name: ns.name.clone(),
                embedding_dim: ns.embedding_dim as u16,
                memory_count: 0,
                created_at: ns.created_at,
                created_at_formatted: Some(format_timestamp(ns.created_at, tz)),
            })
            .collect())
    }

    async fn create_namespace(
        &self,
        input: bridge::CreateNamespaceInput,
    ) -> Result<bridge::NamespaceInfo, bridge::BridgeError> {
        use crate::model::NamespaceConfig;

        let now = chrono::Utc::now().timestamp_millis();
        let ns_config = NamespaceConfig {
            id: NamespaceId::UNSET,
            name: input.name.clone(),
            embedding_dim: input.embedding_dim.map(|d| d as u32).unwrap_or_else(|| {
                self.storage
                    .read()
                    .ok()
                    .and_then(|s| s.get_namespace_by_name("default").ok().flatten())
                    .map(|ns| ns.embedding_dim)
                    .unwrap_or(1536)
            }),
            initial_stability: input.initial_stability.unwrap_or(3.7),
            default_difficulty: 5.0,
            phase_thresholds: crate::model::namespace::PhaseThresholds::default(),
            permastore_threshold: 1500.0,
            created_at: now,
            desired_retention: input.desired_retention.unwrap_or(0.9),
            decay_rate_multiplier: input.decay_rate_multiplier,
        };

        let mut storage_w = self
            .storage
            .write()
            .map_err(|e| bridge::BridgeError::Internal(format!("storage lock poisoned: {e}")))?;
        let assigned_id = storage_w
            .create_namespace(&ns_config)
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

        Ok(bridge::NamespaceInfo {
            id: assigned_id.get(),
            name: input.name,
            embedding_dim: ns_config.embedding_dim as u16,
            memory_count: 0,
            created_at: now,
            created_at_formatted: Some(format_timestamp(now, self.timezone)),
        })
    }

    async fn namespace_stats(
        &self,
        name: &str,
    ) -> Result<bridge::NamespaceStats, bridge::BridgeError> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| bridge::BridgeError::Internal(format!("storage lock poisoned: {e}")))?;
        let _ns = storage_r
            .get_namespace_by_name(name)
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
            .ok_or_else(|| {
                bridge::BridgeError::NotFound(format!("namespace '{name}' not found"))
            })?;

        // Full stats would require iterating records. Return basic info.
        Ok(bridge::NamespaceStats {
            name: name.to_string(),
            memory_count: 0,
            phase_counts: bridge::PhaseCounts {
                full: 0,
                summary: 0,
                ghost: 0,
            },
            permastore_count: 0,
            avg_strength: 0.0,
            edge_count: 0,
            vector_bytes: 0,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
// McpHealthAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts subsystem health checks to the MCP `HealthChecker` trait.
pub struct McpHealthAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    started_at: std::time::Instant,
}

impl McpHealthAdapter {
    /// Create a new health adapter wrapping the storage engine.
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self {
            storage,
            started_at: std::time::Instant::now(),
        }
    }
}

#[async_trait]
impl bridge::HealthChecker for McpHealthAdapter {
    async fn check_health(&self) -> bridge::HealthStatus {
        let storage_ok = self.storage.read().is_ok();
        let subsystems = vec![bridge::SubsystemHealth {
            name: "storage".to_string(),
            status: if storage_ok { "ok" } else { "error" }.to_string(),
            message: None,
        }];
        let overall = if storage_ok { "ok" } else { "degraded" };
        bridge::HealthStatus {
            status: overall.to_string(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            subsystems,
        }
    }
}
