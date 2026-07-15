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
            let storage = self.storage.clone();
            let ns_name = query.namespace.clone();
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                storage_r
                    .get_namespace_by_name(&ns_name)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                    .ok_or_else(|| {
                        bridge::BridgeError::NotFound(format!("namespace '{}' not found", ns_name,))
                    })
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??;
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
                min_strength: query.min_strength,
                ..Default::default()
            },
            limit: query.limit,
            min_score: 0.0,
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
            let storage = self.storage.clone();
            let result_ids: Vec<crate::model::MemoryId> =
                response.results.iter().map(|r| r.memory_id).collect();
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                let mut map = std::collections::HashMap::new();
                for mid in &result_ids {
                    if let Ok(Some(disk)) = storage_r.get_record(*mid) {
                        if disk.text_length > 0 {
                            let text_ref = crate::storage::TextRef {
                                file_offset: disk.text_offset,
                                length: disk.text_length,
                            };
                            if let Ok(Some(text)) = storage_r.get_text(text_ref) {
                                map.insert(*mid, text);
                            }
                        }
                    }
                }
                Ok::<_, bridge::BridgeError>(map)
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
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
                    created_at: format_timestamp(r.created_at, tz),
                    last_accessed_at: format_timestamp(r.last_accessed_at, tz),
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
            let storage = self.storage.clone();
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;

                let result: Vec<bridge::NeighborMemory> = sorted
                    .into_iter()
                    .filter_map(|(mid, weight, edge_type, connected_to)| {
                        let disk = storage_r.get_record(mid).ok()??;
                        if disk.summary.is_empty() {
                            return None;
                        }
                        let full_text = if top_ft_ids.contains(&mid) && disk.text_length > 0 {
                            let text_ref = crate::storage::TextRef {
                                file_offset: disk.text_offset,
                                length: disk.text_length,
                            };
                            storage_r.get_text(text_ref).ok().flatten()
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
                    .collect();
                Ok::<_, bridge::BridgeError>(result)
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
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
                    created_at: format_timestamp(r.created_at, tz),
                    last_accessed_at: format_timestamp(r.last_accessed_at, tz),
                    related: Vec::new(),
                }
            })
            .collect())
    }

    async fn scan_duplicates(
        &self,
        namespace: &str,
        threshold: f32,
        max_memories: usize,
    ) -> Result<Vec<bridge::DuplicateCluster>, bridge::BridgeError> {
        // 1. Resolve namespace and get memory IDs.
        let storage = self.storage.clone();
        let ns_name = namespace.to_string();
        let (ns_config, memory_ids) = tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            let ns = storage_r
                .get_namespace_by_name(&ns_name)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    bridge::BridgeError::NotFound(format!("namespace '{ns_name}' not found"))
                })?;
            let all_ids = storage_r
                .meta_store()
                .memories_in_namespace(ns.id)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
            let ids: Vec<MemoryId> = all_ids.into_iter().take(max_memories).collect();
            Ok::<_, bridge::BridgeError>((ns, ids))
        })
        .await
        .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))??;

        if memory_ids.len() < 2 {
            return Ok(Vec::new());
        }

        // 3. Load embeddings for all sampled memories via the vector index.
        let mut id_embeddings: Vec<(MemoryId, Vec<f32>)> = Vec::with_capacity(memory_ids.len());
        for &mid in &memory_ids {
            if let Some(emb) = self.query_engine.get_vector(mid) {
                id_embeddings.push((mid, emb));
            }
        }

        if id_embeddings.len() < 2 {
            return Ok(Vec::new());
        }

        // 4. Union-Find based clustering: for each memory, find similar
        //    ones via the vector index and union those that exceed threshold.
        let n = id_embeddings.len();
        let mut parent: Vec<usize> = (0..n).collect();

        // Helper: find root with path compression (iterative).
        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]]; // path halving
                i = parent[i];
            }
            i
        }

        // Helper: union two elements.
        fn union(parent: &mut [usize], a: usize, b: usize) {
            let ra = find(parent, a);
            let rb = find(parent, b);
            if ra != rb {
                parent[rb] = ra;
            }
        }

        // Build an index from MemoryId -> position for quick lookup.
        let id_to_idx: std::collections::HashMap<MemoryId, usize> = id_embeddings
            .iter()
            .enumerate()
            .map(|(i, (mid, _))| (*mid, i))
            .collect();

        // Track the max similarity seen per pair for cluster reporting.
        let mut max_sim_per_cluster: std::collections::HashMap<usize, f32> =
            std::collections::HashMap::new();

        // For each memory, search for its K nearest neighbors and union
        // those above threshold. Using K=10 is enough to find clusters
        // without doing full O(n^2) pairwise comparison.
        const NEIGHBORS_PER_QUERY: usize = 10;

        for (i, (_mid, embedding)) in id_embeddings.iter().enumerate() {
            let scored = self
                .query_engine
                .vector_search(ns_config.id, embedding, NEIGHBORS_PER_QUERY + 1)
                .map_err(|e| bridge::BridgeError::Search(e.to_string()))?;

            for result in scored {
                // Skip self.
                if result.memory_id == id_embeddings[i].0 {
                    continue;
                }
                if result.score >= threshold {
                    if let Some(&j) = id_to_idx.get(&result.memory_id) {
                        union(&mut parent, i, j);
                        // Track max similarity for the cluster.
                        let root = find(&mut parent, i);
                        let entry = max_sim_per_cluster.entry(root).or_insert(0.0);
                        if result.score > *entry {
                            *entry = result.score;
                        }
                    }
                }
            }
        }

        // 5. Group into clusters (only clusters with 2+ members).
        let mut clusters_map: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            clusters_map.entry(root).or_default().push(i);
        }

        // 6. Load summaries for clustered memories and build response.
        let mut clusters: Vec<bridge::DuplicateCluster> = Vec::new();

        for (root, members) in &clusters_map {
            if members.len() < 2 {
                continue;
            }

            let member_ids: Vec<MemoryId> =
                members.iter().map(|&idx| id_embeddings[idx].0).collect();
            let storage = self.storage.clone();
            let summaries: Vec<(MemoryId, String)> = tokio::task::spawn_blocking(move || {
                let storage_r = match storage.read() {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                member_ids
                    .iter()
                    .map(|&mid| {
                        let summary = storage_r
                            .get_record(mid)
                            .ok()
                            .flatten()
                            .map(|r| r.summary.clone())
                            .unwrap_or_default();
                        (mid, summary)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default();

            let entries: Vec<bridge::DuplicateEntry> = summaries
                .into_iter()
                .map(|(mid, summary)| bridge::DuplicateEntry {
                    id: mid.to_string(),
                    summary,
                })
                .collect();

            let max_similarity = max_sim_per_cluster.get(root).copied().unwrap_or(0.0);

            clusters.push(bridge::DuplicateCluster {
                memories: entries,
                max_similarity,
            });
        }

        // Sort clusters by max_similarity descending (most likely duplicates first).
        clusters.sort_by(|a, b| {
            b.max_similarity
                .partial_cmp(&a.max_similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(clusters)
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
            let storage = self.storage.clone();
            let ns_name = input.namespace.clone();
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                storage_r
                    .get_namespace_by_name(&ns_name)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                    .ok_or_else(|| {
                        bridge::BridgeError::NotFound(format!("namespace '{}' not found", ns_name,))
                    })
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
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

        // Generate full embedding (summary + full_text + tags for max surface).
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
            phase: DecayPhase::Full,
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

        // Insert into storage (on blocking thread to avoid starving the async runtime).
        let record = {
            let storage = self.storage.clone();
            let embedding_clone = embedding.clone();
            let full_text_clone = input.full_text.clone();
            let ns_id = ns_config.id;
            tokio::task::spawn_blocking(move || {
                let mut storage_w = storage.write().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                storage_w
                    .insert_memory(
                        memory_id,
                        ns_id,
                        &mut record,
                        &embedding_clone,
                        full_text_clone.as_deref(),
                    )
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
                Ok::<_, bridge::BridgeError>(record)
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
        };

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
        // Track whether we need to persist a supersedes edge after releasing graph lock.
        let mut supersedes_edge: Option<crate::storage::PersistedEdge> = None;
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
                } else {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    supersedes_edge = Some(crate::storage::PersistedEdge {
                        source: memory_id,
                        target: old_id,
                        edge_type: crate::model::EdgeType::Supersedes,
                        weight: 1.0,
                        auto_created: false,
                        created_at: now_ms,
                    });
                }
            }
        } // graph write lock released

        // Persist supersedes edge AFTER graph lock is released to avoid lock ordering inversion.
        if let Some(persisted) = supersedes_edge {
            let old_id = persisted.target;
            let storage = self.storage.clone();
            let persist_result = tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                storage_r
                    .batch_add_edges(&[persisted])
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))
            })
            .await;
            match persist_result {
                Ok(Err(e)) => {
                    tracing::warn!(
                        memory_id = %memory_id,
                        superseded = %old_id,
                        error = %e,
                        "supersedes edge persistence failed (non-fatal)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        memory_id = %memory_id,
                        superseded = %old_id,
                        error = %e,
                        "supersedes edge persistence task panicked (non-fatal)"
                    );
                }
                _ => {}
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
            created_at: format_timestamp(record.created_at, self.timezone),
        })
    }

    async fn get_memory(
        &self,
        id: MemoryId,
    ) -> Result<Option<bridge::MemoryRecord>, bridge::BridgeError> {
        let storage = self.storage.clone();
        let tz = self.timezone;
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;

            let disk_record = storage_r
                .get_record(id)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

            match disk_record {
                Some(record) => {
                    if record.phase == DecayPhase::Tombstone {
                        let ns_name = storage_r
                            .get_namespace(NamespaceId::new(record.namespace_id))
                            .ok()
                            .flatten()
                            .map(|ns| ns.name.clone())
                            .unwrap_or_default();

                        return Ok(Some(bridge::MemoryRecord {
                            id: id.to_string(),
                            namespace: ns_name,
                            summary: String::new(),
                            full_text: None,
                            tags: Vec::new(),
                            phase: "Tombstone".to_string(),
                            strength: 0.0,
                            stability: record.stability,
                            created_at: format_timestamp(record.created_at, tz),
                            last_accessed_at: format_timestamp(record.last_accessed_at, tz),
                            is_permastore: false,
                            edge_count: record.edge_count,
                        }));
                    }

                    let ns_name = storage_r
                        .get_namespace(NamespaceId::new(record.namespace_id))
                        .ok()
                        .flatten()
                        .map(|ns| ns.name.clone())
                        .unwrap_or_default();

                    let full_text = if record.text_length > 0 {
                        let text_ref = crate::storage::TextRef {
                            file_offset: record.text_offset,
                            length: record.text_length,
                        };
                        storage_r.get_text(text_ref).ok().flatten()
                    } else {
                        None
                    };

                    Ok(Some(bridge::MemoryRecord {
                        id: id.to_string(),
                        namespace: ns_name,
                        summary: record.summary.clone(),
                        full_text,
                        tags: record.tags.iter().map(|t| t.to_string()).collect(),
                        phase: format!("{:?}", record.phase),
                        strength: record.strength,
                        stability: record.stability,
                        created_at: format_timestamp(record.created_at, tz),
                        last_accessed_at: format_timestamp(record.last_accessed_at, tz),
                        is_permastore: record.is_permastore != 0,
                        edge_count: record.edge_count,
                    }))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))?
    }

    async fn delete_memory(&self, id: MemoryId) -> Result<bool, bridge::BridgeError> {
        // Tombstone-based deletion: strip content from the record but
        // preserve the graph node and edges so relationship chains
        // remain intact for spreading activation traversal.

        // 1. Read, check, tombstone, and free the vector slot atomically
        //    under a single write lock to prevent TOCTOU races. Without
        //    this, two concurrent deletes could both read the record as
        //    non-tombstoned, both tombstone it, and both free the same
        //    vector slot -- corrupting the free list.
        let existing_record = {
            let storage = self.storage.clone();
            tokio::task::spawn_blocking(move || {
                let mut storage_w = storage.write().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;

                let existing = storage_w
                    .get_record(id)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

                let Some(existing_record) = existing else {
                    return Ok::<Option<crate::model::record::DiskRecord>, bridge::BridgeError>(None);
                };

                if existing_record.phase == DecayPhase::Tombstone {
                    return Ok(None);
                }

                // Tombstone the record.
                storage_w
                    .tombstone_memory(id)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

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

                Ok(Some(existing_record))
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
        };

        let Some(existing_record) = existing_record else {
            return Ok(false);
        };

        // 2. Invalidate cache entry.
        self.cache.invalidate(id).await;

        // 3. Remove from FTS5 index.
        {
            let fts = self.fts_index.lock().await;
            if let Err(e) = fts.remove(id) {
                tracing::warn!(
                    memory_id = %id,
                    %e,
                    "FTS5 removal failed (non-fatal)"
                );
            }
        }

        // 4. Remove from vector index.
        {
            use crate::search::VectorIndex;
            let mut vi = self.vector_index.write().await;
            if let Err(e) = vi.remove(id) {
                tracing::warn!(
                    memory_id = %id,
                    %e,
                    "vector index removal failed (non-fatal)"
                );
            }
        }

        // 5. Remove from entity index.
        {
            let metadata = crate::model::parse_structured_tags(&existing_record.tags);
            if !metadata.entities.is_empty() {
                let mut ei = self.entity_index.write().await;
                ei.remove(id, &metadata.entities);
            }
        }

        // 6. Update graph node phase to Tombstone (keep node and edges).
        {
            let mut graph_w = self.graph.write().await;
            let _ = graph_w.update_node_state(id, DecayPhase::Tombstone, 0.0);
        }

        Ok(true)
    }

    async fn reinforce_memory(
        &self,
        id: MemoryId,
        quality: u8,
    ) -> Result<bridge::ReinforceResult, bridge::BridgeError> {
        // Step 1: Read the current record.
        let record = {
            let storage = self.storage.clone();
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                storage_r
                    .get_record(id)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                    .ok_or_else(|| bridge::BridgeError::NotFound(format!("memory {id} not found")))
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
        };

        // Tombstoned memories cannot be reinforced.
        if record.phase == DecayPhase::Tombstone {
            return Err(bridge::BridgeError::NotFound(format!(
                "memory {id} has been deleted (tombstoned)"
            )));
        }

        let phase = record.phase;

        // Step 2: Look up the namespace's decay rate multiplier.
        let ns_decay_multiplier = {
            let storage = self.storage.clone();
            let ns_id = record.namespace_id;
            let global_multiplier = self.config.decay.decay_rate_multiplier;
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                let multiplier = storage_r
                    .get_namespace(NamespaceId::new(ns_id))
                    .ok()
                    .flatten()
                    .and_then(|ns| ns.decay_rate_multiplier)
                    .unwrap_or(global_multiplier as f32);
                Ok::<f32, bridge::BridgeError>(multiplier)
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
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
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                storage_r
                    .update_decay_state(id, phase, new_strength, new_strength, new_stability, is_permastore)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
                storage_r
                    .update_access(id, now, crate::model::AccessKind::ManualReinforcement)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??;
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

        Ok(bridge::ReinforceResult {
            id: id.to_string(),
            strength: new_strength,
            stability: new_stability,
            phase: format!("{:?}", phase),
            is_permastore,
        })
    }

    async fn list_memories(
        &self,
        input: bridge::ListMemoriesInput,
    ) -> Result<bridge::ListMemoriesResponse, bridge::BridgeError> {
        // Build tag filters: merge explicit tags + entity/topic tags for AND semantics.
        let mut require_tags: Vec<crate::model::Tag> = input
            .tags
            .iter()
            .filter_map(|t| crate::model::Tag::new(t).ok())
            .collect();
        for entity in &input.entities {
            if let Ok(tag) = crate::model::Tag::new(&format!("entity/{}", entity.to_lowercase())) {
                require_tags.push(tag);
            }
        }

        // Resolve namespace and query in a single blocking task.
        let storage = self.storage.clone();
        let ns_name = input.namespace.clone();
        let offset = input.offset;
        let limit = input.limit;
        let time_range_start = input.time_range_start;
        let time_range_end = input.time_range_end;
        let (_ns_config, page, total) = tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            let ns = storage_r
                .get_namespace_by_name(&ns_name)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    bridge::BridgeError::NotFound(format!("namespace '{}' not found", ns_name,))
                })?;
            let (page, total) = storage_r
                .meta_store()
                .list_memories_filtered(
                    ns.id,
                    &require_tags,
                    time_range_start,
                    time_range_end,
                    offset,
                    limit,
                )
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
            Ok::<_, bridge::BridgeError>((ns, page, total))
        })
        .await
        .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))??;

        // Convert DiskRecords to ListMemoryEntry.
        let tz = self.timezone;
        let memories: Vec<bridge::ListMemoryEntry> = page
            .into_iter()
            .map(|(mid, record)| {
                let metadata = crate::model::parse_structured_tags(&record.tags);
                bridge::ListMemoryEntry {
                    id: mid.to_string(),
                    summary: record.summary,
                    entities: metadata.entities,
                    topics: metadata.topics,
                    created_at: format_timestamp(record.created_at, tz),
                }
            })
            .collect();

        Ok(bridge::ListMemoriesResponse {
            memories,
            total,
            offset: input.offset,
            limit: input.limit,
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
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        timezone: chrono_tz::Tz,
    ) -> Self {
        Self { storage, timezone }
    }
}

#[async_trait]
impl bridge::NamespaceRegistry for McpNamespaceAdapter {
    async fn list_namespaces(&self) -> Result<Vec<bridge::NamespaceInfo>, bridge::BridgeError> {
        let storage = self.storage.clone();
        let tz = self.timezone;
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            let namespaces = storage_r
                .list_namespaces()
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
            Ok(namespaces
                .into_iter()
                .map(|ns| bridge::NamespaceInfo {
                    id: ns.id.get(),
                    name: ns.name.clone(),
                    embedding_dim: ns.embedding_dim as u16,
                    memory_count: 0,
                    created_at: format_timestamp(ns.created_at, tz),
                })
                .collect())
        })
        .await
        .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))?
    }

    async fn create_namespace(
        &self,
        input: bridge::CreateNamespaceInput,
    ) -> Result<bridge::NamespaceInfo, bridge::BridgeError> {
        use crate::model::NamespaceConfig;

        let now = chrono::Utc::now().timestamp_millis();
        let storage = self.storage.clone();
        let tz = self.timezone;
        let input_name = input.name.clone();
        let embedding_dim = input.embedding_dim;
        let initial_stability = input.initial_stability;
        let desired_retention = input.desired_retention;
        let decay_rate_multiplier = input.decay_rate_multiplier;

        tokio::task::spawn_blocking(move || {
            let dim = embedding_dim.map(|d| d as u32).unwrap_or_else(|| {
                storage
                    .read()
                    .ok()
                    .and_then(|s| s.get_namespace_by_name("default").ok().flatten())
                    .map(|ns| ns.embedding_dim)
                    .unwrap_or(1536)
            });

            let ns_config = NamespaceConfig {
                id: NamespaceId::UNSET,
                name: input_name.clone(),
                embedding_dim: dim,
                initial_stability: initial_stability.unwrap_or(3.7145),
                default_difficulty: 5.0,
                phase_thresholds: crate::model::namespace::PhaseThresholds::default(),
                permastore_threshold: 1500.0,
                created_at: now,
                desired_retention: desired_retention.unwrap_or(0.9),
                decay_rate_multiplier,
            };

            let mut storage_w = storage.write().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            let assigned_id = storage_w
                .create_namespace(&ns_config)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

            Ok(bridge::NamespaceInfo {
                id: assigned_id.get(),
                name: input_name,
                embedding_dim: dim as u16,
                memory_count: 0,
                created_at: format_timestamp(now, tz),
            })
        })
        .await
        .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))?
    }

    async fn namespace_stats(
        &self,
        name: &str,
    ) -> Result<bridge::NamespaceStats, bridge::BridgeError> {
        let storage = self.storage.clone();
        let ns_name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
            })?;
            let ns = storage_r
                .get_namespace_by_name(&ns_name)
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    bridge::BridgeError::NotFound(format!("namespace '{ns_name}' not found"))
                })?;

            // Compute real stats by scanning all records in the namespace.
            let all_records = storage_r
                .scan_all()
                .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

            let mut memory_count: u64 = 0;
            let mut full_count: u64 = 0;
            let mut summary_count: u64 = 0;
            let mut ghost_count: u64 = 0;
            let mut permastore_count: u64 = 0;
            let mut strength_sum: f64 = 0.0;
            let mut edge_count: u64 = 0;

            for (_id, record) in &all_records {
                if NamespaceId::new(record.namespace_id) != ns.id {
                    continue;
                }
                memory_count += 1;

                match record.phase {
                    DecayPhase::Full => full_count += 1,
                    DecayPhase::Summary => summary_count += 1,
                    DecayPhase::Ghost => ghost_count += 1,
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

            let vector_bytes = memory_count * ns.embedding_dim as u64 * 4;

            Ok(bridge::NamespaceStats {
                name: ns_name,
                memory_count,
                phase_counts: bridge::PhaseCounts {
                    full: full_count,
                    summary: summary_count,
                    ghost: ghost_count,
                },
                permastore_count,
                avg_strength,
                edge_count,
                vector_bytes,
            })
        })
        .await
        .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))?
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
        let storage = self.storage.clone();
        let storage_ok = tokio::task::spawn_blocking(move || storage.read().is_ok())
            .await
            .unwrap_or(false);
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
