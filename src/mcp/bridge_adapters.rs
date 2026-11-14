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

/// Map an explicit-link validation failure onto the bridge error the tool
/// layer already knows how to render.
///
/// The distinction that matters to a caller: `NotFound` and `InvalidInput`
/// mean "fix your request", while `Internal` and `Storage` mean "this one is
/// ours". `tools.rs` surfaces all four through its existing
/// `Failed to store memory: {e}` path, so no error handling changes there.
fn map_link_error(e: crate::graph::LinkError) -> bridge::BridgeError {
    use crate::graph::LinkError;
    match e {
        LinkError::TargetNotFound { .. } => bridge::BridgeError::NotFound(e.to_string()),
        LinkError::NamespaceMismatch { .. } | LinkError::SelfReference { .. } => {
            bridge::BridgeError::InvalidInput(e.to_string())
        }
        LinkError::TargetMissingFromGraph { .. } => bridge::BridgeError::Internal(e.to_string()),
        LinkError::Lookup { .. } => bridge::BridgeError::Storage(e.to_string()),
    }
}

async fn effects_stamp(
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
) -> Result<u64, bridge::BridgeError> {
    let storage = storage.clone();
    tokio::task::spawn_blocking(move || {
        storage
            .read()
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
            .meta_store()
            .effects_stamp()
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))
    })
    .await
    .map_err(|e| bridge::BridgeError::Internal(e.to_string()))?
}

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
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
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
                            bridge::BridgeError::NotFound(format!(
                                "namespace '{}' not found",
                                ns_name,
                            ))
                        })
                })
                .await
                .map_err(|e| {
                    bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
                })??;
            };

            // A label that cannot become a tag is an ERROR, not a dropped
            // filter: dropping one widens the query, and "every memory in
            // the namespace" is indistinguishable from a real result set.
            let require_tags = match filter_tags(
                &[
                    ("tags", "", &query.tags),
                    (
                        "entities",
                        crate::model::constants::ENTITY_TAG_PREFIX,
                        &query.entities,
                    ),
                    (
                        "topics",
                        crate::model::constants::TOPIC_TAG_PREFIX,
                        &query.topics,
                    ),
                    (
                        "emotions",
                        crate::model::constants::EMOTION_TAG_PREFIX,
                        &query.emotions,
                    ),
                ],
                "recall_memories",
            ) {
                Ok(tags) => tags,
                Err(e) => return Err(bridge::BridgeError::InvalidInput(e)),
            };

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
                            let entry =
                                neighbor_weights.entry(neighbor_node.memory_id).or_insert((
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
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
    }

    async fn find_similar(
        &self,
        id: MemoryId,
        limit: usize,
        min_score: Option<f32>,
        _same_namespace: bool,
    ) -> Result<Vec<bridge::SearchHit>, bridge::BridgeError> {
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
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
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
    }

    async fn scan_duplicates(
        &self,
        namespace: &str,
        threshold: f32,
        max_memories: usize,
    ) -> Result<Vec<bridge::DuplicateCluster>, bridge::BridgeError> {
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
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
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??;

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
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
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
        cache.bind_storage(&storage);
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

    /// Borrow this adapter's out-of-storage indexes for
    /// [`crate::index_cleanup::purge_from_indexes`].
    fn index_handles(&self) -> crate::index_cleanup::IndexHandles<'_> {
        crate::index_cleanup::IndexHandles {
            cache: &self.cache,
            vector_index: &self.vector_index,
            fts_index: &self.fts_index,
            entity_index: &self.entity_index,
            graph: &self.graph,
        }
    }

    /// Turn raw `(id, score)` candidates into a caller-facing report,
    /// loading each summary from the cache and falling back to storage.
    ///
    /// Only runs when candidates were found, so the record loads are
    /// conditional on the warning actually firing rather than a cost every
    /// store pays. The common case is a cache hit — the memories most
    /// likely to duplicate a new one are the ones just written.
    ///
    /// A summary that cannot be loaded yields an empty string rather than
    /// dropping the match: "there is something very like this, id X" is
    /// still the useful half of the advisory.
    async fn hydrate_near_duplicates(
        &self,
        candidates: Vec<(MemoryId, f32)>,
        threshold: f64,
    ) -> Option<crate::model::NearDuplicateReport> {
        use crate::model::constants::NEAR_DUPLICATE_SUMMARY_MAX_BYTES;
        use crate::model::{NearDuplicateMatch, NearDuplicateReport, truncate_summary};

        if candidates.is_empty() {
            return None;
        }

        // Summary AND phase. The phase is the load-bearing half: the
        // `decay_phases` filter `find_near_duplicates` passes to the
        // vector index is INERT, because `FlatVectorIndex` writes each
        // entry's `decay_phase` once at add time (always Full) and
        // `VectorIndex::update_metadata` has no callers. Without a check
        // here, a Ghost — which has no summary left — and a
        // hard-deleted memory both get reported as near-duplicates with
        // an empty summary, and the tool description then tells the
        // agent to reinforce them.
        let mut hydrated: std::collections::HashMap<MemoryId, (String, DecayPhase)> =
            std::collections::HashMap::new();
        let mut misses: Vec<MemoryId> = Vec::new();
        for (id, _) in &candidates {
            match self.cache.get(*id).await {
                Some(cached) => {
                    hydrated.insert(*id, (cached.summary.clone(), cached.phase));
                }
                None => misses.push(*id),
            }
        }

        // One blocking hop for every miss, not one per miss. A record
        // that is not there at all stays out of `hydrated` and is
        // dropped below: it was hard-deleted and the vector index has
        // not caught up.
        if !misses.is_empty() {
            let storage = self.storage.clone();
            let loaded: Vec<(MemoryId, (String, DecayPhase))> =
                tokio::task::spawn_blocking(move || {
                    let storage_r = match storage.read() {
                        Ok(s) => s,
                        Err(_) => return Vec::new(),
                    };
                    misses
                        .iter()
                        .filter_map(|&mid| {
                            let record = storage_r.get_record(mid).ok().flatten()?;
                            Some((mid, (record.summary, record.phase)))
                        })
                        .collect()
                })
                .await
                .unwrap_or_default();
            hydrated.extend(loaded);
        }

        let matches: Vec<NearDuplicateMatch> = candidates
            .into_iter()
            .filter_map(|(id, score)| {
                // Full and Summary only, matching what the (inert)
                // index-level filter claims and what the docs promise.
                // Ghost has no summary left to show and Tombstone is
                // deleted; neither is something to be told it duplicates.
                let (summary, phase) = hydrated.get(&id)?;
                if !matches!(phase, DecayPhase::Full | DecayPhase::Summary) {
                    return None;
                }
                Some(NearDuplicateMatch {
                    id,
                    score,
                    summary: truncate_summary(summary, NEAR_DUPLICATE_SUMMARY_MAX_BYTES),
                })
            })
            .collect();

        NearDuplicateReport::from_matches(threshold, matches)
    }
}

#[async_trait]
impl bridge::StorageEngine for McpStorageAdapter {
    async fn store_memory(
        &self,
        input: bridge::StoreInput,
    ) -> Result<bridge::StoredMemory, bridge::BridgeError> {
        let _publication = crate::derived_completion::publication_gate(&self.storage)
            .await
            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
            .read_owned()
            .await;
        effects_stamp(&self.storage).await?;

        use crate::model::Tag;
        use crate::model::constants::{DUPLICATE_SIMILARITY_THRESHOLD, MAX_NEAR_DUPLICATE_MATCHES};
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

        // The ID is minted here rather than just before the insert so the
        // link pre-validation below can reject a self-reference. Minting is
        // pure (UUIDv7 from the clock), so an early mint costs nothing.
        let memory_id = MemoryId::new();

        // Validate caller-specified links BEFORE anything is written.
        //
        // The record is committed to meta.db, fulltext.dat, vectors.dat, FTS
        // and the cache further down; a link failure discovered after that
        // would have to unwind all of it. Checking here makes "reject the
        // whole store" free, and it also avoids paying for an embedding
        // round-trip on a store that cannot succeed.
        if let Some(parent_id) = input.parent_id {
            crate::graph::check_link_target(
                "parentId",
                Some(memory_id),
                parent_id,
                ns_config.id,
                &self.graph,
                &self.storage,
                &self.cache,
            )
            .await
            .map_err(map_link_error)?;
        }
        if let Some(old_id) = input.supersedes {
            crate::graph::check_link_target(
                "supersedes",
                Some(memory_id),
                old_id,
                ns_config.id,
                &self.graph,
                &self.storage,
                &self.cache,
            )
            .await
            .map_err(map_link_error)?;
        }

        // Convert structured metadata fields to tags and merge with explicit
        // tags. The prefixes come from `constants` because the label length
        // limits reserve room for exactly these bytes.
        use crate::model::constants::{EMOTION_TAG_PREFIX, ENTITY_TAG_PREFIX, TOPIC_TAG_PREFIX};
        let mut merged_tags = input.tags.clone();
        for entity in &input.entities {
            let tag = format!("{ENTITY_TAG_PREFIX}{}", entity.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }
        for topic in &input.topics {
            let tag = format!("{TOPIC_TAG_PREFIX}{}", topic.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }
        for emotion in &input.emotions {
            let tag = format!("{EMOTION_TAG_PREFIX}{}", emotion.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }

        // Generate full embedding (summary + full_text + tags for max surface).
        let embedding = match input.embedding {
            Some(emb) => emb,
            None => {
                let mut embed_text = match &input.full_text {
                    Some(ft) => format!(
                        "{}
\n{}",
                        input.summary, ft
                    ),
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

        // Near-duplicate advisory.
        //
        // Runs BEFORE the write, for two reasons. The embedding is already
        // in hand, so there is no second embed round-trip. And the new
        // memory is not in the vector index yet — `index.add` is ~70 lines
        // below — so it cannot match itself and no self-filter is needed.
        // Moving this below the write silently turns every store into its
        // own duplicate at score 1.0.
        //
        // An unavailable advisory must not be reported as "no duplicates".
        let advisory_stamp = effects_stamp(&self.storage).await?;
        let near_duplicates = if input.check_duplicates.unwrap_or(true) {
            let candidates = {
                let index = self.vector_index.read().await;
                crate::search::find_near_duplicates(
                    &index,
                    &embedding,
                    ns_config.id,
                    DUPLICATE_SIMILARITY_THRESHOLD as f32,
                    MAX_NEAR_DUPLICATE_MATCHES,
                )
            }; // index read lock released before the awaits below
            self.hydrate_near_duplicates(candidates, DUPLICATE_SIMILARITY_THRESHOLD)
                .await
        } else {
            None
        };

        if effects_stamp(&self.storage).await? != advisory_stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during advisory".into(),
            ));
        }
        let now = chrono::Utc::now().timestamp_millis();
        // Over-long tags cannot reach this point: the shared request
        // validator bounds every tag and reserves the `entity/` /
        // `topic/` / `emotion/` prefix out of the label limits. A drop
        // here therefore means a tag outside `Tag`'s alphabet, and it
        // is logged rather than swallowed.
        let parsed_tags: Vec<Tag> =
            crate::model::parse_tags_lossy("store_memory.tags", &merged_tags);

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

        // Add node to the relationship graph (must happen BEFORE autolink,
        // and before either explicit edge — both endpoints must exist).
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
        } // graph write lock released

        // Explicit links. Both targets were validated before the record was
        // written, so anything that goes wrong here is a durability or
        // idempotency detail, not a reason to fail a store that succeeded.
        if let Some(parent_id) = input.parent_id {
            crate::graph::apply_parent_edge(
                parent_id,
                memory_id,
                &self.graph,
                &self.storage,
                &self.cache,
            )
            .await;
        }

        let supersedes_outcome = match input.supersedes {
            Some(old_id) => Some(
                crate::graph::apply_supersedes_edge(
                    memory_id,
                    old_id,
                    &self.graph,
                    &self.storage,
                    &self.cache,
                )
                .await,
            ),
            None => None,
        };

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
            supersedes: supersedes_outcome,
            near_duplicates,
        })
    }

    async fn get_memory(
        &self,
        id: MemoryId,
    ) -> Result<Option<bridge::MemoryRecord>, bridge::BridgeError> {
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
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
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
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
                    return Ok::<Option<crate::model::record::DiskRecord>, bridge::BridgeError>(
                        None,
                    );
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

        // 2. Cache and the four indexes storage cannot reach. Shared
        //    with the HTTP delete path and with namespace deletion, so
        //    the three cannot drift apart again. `existing_record` is
        //    the PRE-strip record, which is what keeps the entity names
        //    reachable.
        crate::index_cleanup::purge_from_indexes(
            &self.index_handles(),
            std::slice::from_ref(&(id, existing_record)),
            crate::index_cleanup::FtsAction::PerRecord,
            crate::index_cleanup::GraphAction::MarkTombstoned,
        )
        .await;

        Ok(true)
    }

    async fn delete_memories(&self, ids: &[MemoryId]) -> Result<Vec<bool>, bridge::BridgeError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // ONE spawn_blocking holding ONE storage.write() for the whole
        // batch. 100 memories go from 100 redb commits (100 fsyncs) to
        // one, and from ~700 lock acquisitions to a handful.
        //
        // `batch_tombstone` is also the single source of truth for
        // liveness here: only a `Tombstoned` outcome frees a vector
        // slot, and a duplicate id reports `AlreadyTombstoned`, so the
        // same slot cannot be freed twice — which would cycle the
        // on-disk free list and underflow `live_count`.
        let (flags, purged) = {
            let storage = self.storage.clone();
            let ids = ids.to_vec();
            tokio::task::spawn_blocking(move || {
                let mut storage_w = storage.write().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;

                let outcomes = storage_w
                    .meta_store()
                    .batch_tombstone(&ids)
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;

                let mut flags = Vec::with_capacity(outcomes.len());
                let mut purged: Vec<(MemoryId, crate::model::DiskRecord)> = Vec::new();

                for (id, outcome) in ids.iter().zip(outcomes) {
                    match outcome {
                        crate::storage::metadata::TombstoneOutcome::Tombstoned(record) => {
                            let ns_id = NamespaceId::new(record.namespace_id);
                            if let Err(e) = storage_w.free_vector_slot(ns_id, record.vector_slot) {
                                tracing::warn!(
                                    memory_id = %id,
                                    vector_slot = record.vector_slot,
                                    %e,
                                    "vector slot free failed (non-fatal)"
                                );
                            }
                            flags.push(true);
                            purged.push((*id, record));
                        }
                        crate::storage::metadata::TombstoneOutcome::NotFound
                        | crate::storage::metadata::TombstoneOutcome::AlreadyTombstoned => {
                            flags.push(false)
                        }
                    }
                }

                Ok::<_, bridge::BridgeError>((flags, purged))
            })
            .await
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??
        };

        // One pass over the indexes for the whole batch, with the
        // PRE-strip records so the entity names are still there.
        crate::index_cleanup::purge_from_indexes(
            &self.index_handles(),
            &purged,
            crate::index_cleanup::FtsAction::PerRecord,
            crate::index_cleanup::GraphAction::MarkTombstoned,
        )
        .await;

        Ok(flags)
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
        let new_stability =
            engine.review_stability(record.stability, elapsed_days, quality, ns_decay_multiplier);

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
                    .update_decay_state(
                        id,
                        phase,
                        new_strength,
                        new_strength,
                        new_stability,
                        is_permastore,
                    )
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
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
            // Build tag filters: merge explicit tags + entity tags for AND
            // semantics. An unfilterable label is an error — see
            // `filter_tags`.
            let require_tags = match filter_tags(
                &[
                    ("tags", "", &input.tags),
                    (
                        "entities",
                        crate::model::constants::ENTITY_TAG_PREFIX,
                        &input.entities,
                    ),
                ],
                "list_memories",
            ) {
                Ok(tags) => tags,
                Err(e) => return Err(bridge::BridgeError::InvalidInput(e)),
            };

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
            .map_err(|e| {
                bridge::BridgeError::Internal(format!("blocking task join error: {e}"))
            })??;

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
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
    }

    async fn list_tags(
        &self,
        input: bridge::ListTagsInput,
    ) -> Result<bridge::ListTagsResponse, bridge::BridgeError> {
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
            let storage = self.storage.clone();
            let namespace = input.namespace.clone();
            let limit = input.limit;

            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;

                let (counts, memories_counted) = match namespace.as_deref() {
                    Some(name) => {
                        let ns = storage_r
                            .get_namespace_by_name(name)
                            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?
                            .ok_or_else(|| {
                                bridge::BridgeError::NotFound(format!(
                                    "namespace '{name}' not found"
                                ))
                            })?;
                        let (counts, memories) = storage_r
                            .meta_store()
                            .tag_counts_in_namespace(ns.id)
                            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
                        (counts.into_iter().collect::<Vec<_>>(), Some(memories))
                    }
                    // Global scope reads TAG_INDEX directly, which is keyed by
                    // tag alone: no memory records are touched, so there is no
                    // honest number to report for `memoriesCounted`.
                    None => (
                        storage_r
                            .list_tags()
                            .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?,
                        None,
                    ),
                };

                Ok(build_list_tags_response(
                    namespace,
                    memories_counted,
                    counts,
                    limit,
                ))
            })
            .await
            .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))?
        }
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
    }
}

/// Split a flat histogram into plain/entity/topic/emotion buckets, order
/// each most-common-first, and truncate to `limit`.
///
/// A label whose prefix is one of the derived ones moves to that bucket
/// with the prefix stripped; everything else — including a tag that
/// merely *contains* a slash, such as `project/recalld` — stays in
/// `tags`. A bare prefix with an empty label also stays in `tags`, rather
/// than becoming an entity with no name.
///
/// Pure, so the bucketing rules are testable without storage.
fn build_list_tags_response(
    namespace: Option<String>,
    memories_counted: Option<u64>,
    counts: Vec<(String, u64)>,
    limit: usize,
) -> bridge::ListTagsResponse {
    use crate::model::constants::{EMOTION_TAG_PREFIX, ENTITY_TAG_PREFIX, TOPIC_TAG_PREFIX};

    let mut plain = Vec::new();
    let mut entities = Vec::new();
    let mut topics = Vec::new();
    let mut emotions = Vec::new();

    for (name, count) in counts {
        let bucketed = [
            (ENTITY_TAG_PREFIX, &mut entities),
            (TOPIC_TAG_PREFIX, &mut topics),
            (EMOTION_TAG_PREFIX, &mut emotions),
        ]
        .into_iter()
        .find_map(|(prefix, bucket)| {
            let label = name.strip_prefix(prefix)?;
            // A bare `entity/` names nothing; leave it where it is
            // rather than inventing an empty-named entity.
            (!label.is_empty()).then(|| bucket.push((label.to_string(), count)))
        });

        if bucketed.is_none() {
            plain.push((name, count));
        }
    }

    let finish = |mut bucket: Vec<(String, u64)>| {
        crate::model::sort_tag_counts(&mut bucket);
        let total = bucket.len() as u64;
        let truncated = bucket.len() > limit;
        bucket.truncate(limit);
        bridge::TagBucket {
            total,
            truncated,
            items: bucket
                .into_iter()
                .map(|(name, count)| bridge::TagCount { name, count })
                .collect(),
        }
    };

    bridge::ListTagsResponse {
        namespace,
        memories_counted,
        tags: finish(plain),
        entities: finish(entities),
        topics: finish(topics),
        emotions: finish(emotions),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// McpNamespaceAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` to the MCP `NamespaceRegistry` trait.
///
/// Holds the four out-of-storage indexes as well as storage, because
/// namespace deletion has to clean them: storage can destroy the
/// records, but a namespace whose FTS rows and vector-index entries
/// survive is a namespace that goes on burning top-k slots in every
/// search. Namespace lifecycle already lives on this trait —
/// create, list and stats are all here — so widening it is preferable
/// to splitting deletion onto `StorageEngine` because that is where the
/// handles happen to be.
pub struct McpNamespaceAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
    graph: SharedGraph,
    timezone: chrono_tz::Tz,
}

impl McpNamespaceAdapter {
    /// Create a new namespace adapter wrapping the storage engine and
    /// the indexes a namespace deletion has to clean.
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
        entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
        graph: SharedGraph,
        timezone: chrono_tz::Tz,
    ) -> Self {
        cache.bind_storage(&storage);
        Self {
            storage,
            cache,
            vector_index,
            fts_index,
            entity_index,
            graph,
            timezone,
        }
    }

    async fn complete_effects(&self) -> Result<(), bridge::BridgeError> {
        crate::derived_completion::DerivedCompletion {
            storage: self.storage.clone(),
            cache: self.cache.clone(),
            graph: self.graph.clone(),
            vectors: self.vector_index.clone(),
            fts: self.fts_index.clone(),
            entities: self.entity_index.clone(),
        }
        .complete()
        .await
        .map_err(|e| bridge::BridgeError::Storage(e.to_string()))
    }

    /// Borrow this adapter's out-of-storage indexes for
    /// [`crate::index_cleanup::purge_from_indexes`].
    fn index_handles(&self) -> crate::index_cleanup::IndexHandles<'_> {
        crate::index_cleanup::IndexHandles {
            cache: &self.cache,
            vector_index: &self.vector_index,
            fts_index: &self.fts_index,
            entity_index: &self.entity_index,
            graph: &self.graph,
        }
    }
}

/// Build a query's `require_tags` from several labelled groups, failing
/// with a caller-facing message on the first label that is not a tag.
///
/// `groups` is `(request field, tag prefix, labels)`. Order is fixed so
/// a request broken in several ways always reports the same failure.
///
/// Deliberately NOT `parse_tags_lossy`: that drops what it cannot parse,
/// which is the right trade-off for a store (keep the memory, lose a
/// label) and exactly the wrong one for a filter. A dropped filter
/// WIDENS the query — `list_memories {"entities": ["José"]}` returned
/// every memory in the namespace — and the caller has no way to tell.
fn filter_tags(
    groups: &[(&str, &str, &Vec<String>)],
    tool: &str,
) -> Result<Vec<crate::model::Tag>, String> {
    let mut out = Vec::new();
    for (field, prefix, labels) in groups {
        match crate::model::parse_filter_tags(field, prefix, labels) {
            Ok(tags) => out.extend(tags),
            Err(e) => {
                tracing::warn!(
                    tool = %tool,
                    field = %e.field,
                    value = %e.value,
                    error = %e.source,
                    "Rejected an unfilterable label rather than widening the query"
                );
                return Err(e.to_string());
            }
        }
    }
    Ok(out)
}

/// A namespace deletion that failed, and what it had already destroyed.
///
/// Exists because `spawn_blocking` can only hand back one value: the
/// error the caller sees and the records the caller still owes index
/// cleanup for have to travel together, or the second is lost.
struct DeleteFailure {
    /// Records destroyed before the failure. Empty when nothing was.
    destroyed: Vec<(MemoryId, crate::model::DiskRecord)>,
    /// What the caller is told.
    error: bridge::BridgeError,
}

impl DeleteFailure {
    /// A failure that destroyed nothing.
    fn nothing_destroyed(error: bridge::BridgeError) -> Self {
        Self {
            destroyed: Vec::new(),
            error,
        }
    }
}

#[async_trait]
impl bridge::NamespaceRegistry for McpNamespaceAdapter {
    async fn list_namespaces(&self) -> Result<Vec<bridge::NamespaceInfo>, bridge::BridgeError> {
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
            let storage = self.storage.clone();
            let tz = self.timezone;
            tokio::task::spawn_blocking(move || {
                let storage_r = storage.read().map_err(|e| {
                    bridge::BridgeError::Internal(format!("storage lock poisoned: {e}"))
                })?;
                let namespaces = storage_r
                    .list_namespaces()
                    .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
                // `memory_count` was hardcoded to 0 here, so both this tool
                // and the `recalld://namespaces` resource reported every
                // namespace as empty. Counting live members costs one point
                // lookup per member and agrees with `list_memories.total` by
                // construction.
                let mut out = Vec::with_capacity(namespaces.len());
                for ns in namespaces {
                    let memory_count = storage_r
                        .meta_store()
                        .count_memories_in_namespace(ns.id)
                        .map_err(|e| bridge::BridgeError::Storage(e.to_string()))?;
                    out.push(bridge::NamespaceInfo {
                        id: ns.id.get(),
                        name: ns.name.clone(),
                        embedding_dim: ns.embedding_dim as u16,
                        memory_count,
                        created_at: format_timestamp(ns.created_at, tz),
                    });
                }
                Ok(out)
            })
            .await
            .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))?
        }
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
    }

    async fn create_namespace(
        &self,
        input: bridge::CreateNamespaceInput,
    ) -> Result<bridge::NamespaceInfo, bridge::BridgeError> {
        use crate::model::NamespaceConfig;

        // Validated here as well as at the tool layer, and this is the
        // load-bearing copy for the same reason the `default` guard in
        // `delete_namespace` is: the daemon socket dispatches
        // `create_namespace` straight into this adapter and never runs
        // the tool handler. A namespace name becomes a directory name,
        // so an unvalidated one becomes a path.
        crate::model::validation::validate_namespace_name(&input.name)
            .map_err(|e| bridge::BridgeError::InvalidInput(e.to_string()))?;

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
                    .and_then(|s| {
                        s.get_namespace_by_name(crate::model::constants::DEFAULT_NAMESPACE_NAME)
                            .ok()
                            .flatten()
                    })
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
                // Not a placeholder: a namespace created one line ago is
                // empty.
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
        let stamp = effects_stamp(&self.storage).await?;
        let result = async {
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
                let mut tombstone_count: u64 = 0;
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

                    // Tombstoned records were counted into `memory_count`,
                    // which made it disagree with `list_memories.total`, and
                    // then dragged every derived figure with them: `tombstone`
                    // zeroes strength and decay_strength but LEAVES
                    // is_permastore and edge_count intact, so a deleted memory
                    // still counted as permastore, still contributed its edges,
                    // and pulled avg_strength toward zero.
                    if record.phase == DecayPhase::Tombstone {
                        tombstone_count += 1;
                        continue;
                    }
                    memory_count += 1;

                    match record.phase {
                        DecayPhase::Full => full_count += 1,
                        DecayPhase::Summary => summary_count += 1,
                        DecayPhase::Ghost => ghost_count += 1,
                        // Unreachable: skipped above. Left as an arm rather
                        // than an `unreachable!()` because this is a read path
                        // and a corrupt record must not panic a stats call.
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
                    tombstone_count,
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
        .await;
        if effects_stamp(&self.storage).await? != stamp {
            return Err(bridge::BridgeError::Storage(
                "unavailable: effects changed during read".into(),
            ));
        }
        result
    }

    async fn delete_namespace(
        &self,
        input: bridge::DeleteNamespaceInput,
    ) -> Result<bridge::DeleteNamespaceResult, bridge::BridgeError> {
        use crate::model::constants::DEFAULT_NAMESPACE_NAME;

        // Validated before anything else, and for a blunter reason than
        // tidiness: the name is joined onto the data directory and
        // handed to `remove_dir_all`. `""` used to name a live,
        // healthy namespace whose directory WAS the data directory.
        crate::model::validation::validate_namespace_name(&input.name)
            .map_err(|e| bridge::BridgeError::InvalidInput(e.to_string()))?;

        // The default namespace is refused here as well as at the tool
        // layer, and this is the load-bearing copy: the daemon socket is
        // its own entry point and never runs the tool handler.
        //
        // Refused even with `force: true`, and even when empty. Startup
        // recreates a missing default under the SAME directory name with
        // a NEW namespace id, so a deleted default leaves a stale
        // vectors.dat that the recreated namespace opens and reuses —
        // handing unrelated memories somebody else's embeddings. The
        // boot sweep would remove that directory first, which makes the
        // failure unlikely rather than impossible, and "unlikely" is not
        // the right guarantee for silently wrong vectors.
        //
        // Compared case-insensitively, because the guard protects a
        // DIRECTORY and macOS APFS resolves `Default/` to `default/`.
        // With an exact compare, `create_namespace("Default")` +
        // `delete_namespace("Default", force: true)` walked straight
        // past this and unlinked the real default's vectors.dat; on the
        // next boot every record's vector_slot pointed past EOF.
        if input.name.eq_ignore_ascii_case(DEFAULT_NAMESPACE_NAME) {
            return Err(bridge::BridgeError::InvalidInput(format!(
                "The '{DEFAULT_NAMESPACE_NAME}' namespace cannot be deleted. It is \
                 recreated on every startup under the same directory name, so deleting \
                 it would leave a stale vectors.dat that a new namespace could silently \
                 reuse."
            )));
        }

        // Lookup, refuse-check and purge all happen inside ONE
        // storage.write() in ONE blocking closure. Splitting them across
        // two blocking hops would open a window in which a memory stored
        // between the check and the purge is destroyed by a check that
        // said "empty".
        let outcome = {
            let storage = self.storage.clone();
            let name = input.name.clone();
            let force = input.force;
            tokio::task::spawn_blocking(move || {
                let mut storage_w = storage.write().map_err(|e| {
                    DeleteFailure::nothing_destroyed(bridge::BridgeError::Internal(format!(
                        "storage lock poisoned: {e}"
                    )))
                })?;

                let config = storage_w
                    .get_namespace_by_name(&name)
                    .map_err(|e| {
                        DeleteFailure::nothing_destroyed(bridge::BridgeError::Storage(
                            e.to_string(),
                        ))
                    })?
                    .ok_or_else(|| {
                        DeleteFailure::nothing_destroyed(bridge::BridgeError::NotFound(format!(
                            "namespace '{name}' not found"
                        )))
                    })?;

                if !force {
                    // Live memories only. A namespace holding nothing
                    // but tombstones reports memoryCount 0 everywhere
                    // else, so refusing it here would be unexplainable —
                    // and those tombstones hold no content the user has
                    // not already asked to forget. The purge still reaps
                    // them, and the result reports how many.
                    //
                    // `count_live_records_in_namespace`, NOT
                    // `count_memories_in_namespace`: the latter walks
                    // NAMESPACE_INDEX while the destruction scans
                    // META_TABLE, so a record whose index entry is
                    // missing was invisible to the check and destroyed
                    // by the effect. The guard applies the purge's own
                    // predicate.
                    let live = storage_w
                        .meta_store()
                        .count_live_records_in_namespace(config.id)
                        .map_err(|e| {
                            DeleteFailure::nothing_destroyed(bridge::BridgeError::Storage(
                                e.to_string(),
                            ))
                        })?;
                    if live > 0 {
                        // Nothing has been touched at this point.
                        return Err(DeleteFailure::nothing_destroyed(
                            bridge::BridgeError::InvalidInput(format!(
                                "Namespace '{name}' still holds {live} memories. Deleting it \
                                 permanently destroys them and removes its vector file from \
                                 disk; there is no undo and no backup. Re-run with \
                                 force: true to proceed."
                            )),
                        ));
                    }
                }

                storage_w.purge_namespace(config.id).map_err(|e| {
                    let error = if e.is_partial() {
                        bridge::BridgeError::PartiallyApplied(format!(
                            "Namespace '{name}' was PARTIALLY destroyed: {} memories are \
                             permanently gone, and the namespace still exists holding the \
                             rest. Re-run delete_namespace to finish it. Cause: {}",
                            e.destroyed.len(),
                            e.source
                        ))
                    } else {
                        bridge::BridgeError::Storage(e.source.to_string())
                    };
                    DeleteFailure {
                        destroyed: e.destroyed,
                        error,
                    }
                })
            })
            .await
            .map_err(|e| bridge::BridgeError::Internal(format!("blocking task join error: {e}")))?
        };

        let purged = match outcome {
            Ok(purged) => purged,
            Err(failure) => {
                // The records in `failure.destroyed` are already gone
                // from meta.db and nothing else will ever reach their
                // index entries: a retry looks in META_TABLE and finds
                // them missing. Per-record FTS removal, not
                // whole-namespace — the namespace still holds the
                // records the purge did not get to, and their rows must
                // stay.
                if !failure.destroyed.is_empty() {
                    crate::index_cleanup::purge_from_indexes(
                        &self.index_handles(),
                        &failure.destroyed,
                        crate::index_cleanup::FtsAction::PerRecord,
                        crate::index_cleanup::GraphAction::RemoveNode,
                    )
                    .await;
                }
                // Even a partial purge may have durable effects to publish.
                self.complete_effects().await?;
                return Err(failure.error);
            }
        };

        // Surviving memories whose outgoing edge_count storage just
        // corrected. Their cached copy still holds the old number, and
        // nothing else will ever notice: invalidate so the next read
        // reloads the corrected record.
        if !purged.peer_edge_counts.is_empty() {
            let peers: Vec<MemoryId> = purged.peer_edge_counts.iter().map(|(id, _)| *id).collect();
            self.cache.batch_invalidate(&peers).await;
        }

        // Whole-namespace FTS removal, not per record: it also reaches
        // rows whose meta.db record was already gone. RemoveNode, not
        // MarkTombstoned: the records are destroyed, so a surviving node
        // would dangle.
        let index_counts = crate::index_cleanup::purge_from_indexes(
            &self.index_handles(),
            &purged.records,
            crate::index_cleanup::FtsAction::WholeNamespace(purged.config.id),
            crate::index_cleanup::GraphAction::RemoveNode,
        )
        .await;

        self.complete_effects().await?;
        Ok(bridge::DeleteNamespaceResult {
            name: purged.config.name.clone(),
            id: purged.config.id.get(),
            deleted: true,
            memories_deleted: purged.live_removed as u64,
            tombstones_purged: purged.tombstones_removed as u64,
            edges_removed: purged.edges_removed as u64,
            fts_rows_removed: index_counts.fts_removed as u64,
            vector_file_removed: purged.directory_removed,
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

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::bridge::{NamespaceRegistry, StorageEngine as BridgeStorageEngine};
    use crate::model::constants::DUPLICATE_SIMILARITY_THRESHOLD;
    use crate::test_support::{Fixture, MemorySpec};

    fn default_ns() -> NamespaceId {
        NamespaceId::new(1)
    }

    /// Find one namespace in a `list_namespaces` result by name.
    fn find<'a>(namespaces: &'a [bridge::NamespaceInfo], name: &str) -> &'a bridge::NamespaceInfo {
        namespaces
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("namespace '{name}' missing from list_namespaces"))
    }

    // ── list_namespaces ──────────────────────────────────────────────

    /// The regression: `memory_count` was hardcoded to 0, so every
    /// namespace reported as empty no matter what it held.
    #[tokio::test]
    async fn a41_list_namespaces_reports_the_live_memory_count() {
        let fx = Fixture::new();
        for _ in 0..3 {
            fx.insert_memory().await;
        }

        let namespaces = fx.namespace_adapter().list_namespaces().await.unwrap();
        assert_eq!(find(&namespaces, "default").memory_count, 3);
    }

    #[tokio::test]
    async fn a42_list_namespaces_excludes_tombstoned_memories() {
        let fx = Fixture::new();
        let doomed = fx.insert_memory().await;
        fx.insert_memory().await;
        fx.tombstone(doomed);

        let namespaces = fx.namespace_adapter().list_namespaces().await.unwrap();
        assert_eq!(find(&namespaces, "default").memory_count, 1);
    }

    #[tokio::test]
    async fn a43_list_namespaces_reports_zero_for_an_empty_namespace() {
        let fx = Fixture::new();
        fx.create_namespace("empty");
        // A memory in `default` must not be counted against `empty`.
        fx.insert_memory().await;

        let namespaces = fx.namespace_adapter().list_namespaces().await.unwrap();
        assert_eq!(find(&namespaces, "empty").memory_count, 0);
        assert_eq!(find(&namespaces, "default").memory_count, 1);
    }

    /// Each namespace is counted on its own members, not on the whole
    /// database.
    #[tokio::test]
    async fn a43b_counts_are_per_namespace() {
        let fx = Fixture::new();
        let other = fx.create_namespace("other");
        fx.insert_memory_with(MemorySpec::new(other)).await;
        fx.insert_memory_with(MemorySpec::new(default_ns())).await;

        let namespaces = fx.namespace_adapter().list_namespaces().await.unwrap();
        assert_eq!(find(&namespaces, "other").memory_count, 1);
        assert_eq!(find(&namespaces, "default").memory_count, 1);
    }

    // ── namespace_stats ──────────────────────────────────────────────

    /// The regression: tombstoned records were counted as memories, so
    /// `memoryCount` disagreed with what `list_memories` would return and
    /// there was no way to tell a deleted memory from a lost one.
    #[tokio::test]
    async fn a44_namespace_stats_excludes_tombstones_and_reports_them_separately() {
        let fx = Fixture::new();
        let a = fx.insert_memory().await;
        let b = fx.insert_memory().await;
        fx.insert_memory().await;
        fx.tombstone(a);
        fx.tombstone(b);

        let stats = fx
            .namespace_adapter()
            .namespace_stats("default")
            .await
            .unwrap();
        assert_eq!(stats.memory_count, 1);
        assert_eq!(stats.tombstone_count, 2);
    }

    /// The tell: `memoryCount` is assertable against the phase breakdown,
    /// which it was not while tombstones were folded into it.
    #[tokio::test]
    async fn a45_memory_count_equals_the_sum_of_the_phase_counts() {
        let fx = Fixture::new();
        for phase in [DecayPhase::Full, DecayPhase::Summary, DecayPhase::Ghost] {
            fx.insert_memory_with(MemorySpec {
                phase,
                ..MemorySpec::new(default_ns())
            })
            .await;
        }
        let doomed = fx.insert_memory().await;
        fx.tombstone(doomed);

        let stats = fx
            .namespace_adapter()
            .namespace_stats("default")
            .await
            .unwrap();
        let p = &stats.phase_counts;
        assert_eq!(stats.memory_count, p.full + p.summary + p.ghost);
        assert_eq!(stats.memory_count, 3);
    }

    /// `tombstone` zeroes `decay_strength`, so counting tombstones dragged
    /// the average toward zero: one live memory at 0.8 beside two
    /// tombstones reported ~0.27.
    #[tokio::test]
    async fn a46_avg_strength_ignores_tombstoned_zero_strength_rows() {
        let fx = Fixture::new();
        fx.insert_memory_with(MemorySpec {
            decay_strength: 0.8,
            ..MemorySpec::new(default_ns())
        })
        .await;
        for _ in 0..2 {
            let doomed = fx.insert_memory().await;
            fx.tombstone(doomed);
        }

        let stats = fx
            .namespace_adapter()
            .namespace_stats("default")
            .await
            .unwrap();
        assert!(
            (stats.avg_strength - 0.8).abs() < 1e-5,
            "expected ~0.8, got {}",
            stats.avg_strength
        );
    }

    #[tokio::test]
    async fn a47_vector_bytes_counts_only_live_memories() {
        let fx = Fixture::new();
        fx.insert_memory().await;
        let doomed = fx.insert_memory().await;
        fx.tombstone(doomed);

        let stats = fx
            .namespace_adapter()
            .namespace_stats("default")
            .await
            .unwrap();
        // One live memory x DIM dimensions x 4 bytes per f32. A
        // tombstone's slot is freed, so it occupies no live vector bytes.
        assert_eq!(stats.vector_bytes, crate::test_support::DIM as u64 * 4);
    }

    /// `MetadataStore::tombstone` zeroes summary, tags and strength but
    /// LEAVES `is_permastore` and `edge_count` intact, so these two only
    /// come out right because the record is skipped entirely.
    #[tokio::test]
    async fn a48_edge_and_permastore_counts_exclude_tombstones() {
        let fx = Fixture::new();
        let doomed = fx
            .insert_memory_with(MemorySpec {
                is_permastore: true,
                edge_count: 2,
                ..MemorySpec::new(default_ns())
            })
            .await;
        fx.insert_memory_with(MemorySpec {
            is_permastore: true,
            edge_count: 3,
            ..MemorySpec::new(default_ns())
        })
        .await;
        fx.tombstone(doomed);

        let stats = fx
            .namespace_adapter()
            .namespace_stats("default")
            .await
            .unwrap();
        assert_eq!(stats.permastore_count, 1);
        assert_eq!(stats.edge_count, 3);
    }

    /// The three numbers a caller can compare have to agree after a
    /// delete, or one of them is lying.
    #[tokio::test]
    async fn a49_stats_agree_with_list_memories_and_list_namespaces() {
        let fx = Fixture::new();
        for _ in 0..3 {
            fx.insert_memory().await;
        }
        let doomed = fx.insert_memory().await;
        fx.tombstone(doomed);

        let stats = fx
            .namespace_adapter()
            .namespace_stats("default")
            .await
            .unwrap();
        let listed = fx
            .storage_adapter()
            .list_memories(bridge::ListMemoriesInput {
                namespace: "default".into(),
                limit: 50,
                offset: 0,
                tags: Vec::new(),
                entities: Vec::new(),
                time_range_start: None,
                time_range_end: None,
            })
            .await
            .unwrap();
        let namespaces = fx.namespace_adapter().list_namespaces().await.unwrap();

        assert_eq!(stats.memory_count, 3);
        assert_eq!(listed.total, 3);
        assert_eq!(find(&namespaces, "default").memory_count, 3);
    }

    #[tokio::test]
    async fn a50_namespace_stats_are_scoped_to_one_namespace() {
        let fx = Fixture::new();
        let other = fx.create_namespace("other");
        fx.insert_memory_with(MemorySpec::new(default_ns())).await;
        for _ in 0..2 {
            fx.insert_memory_with(MemorySpec::new(other)).await;
        }

        let default_stats = fx
            .namespace_adapter()
            .namespace_stats("default")
            .await
            .unwrap();
        let other_stats = fx
            .namespace_adapter()
            .namespace_stats("other")
            .await
            .unwrap();
        assert_eq!(default_stats.memory_count, 1);
        assert_eq!(other_stats.memory_count, 2);
    }

    // ── list_tags (live storage) ─────────────────────────────────────

    /// Tags as they appear on disk, prefixes included.
    fn tagged<'a>(namespace: NamespaceId, tags: &'a [&'a str]) -> MemorySpec<'a> {
        MemorySpec {
            tags,
            ..MemorySpec::new(namespace)
        }
    }

    fn names(bucket: &bridge::TagBucket) -> Vec<(&str, u64)> {
        bucket
            .items
            .iter()
            .map(|t| (t.name.as_str(), t.count))
            .collect()
    }

    fn scoped(namespace: &str) -> bridge::ListTagsInput {
        bridge::ListTagsInput {
            namespace: Some(namespace.to_string()),
            limit: 50,
        }
    }

    #[tokio::test]
    async fn a51_list_tags_counts_only_the_requested_namespace() {
        let fx = Fixture::new();
        let other = fx.create_namespace("other");
        fx.insert_memory_with(tagged(default_ns(), &["type/decision"]))
            .await;
        fx.insert_memory_with(tagged(default_ns(), &["type/decision"]))
            .await;
        fx.insert_memory_with(tagged(other, &["type/decision"]))
            .await;

        let response = fx
            .storage_adapter()
            .list_tags(scoped("default"))
            .await
            .unwrap();
        assert_eq!(response.namespace.as_deref(), Some("default"));
        assert_eq!(response.memories_counted, Some(2));
        assert_eq!(names(&response.tags), [("type/decision", 2)]);
    }

    #[tokio::test]
    async fn a52_list_tags_excludes_tombstoned_memories() {
        let fx = Fixture::new();
        let doomed = fx
            .insert_memory_with(tagged(default_ns(), &["type/decision"]))
            .await;
        fx.insert_memory_with(tagged(default_ns(), &["type/decision"]))
            .await;
        fx.tombstone(doomed);

        let response = fx
            .storage_adapter()
            .list_tags(scoped("default"))
            .await
            .unwrap();
        assert_eq!(response.memories_counted, Some(1));
        assert_eq!(names(&response.tags), [("type/decision", 1)]);
    }

    #[tokio::test]
    async fn a53_list_tags_aggregates_across_namespaces_in_global_scope() {
        let fx = Fixture::new();
        let other = fx.create_namespace("other");
        fx.insert_memory_with(tagged(default_ns(), &["type/decision"]))
            .await;
        fx.insert_memory_with(tagged(other, &["type/decision"]))
            .await;

        let response = fx
            .storage_adapter()
            .list_tags(bridge::ListTagsInput {
                namespace: None,
                limit: 50,
            })
            .await
            .unwrap();
        assert_eq!(names(&response.tags), [("type/decision", 2)]);
    }

    #[tokio::test]
    async fn a54_list_tags_rejects_an_unknown_namespace() {
        let fx = Fixture::new();
        let err = fx
            .storage_adapter()
            .list_tags(scoped("nope"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, bridge::BridgeError::NotFound(_)),
            "expected NotFound, got {err}"
        );
        assert!(err.to_string().contains("nope"), "{err}");
    }

    /// A phase transition strips text, not tags, so a Summary or Ghost
    /// memory is still part of the namespace's vocabulary.
    #[tokio::test]
    async fn a55_list_tags_counts_summary_and_ghost_phases() {
        let fx = Fixture::new();
        for phase in [DecayPhase::Full, DecayPhase::Summary, DecayPhase::Ghost] {
            fx.insert_memory_with(MemorySpec {
                phase,
                tags: &["type/decision"],
                ..MemorySpec::new(default_ns())
            })
            .await;
        }

        let response = fx
            .storage_adapter()
            .list_tags(scoped("default"))
            .await
            .unwrap();
        assert_eq!(response.memories_counted, Some(3));
        assert_eq!(names(&response.tags), [("type/decision", 3)]);
    }

    /// Global scope reads the tag index directly and touches no memory
    /// records, so there is no honest number to report.
    #[tokio::test]
    async fn a56_list_tags_reports_no_memory_count_in_global_scope() {
        let fx = Fixture::new();
        fx.insert_memory_with(tagged(default_ns(), &["type/decision"]))
            .await;

        let response = fx
            .storage_adapter()
            .list_tags(bridge::ListTagsInput {
                namespace: None,
                limit: 50,
            })
            .await
            .unwrap();
        assert_eq!(response.namespace, None);
        assert_eq!(response.memories_counted, None);
    }

    // ── build_list_tags_response (pure) ──────────────────────────────

    fn histogram(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|(n, c)| ((*n).to_string(), *c)).collect()
    }

    #[test]
    fn a57_build_orders_count_descending_then_name_ascending() {
        let response = build_list_tags_response(
            None,
            None,
            histogram(&[("zebra", 1), ("aardvark", 9), ("moose", 1)]),
            50,
        );
        assert_eq!(
            names(&response.tags),
            [("aardvark", 9), ("moose", 1), ("zebra", 1)]
        );
    }

    #[test]
    fn a58_build_splits_derived_prefixes_and_strips_them() {
        let response = build_list_tags_response(
            None,
            None,
            histogram(&[
                ("entity/recalld", 3),
                ("topic/rust", 2),
                ("emotion/curious", 1),
                ("type/decision", 4),
            ]),
            50,
        );
        assert_eq!(names(&response.entities), [("recalld", 3)]);
        assert_eq!(names(&response.topics), [("rust", 2)]);
        assert_eq!(names(&response.emotions), [("curious", 1)]);
        assert_eq!(names(&response.tags), [("type/decision", 4)]);
    }

    #[test]
    fn a59_build_reports_the_untruncated_total_and_flags_truncation() {
        let response =
            build_list_tags_response(None, None, histogram(&[("a", 3), ("b", 2), ("c", 1)]), 2);
        assert_eq!(
            response.tags.total, 3,
            "total is the size before truncation"
        );
        assert!(response.tags.truncated);
        assert_eq!(names(&response.tags), [("a", 3), ("b", 2)]);

        let untruncated = build_list_tags_response(None, None, histogram(&[("a", 1)]), 2);
        assert_eq!(untruncated.tags.total, 1);
        assert!(!untruncated.tags.truncated);
    }

    /// Containing a slash does not make a tag derived. `project/recalld`
    /// is a plain tag a caller filters on, and moving it to `entities`
    /// would make it unusable there.
    #[test]
    fn a60_build_keeps_a_slashed_plain_tag_in_the_tags_bucket() {
        let response =
            build_list_tags_response(None, None, histogram(&[("project/recalld", 5)]), 50);
        assert_eq!(names(&response.tags), [("project/recalld", 5)]);
        assert!(response.entities.items.is_empty());
        assert_eq!(response.entities.total, 0);
    }

    #[test]
    fn a61_build_treats_a_bare_prefix_as_a_plain_tag() {
        let response = build_list_tags_response(
            None,
            None,
            histogram(&[("entity/", 2), ("topic/", 1), ("emotion/", 1)]),
            50,
        );
        assert_eq!(
            names(&response.tags),
            [("entity/", 2), ("emotion/", 1), ("topic/", 1)],
            "a bare prefix names nothing; it must never become an empty-named entity"
        );
        assert!(response.entities.items.is_empty());
        assert!(response.topics.items.is_empty());
        assert!(response.emotions.items.is_empty());
    }

    // ── near-duplicate advisory ──────────────────────────────────────
    //
    // These route through `PassthroughProvider` and `StoreInput.embedding`
    // — the exact un-validated caller-vector path the normalization guard
    // exists for. The vectors are unit vectors, so they pass that guard,
    // which also demonstrates the guard is not quietly disabling the
    // feature for everyone.

    /// A store with a caller-supplied embedding, defaults elsewhere.
    fn store_input(summary: &str, namespace: &str, embedding: Vec<f32>) -> bridge::StoreInput {
        bridge::StoreInput {
            summary: summary.to_string(),
            full_text: None,
            tags: Vec::new(),
            entities: Vec::new(),
            topics: Vec::new(),
            emotions: Vec::new(),
            namespace: namespace.to_string(),
            embedding: Some(embedding),
            initial_stability: None,
            parent_id: None,
            supersedes: None,
            check_duplicates: None,
        }
    }

    fn unit_x() -> Vec<f32> {
        vec![1.0, 0.0, 0.0, 0.0]
    }

    /// The ordering guard, and the only test that catches it. The check
    /// runs before `index.add`; move it below and the new memory finds
    /// itself at score 1.0 and every store reports a duplicate.
    #[tokio::test]
    async fn f26_a_store_does_not_report_itself_as_its_own_duplicate() {
        let fx = Fixture::new();
        let stored = fx
            .storage_adapter()
            .store_memory(store_input("the first fact", "default", unit_x()))
            .await
            .expect("store succeeds");

        assert!(
            stored.near_duplicates.is_none(),
            "a store into an empty namespace reported itself: {:?}",
            stored.near_duplicates
        );
    }

    /// End to end, hydration included: the summary comes back, so the
    /// caller can judge the duplicate without a second round trip.
    #[tokio::test]
    async fn f27_a_second_store_of_the_same_embedding_reports_the_first() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();
        let first = adapter
            .store_memory(store_input("the first fact", "default", unit_x()))
            .await
            .expect("first store succeeds");

        let second = adapter
            .store_memory(store_input("the same fact again", "default", unit_x()))
            .await
            .expect("second store succeeds");

        let report = second.near_duplicates.expect("duplicate reported");
        assert_eq!(report.threshold, DUPLICATE_SIMILARITY_THRESHOLD);
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].id.to_string(), first.id);
        assert!(report.matches[0].score >= 0.85, "{:?}", report.matches[0]);
        assert_eq!(report.matches[0].summary, "the first fact");
    }

    /// The guarantee the docs make and the index could not keep.
    /// `find_near_duplicates` asks the vector index for Full and Summary
    /// only, but `FlatVectorIndex` writes each entry's `decay_phase`
    /// once at add time — always Full — and `VectorIndex::update_metadata`
    /// has no callers, so that filter is inert in production. A ghosted
    /// memory was reported as a near-duplicate with an EMPTY summary,
    /// and `store_memory`'s description then told the agent to reinforce
    /// it.
    #[tokio::test]
    async fn w62_a_ghosted_memory_is_not_reported_as_a_near_duplicate() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();
        let first = adapter
            .store_memory(store_input("the first fact", "default", unit_x()))
            .await
            .expect("first store succeeds");
        let first_id = MemoryId::from_uuid(uuid::Uuid::parse_str(&first.id).expect("uuid"));

        // Decay it to Ghost. The vector-index entry keeps saying Full,
        // which is the whole point.
        fx.storage
            .read()
            .expect("storage lock")
            .update_decay_state(first_id, DecayPhase::Ghost, 0.1, 0.1, 1.0, false)
            .expect("ghost the memory");
        fx.cache.invalidate(first_id).await;

        let second = adapter
            .store_memory(store_input("the same fact again", "default", unit_x()))
            .await
            .expect("second store succeeds");

        assert!(
            second.near_duplicates.is_none(),
            "a ghosted memory was reported as a near-duplicate: {:?}",
            second.near_duplicates
        );
    }

    /// The same guarantee for a memory whose record is gone entirely.
    /// A hard delete that the in-memory vector index has not caught up
    /// with used to surface as a match with an empty summary.
    #[tokio::test]
    async fn w63_a_hard_deleted_memory_is_not_reported_as_a_near_duplicate() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();
        let first = adapter
            .store_memory(store_input("the first fact", "default", unit_x()))
            .await
            .expect("first store succeeds");
        let first_id = MemoryId::from_uuid(uuid::Uuid::parse_str(&first.id).expect("uuid"));

        fx.storage
            .write()
            .expect("storage lock")
            .delete_memory(first_id)
            .expect("hard delete");
        fx.cache.invalidate(first_id).await;

        assert!(
            adapter
                .store_memory(store_input("blocked", "default", unit_x()))
                .await
                .is_err(),
            "an unavailable advisory must not silently report no duplicate"
        );
        fx.namespace_adapter().complete_effects().await.unwrap();

        let second = adapter
            .store_memory(store_input("the same fact again", "default", unit_x()))
            .await
            .expect("second store succeeds");

        assert!(
            second.near_duplicates.is_none(),
            "a hard-deleted memory was reported as a near-duplicate: {:?}",
            second.near_duplicates
        );
    }

    #[tokio::test]
    async fn f28_check_duplicates_false_suppresses_the_report() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();
        adapter
            .store_memory(store_input("the first fact", "default", unit_x()))
            .await
            .expect("first store succeeds");

        let mut input = store_input("the same fact again", "default", unit_x());
        input.check_duplicates = Some(false);
        let second = adapter.store_memory(input).await.expect("store succeeds");

        assert!(second.near_duplicates.is_none());
    }

    #[tokio::test]
    async fn f29_a_duplicate_in_another_namespace_is_not_reported() {
        let fx = Fixture::new();
        fx.create_namespace("other");
        let adapter = fx.storage_adapter();
        adapter
            .store_memory(store_input("the first fact", "default", unit_x()))
            .await
            .expect("first store succeeds");

        let second = adapter
            .store_memory(store_input("the same fact again", "other", unit_x()))
            .await
            .expect("second store succeeds");

        assert!(second.near_duplicates.is_none());
    }

    // ── delete_memory: the shared index cleanup ──────────────────────

    /// Pins the refactor onto `index_cleanup::purge_from_indexes`. These
    /// four removals used to be hand-written here; they are now shared
    /// with the HTTP path and with namespace deletion, and the whole
    /// point of sharing them is that all three keep doing all four.
    #[tokio::test]
    async fn w25_delete_memory_still_clears_every_index_and_marks_the_graph_node() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();

        let mut input = store_input("a fact about Sarah", "default", unit_x());
        input.entities = vec!["Sarah".to_string()];
        let stored = adapter.store_memory(input).await.expect("store");
        let id =
            MemoryId::from_uuid(uuid::Uuid::parse_str(&stored.id).expect("stored id is a uuid"));

        // Everything is indexed before the delete.
        assert!(
            fx.graph.read().await.contains(&id),
            "precondition: the store created a graph node"
        );
        assert!(
            !fx.fts_index.lock().await.is_empty().expect("fts"),
            "precondition: the memory is in the FTS index"
        );
        assert_eq!(fx.entity_index.read().await.len(), 1);

        assert!(adapter.delete_memory(id).await.expect("delete"));

        assert!(
            fx.fts_index.lock().await.is_empty().expect("fts"),
            "FTS row survived the delete"
        );
        assert_eq!(
            {
                use crate::search::VectorIndex;
                fx.vector_index.read().await.len()
            },
            0,
            "vector index entry survived the delete"
        );
        assert_eq!(
            fx.entity_index.read().await.len(),
            0,
            "entity index entry survived the delete"
        );

        let graph_r = fx.graph.read().await;
        let node = graph_r
            .get_node(&id)
            .expect("graph node is kept, not removed");
        assert_eq!(node.decay_phase, DecayPhase::Tombstone);
    }

    // ── delete_memories (batch) ──────────────────────────────────────

    #[tokio::test]
    async fn w26_delete_memories_returns_one_flag_per_id_in_input_order() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();

        let live = fx.insert_memory().await;
        let already = fx.insert_memory().await;
        fx.tombstone(already);
        let missing = MemoryId::new();

        let flags = adapter
            .delete_memories(&[missing, live, already])
            .await
            .expect("batch delete");

        assert_eq!(flags, vec![false, true, false]);
        assert_eq!(
            fx.record(live).expect("record survives").phase,
            DecayPhase::Tombstone
        );
    }

    #[tokio::test]
    async fn w27_delete_memories_clears_every_index_for_every_deleted_id() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();

        let mut ids = Vec::new();
        for name in ["Sarah", "Miguel"] {
            let mut input = store_input(&format!("a fact about {name}"), "default", unit_x());
            input.entities = vec![name.to_string()];
            let stored = adapter.store_memory(input).await.expect("store");
            ids.push(MemoryId::from_uuid(
                uuid::Uuid::parse_str(&stored.id).expect("stored id is a uuid"),
            ));
        }

        assert_eq!(fx.entity_index.read().await.len(), 2);

        let flags = adapter.delete_memories(&ids).await.expect("batch delete");
        assert_eq!(flags, vec![true, true]);

        assert!(fx.fts_index.lock().await.is_empty().expect("fts"));
        assert_eq!(
            {
                use crate::search::VectorIndex;
                fx.vector_index.read().await.len()
            },
            0
        );
        assert_eq!(fx.entity_index.read().await.len(), 0);
        let graph_r = fx.graph.read().await;
        for id in &ids {
            assert_eq!(
                graph_r.get_node(id).expect("node kept").decay_phase,
                DecayPhase::Tombstone
            );
        }
    }

    /// Freeing one vector slot twice writes the slot's own index into
    /// its next-pointer, turning the on-disk free list into a
    /// self-cycle: the allocator then hands the same slot to two
    /// different memories, and `live_count` underflows. The batch is the
    /// only place a caller can present the same id twice, so this is
    /// where it has to be impossible.
    #[tokio::test]
    async fn w28_a_duplicate_id_in_one_batch_frees_its_vector_slot_once() {
        let fx = Fixture::new();
        let adapter = fx.storage_adapter();
        let id = fx.insert_memory().await;

        let flags = adapter
            .delete_memories(&[id, id])
            .await
            .expect("batch delete");
        assert_eq!(flags, vec![true, false], "the second pass deleted it again");

        // A corrupted free list shows up as the next two allocations
        // landing on the same slot.
        let first = adapter
            .store_memory(store_input("after the batch", "default", unit_x()))
            .await
            .expect("store");
        let second = adapter
            .store_memory(store_input("and again", "default", unit_x()))
            .await
            .expect("store");

        let slot_of = |stored: &bridge::StoredMemory| {
            fx.record(MemoryId::from_uuid(
                uuid::Uuid::parse_str(&stored.id).expect("uuid"),
            ))
            .expect("record")
            .vector_slot
        };
        assert_ne!(
            slot_of(&first),
            slot_of(&second),
            "two memories were handed the same vector slot: the free list is a cycle"
        );
    }

    // ── delete_namespace ─────────────────────────────────────────────

    /// Set up a namespace holding one fully indexed memory.
    async fn indexed_namespace(fx: &Fixture, name: &str) -> (NamespaceId, MemoryId) {
        let ns = fx.create_namespace(name);
        let mut input = store_input("a fact worth keeping", name, unit_x());
        input.entities = vec!["Sarah".to_string()];
        let stored = fx
            .storage_adapter()
            .store_memory(input)
            .await
            .expect("store");
        let id = MemoryId::from_uuid(uuid::Uuid::parse_str(&stored.id).expect("uuid"));
        (ns, id)
    }

    /// THE most important test in this wave. A refusal that has already
    /// destroyed something is worse than no refusal at all: the caller
    /// reads an error, believes nothing happened, and the memories are
    /// gone. The check runs before `purge_namespace` inside the same
    /// write lock, and this asserts the whole state is untouched — not
    /// just that an error came back.
    #[tokio::test]
    async fn w29_a_refused_non_force_delete_destroys_nothing() {
        let fx = Fixture::new();
        let (_ns, id) = indexed_namespace(&fx, "scratch").await;

        let err = fx
            .namespace_adapter()
            .delete_namespace(bridge::DeleteNamespaceInput {
                name: "scratch".into(),
                force: false,
            })
            .await
            .expect_err("a non-empty namespace must be refused");

        assert!(
            matches!(err, bridge::BridgeError::InvalidInput(ref m) if m.contains("still holds 1")),
            "the refusal must name the count: {err}"
        );

        assert!(fx.record(id).is_some(), "the record was destroyed");
        assert!(
            fx.namespace_dir("scratch").join("vectors.dat").exists(),
            "the vector file was destroyed"
        );
        assert!(
            !fx.fts_index.lock().await.is_empty().expect("fts"),
            "the FTS rows were destroyed"
        );
        assert!(
            fx.storage
                .read()
                .expect("storage lock")
                .get_namespace_by_name("scratch")
                .expect("lookup")
                .is_some(),
            "the namespace config row was destroyed"
        );
    }

    /// A namespace holding only tombstones reports memoryCount 0 in
    /// `list_namespaces` and `namespace_stats`, so refusing it without
    /// force would be a refusal the caller cannot explain. The purge
    /// still reaps the rows — which is the whole point, since nothing
    /// else in the system ever does — and the result says how many.
    #[tokio::test]
    async fn w30_a_tombstone_only_namespace_deletes_without_force() {
        let fx = Fixture::new();
        let ns = fx.create_namespace("scratch");
        let a = fx.insert_memory_in(ns, &[]).await;
        let b = fx.insert_memory_in(ns, &[]).await;
        fx.tombstone(a);
        fx.tombstone(b);

        let result = fx
            .namespace_adapter()
            .delete_namespace(bridge::DeleteNamespaceInput {
                name: "scratch".into(),
                force: false,
            })
            .await
            .expect("a namespace holding only tombstones is empty");

        assert!(result.deleted);
        assert_eq!(result.memories_deleted, 0);
        assert_eq!(
            result.tombstones_purged, 2,
            "the tombstones were not reaped"
        );
        assert!(fx.record(a).is_none());
        assert!(fx.record(b).is_none());
    }

    #[tokio::test]
    async fn w31_a_forced_delete_removes_the_records_indexes_and_directory() {
        let fx = Fixture::new();
        let (_ns, id) = indexed_namespace(&fx, "scratch").await;
        let other = fx.insert_memory_in(NamespaceId::new(1), &[]).await;

        let result = fx
            .namespace_adapter()
            .delete_namespace(bridge::DeleteNamespaceInput {
                name: "scratch".into(),
                force: true,
            })
            .await
            .expect("forced delete");

        assert!(result.deleted);
        assert_eq!(result.memories_deleted, 1);
        assert_eq!(result.fts_rows_removed, 1);
        assert!(result.vector_file_removed);

        assert!(fx.record(id).is_none(), "the record survived");
        assert!(
            !fx.namespace_dir("scratch").exists(),
            "the directory survived"
        );
        let hits = fx
            .fts_index
            .lock()
            .await
            .search(NamespaceId::new(1), "fixture", 10)
            .unwrap();
        assert_eq!(
            hits.iter().map(|h| h.0).collect::<Vec<_>>(),
            vec![other],
            "completion rebuilds canonical survivors"
        );
        assert_eq!(
            {
                use crate::search::VectorIndex;
                fx.vector_index.read().await.len()
            },
            1
        );
        assert_eq!(fx.entity_index.read().await.len(), 0);
        assert!(
            !fx.graph.read().await.contains(&id),
            "the graph node survived"
        );
        assert!(
            fx.storage
                .read()
                .expect("storage lock")
                .get_namespace_by_name("scratch")
                .expect("lookup")
                .is_none()
        );
        assert!(
            fx.record(other).is_some(),
            "a memory in another namespace was destroyed"
        );
    }

    /// Startup recreates a missing default under the SAME directory name
    /// with a NEW namespace id, so a deleted default leaves a stale
    /// vectors.dat that the recreated namespace would open and reuse —
    /// handing unrelated memories somebody else's embeddings. Refused
    /// even with force, and even when the namespace is empty, because
    /// neither of those makes the recreation any less automatic.
    #[tokio::test]
    async fn w32_the_default_namespace_is_refused_even_forced_and_even_empty() {
        let fx = Fixture::new();
        let adapter = fx.namespace_adapter();

        for force in [false, true] {
            let err = adapter
                .delete_namespace(bridge::DeleteNamespaceInput {
                    name: "default".into(),
                    force,
                })
                .await
                .expect_err("the default namespace must never be deletable");
            assert!(
                matches!(err, bridge::BridgeError::InvalidInput(_)),
                "force={force}: {err}"
            );
        }

        assert!(
            fx.namespace_dir("default").join("vectors.dat").exists(),
            "the default namespace's vector file was removed"
        );
    }

    #[tokio::test]
    async fn w33_an_unknown_namespace_is_not_found_and_touches_nothing() {
        let fx = Fixture::new();
        let id = fx.insert_memory().await;

        let err = fx
            .namespace_adapter()
            .delete_namespace(bridge::DeleteNamespaceInput {
                name: "no-such-namespace".into(),
                force: true,
            })
            .await
            .expect_err("an unknown namespace is not found");

        assert!(matches!(err, bridge::BridgeError::NotFound(_)), "{err}");
        assert!(fx.record(id).is_some());
        assert!(fx.namespace_dir("default").exists());
    }

    /// The stale-vectors.dat regression, from the other direction: a
    /// namespace recreated under a name that was just deleted must get a
    /// fresh, empty vector file. If the directory survived the delete,
    /// the new namespace would open the old file and hand its slots out
    /// again.
    #[tokio::test]
    async fn w34_recreating_a_deleted_namespace_gets_a_fresh_vector_file() {
        let fx = Fixture::new();
        let (old_ns, old_id) = indexed_namespace(&fx, "scratch").await;
        let namespace_adapter = fx.namespace_adapter();

        namespace_adapter
            .delete_namespace(bridge::DeleteNamespaceInput {
                name: "scratch".into(),
                force: true,
            })
            .await
            .expect("forced delete");

        let recreated = namespace_adapter
            .create_namespace(bridge::CreateNamespaceInput {
                name: "scratch".into(),
                embedding_dim: Some(crate::test_support::DIM as u16),
                initial_stability: None,
                desired_retention: None,
                decay_rate_multiplier: None,
            })
            .await
            .expect("recreate");

        assert_ne!(
            recreated.id,
            old_ns.get(),
            "namespace ids must never be recycled"
        );
        assert_eq!(recreated.memory_count, 0);

        let stored = fx
            .storage_adapter()
            .store_memory(store_input("a brand new fact", "scratch", unit_x()))
            .await
            .expect("store into the recreated namespace");
        let new_id = MemoryId::from_uuid(uuid::Uuid::parse_str(&stored.id).expect("uuid"));

        assert_eq!(
            fx.record(new_id).expect("record").vector_slot,
            0,
            "the recreated namespace inherited the deleted one's vector file"
        );
        assert!(fx.record(old_id).is_none());
    }

    // ── unfilterable labels ──────────────────────────────────────────

    /// `entity/josé` fails `Tag`'s alphabet. The `if let Ok` around it
    /// swallowed that, the filter was dropped, and `list_memories`
    /// returned EVERY memory in the namespace — presented as the result
    /// of filtering by entity. An agent acting on "these are the
    /// memories about José" got the whole namespace.
    #[tokio::test]
    async fn w66_an_unfilterable_entity_is_an_error_not_a_wider_query() {
        let fx = Fixture::new();
        let visible = fx.insert_memory().await;

        let err = fx
            .storage_adapter()
            .list_memories(bridge::ListMemoriesInput {
                namespace: "default".into(),
                limit: 10,
                offset: 0,
                tags: Vec::new(),
                entities: vec!["José".to_string()],
                time_range_start: None,
                time_range_end: None,
            })
            .await
            .expect_err("an unfilterable entity must not widen the query");

        assert!(
            matches!(err, bridge::BridgeError::InvalidInput(ref m) if m.contains("José")),
            "the error must name the label: {err:?}"
        );
        assert!(
            fx.record(visible).is_some(),
            "the fixture memory should still exist"
        );
    }

    /// The recall path builds the same filter from four groups; any of
    /// them can carry the bad label, and each must name ITS field so the
    /// caller knows which argument to fix.
    #[test]
    fn w67_filter_tags_names_the_group_that_carried_the_bad_label() {
        let ok = vec!["fine".to_string()];
        let bad = vec!["José".to_string()];

        for field in ["tags", "entities", "topics", "emotions"] {
            let groups: Vec<(&str, &str, &Vec<String>)> =
                ["tags", "entities", "topics", "emotions"]
                    .iter()
                    .map(|g| {
                        let prefix = match *g {
                            "entities" => crate::model::constants::ENTITY_TAG_PREFIX,
                            "topics" => crate::model::constants::TOPIC_TAG_PREFIX,
                            "emotions" => crate::model::constants::EMOTION_TAG_PREFIX,
                            _ => "",
                        };
                        (*g, prefix, if *g == field { &bad } else { &ok })
                    })
                    .collect();

            let err = filter_tags(&groups, "recall_memories")
                .expect_err("an unfilterable label must not be dropped");
            assert!(err.contains(field), "{field}: {err}");
            assert!(err.contains("José"), "{field}: {err}");
        }

        // All four groups valid: every label becomes a prefixed tag.
        let groups: Vec<(&str, &str, &Vec<String>)> = vec![
            ("tags", "", &ok),
            ("entities", crate::model::constants::ENTITY_TAG_PREFIX, &ok),
            ("topics", crate::model::constants::TOPIC_TAG_PREFIX, &ok),
            ("emotions", crate::model::constants::EMOTION_TAG_PREFIX, &ok),
        ];
        let tags: Vec<String> = filter_tags(&groups, "recall_memories")
            .expect("all four groups are valid")
            .iter()
            .map(|t| t.as_str().to_string())
            .collect();
        assert_eq!(
            tags,
            vec!["fine", "entity/fine", "topic/fine", "emotion/fine"]
        );
    }

    // ── namespace-name validation on the socket's own entry point ────

    /// `validate_namespace_name` ran at the tool layer and the HTTP
    /// layer and nowhere else, but the daemon dispatches
    /// `create_namespace` straight into this adapter. A namespace name
    /// becomes a directory name, so an unvalidated one becomes a path.
    #[tokio::test]
    async fn w45_create_namespace_validates_the_name_in_the_adapter() {
        let fx = Fixture::new();
        let adapter = fx.namespace_adapter();

        for bad in ["", "..", "../evil", "has space", "ünïcode"] {
            let err = adapter
                .create_namespace(bridge::CreateNamespaceInput {
                    name: bad.to_string(),
                    embedding_dim: Some(crate::test_support::DIM as u16),
                    initial_stability: None,
                    desired_retention: None,
                    decay_rate_multiplier: None,
                })
                .await
                .expect_err("an unusable namespace name must be refused");
            assert!(
                matches!(err, bridge::BridgeError::InvalidInput(_)),
                "{bad:?}: {err:?}"
            );
        }

        assert_eq!(
            fx.storage
                .read()
                .expect("storage lock")
                .list_namespaces()
                .expect("list")
                .len(),
            1,
            "a refused create left a namespace row behind"
        );
    }

    /// `delete_namespace` checked `!= "default"` and nothing else. `""`
    /// passed that check, named a namespace that really was live, and
    /// resolved to the data directory itself.
    #[tokio::test]
    async fn w46_delete_namespace_refuses_an_unusable_name_as_invalid_input() {
        let fx = Fixture::new();
        let adapter = fx.namespace_adapter();

        for bad in ["", "..", "../default", "has space"] {
            let err = adapter
                .delete_namespace(bridge::DeleteNamespaceInput {
                    name: bad.to_string(),
                    force: true,
                })
                .await
                .expect_err("an unusable namespace name must be refused");
            assert!(
                matches!(err, bridge::BridgeError::InvalidInput(_)),
                "{bad:?}: {err:?}"
            );
        }
    }

    /// The migration case, and the reason the refusal is by name rather
    /// than by lookup: a database created before the validator reached
    /// this layer can already hold a row named `""`, whose directory
    /// **is** the data directory. Planting the row directly is the only
    /// way to reach that state now.
    #[tokio::test]
    async fn w47_a_legacy_empty_named_namespace_cannot_destroy_the_database() {
        let fx = Fixture::new();

        {
            let mut config = crate::model::NamespaceConfig::default_namespace(0);
            config.name = String::new();
            config.embedding_dim = crate::test_support::DIM as u32;
            fx.storage
                .write()
                .expect("storage lock")
                .meta_store()
                .create_namespace(&config)
                .expect("plant a legacy row");
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

    // ── case-insensitive namespace identity ──────────────────────────

    /// Chain B. `create_namespace("Default")` passed the exact-match
    /// uniqueness check, committed its row, and only then failed on the
    /// default namespace's exclusive flock — so the caller saw an error
    /// and the row survived. `delete_namespace("Default", force: true)`
    /// then walked past `"Default" != "default"` and pointed
    /// `remove_dir_all` at the real `default/`, taking its vectors.dat
    /// with it. On the next boot every record's vector_slot is past EOF.
    #[tokio::test]
    async fn w48_the_default_namespace_is_refused_under_any_casing() {
        let fx = Fixture::new();
        let adapter = fx.namespace_adapter();

        for spelling in ["Default", "DEFAULT", "dEfAuLt"] {
            let create = adapter
                .create_namespace(bridge::CreateNamespaceInput {
                    name: spelling.to_string(),
                    embedding_dim: Some(crate::test_support::DIM as u16),
                    initial_stability: None,
                    desired_retention: None,
                    decay_rate_multiplier: None,
                })
                .await;
            assert!(
                create.is_err(),
                "{spelling} was created alongside the default namespace"
            );

            let err = adapter
                .delete_namespace(bridge::DeleteNamespaceInput {
                    name: spelling.to_string(),
                    force: true,
                })
                .await
                .expect_err("the default namespace must never be deletable");
            assert!(
                matches!(err, bridge::BridgeError::InvalidInput(_)),
                "{spelling}: {err:?}"
            );
        }

        assert!(
            fx.namespace_dir("default").join("vectors.dat").exists(),
            "the default namespace's vector file was destroyed"
        );
        assert_eq!(
            fx.storage
                .read()
                .expect("storage lock")
                .list_namespaces()
                .expect("list")
                .len(),
            1,
            "a case-variant of 'default' left a namespace row behind"
        );
    }
}
