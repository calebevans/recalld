//! Auto-link candidate discovery, threshold logic, and batch operations.
//!
//! Provides embedding-similarity-based auto-linking for newly inserted
//! memories, with model-specific thresholds, tag-aware adjustment, and
//! hub prevention via link caps.

use std::sync::Arc;

use crate::cache::CacheManager;
use crate::graph::SharedGraph;
use crate::graph::structure::RelationshipGraph;
use crate::model::{DecayPhase, EdgeType, MemoryId, NamespaceId};
use crate::search::{EntityIndex, FlatVectorIndex, SearchFilter, VectorIndex};
use crate::storage::engine::RedbStorageEngine;
use crate::storage::{PersistedEdge, StorageEngine as StorageEngineTrait};

// ═══════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════

/// Hard floor for similarity threshold. No embedding model produces
/// meaningful "related" signals below this.
pub const THRESHOLD_HARD_FLOOR: f32 = 0.40;

/// Maximum auto-created associative edges per memory.
/// Prevents hub nodes and quadratic edge growth.
pub const DEFAULT_MAX_LINKS: usize = 15;

/// Threshold reduction when new memory shares tags with a candidate.
/// Applied per-pair, capped at 0.05 regardless of shared tag count.
pub const TAG_THRESHOLD_ADJUSTMENT: f32 = 0.05;

// ═══════════════════════════════════════════════════════════════════════
// AutoLinkThresholds
// ═══════════════════════════════════════════════════════════════════════

/// Default similarity thresholds by embedding model family.
///
/// These are starting points -- the actual threshold is stored in
/// namespace configuration and tunable at runtime.
///
/// Thresholds are NOT portable across models. Score distributions
/// vary significantly (ada-002 compresses scores upward; MiniLM
/// spreads them wide). See Spec 05 section 3.2.
pub struct AutoLinkThresholds;

impl AutoLinkThresholds {
    /// Return the recommended threshold for a model family.
    /// Falls back to 0.60 for unknown models.
    pub fn default_for_model(model_name: &str) -> f32 {
        let lower = model_name.to_lowercase();
        if lower.contains("ada-002") {
            0.79
        } else if lower.contains("text-embedding-3-small")
            || lower.contains("text-embedding-3-large")
        {
            0.50
        } else if lower.contains("minilm") || lower.contains("all-minilm") {
            0.60
        } else if lower.contains("nomic-embed") {
            0.60
        } else {
            0.60 // conservative default
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// AutoLinkCandidate
// ═══════════════════════════════════════════════════════════════════════

/// A candidate for auto-linking, returned by the discovery step.
#[derive(Debug, Clone)]
pub struct AutoLinkCandidate {
    /// The candidate memory's ID.
    pub memory_id: MemoryId,
    /// Cosine similarity between the new memory and this candidate.
    pub similarity: f32,
    /// Whether tag adjustment was applied to the threshold.
    pub shared_tags: bool,
}

// ═══════════════════════════════════════════════════════════════════════
// auto_link
// ═══════════════════════════════════════════════════════════════════════

/// Discover and create auto-links for a newly inserted memory.
///
/// This function performs both phases of auto-linking:
/// 1. **Discovery**: find candidate memories above the similarity threshold
/// 2. **Filtering**: exclude candidates that already have edges, apply cap
///
/// The caller is responsible for persisting the returned edges to `edges.db`
/// and inserting them into the in-memory graph.
///
/// # Arguments
/// - `new_memory`: the newly inserted memory's ID
/// - `candidates`: pre-computed (memory_id, similarity) pairs from the
///   vector search buffer. The caller runs the similarity search against
///   `EmbeddingBuffer` and passes results here. This separation keeps
///   autolink.rs decoupled from the vector storage layer.
/// - `threshold`: namespace-configured similarity threshold
/// - `new_tags`: tags on the new memory (for tag-aware threshold adjustment)
/// - `existing_tags`: function to retrieve tags for a candidate memory
///   (avoids coupling to the metadata storage layer)
/// - `max_links`: cap on auto-created edges (default: 15)
/// - `graph`: reference to check for existing edges
///
/// # Returns
/// Edges to create, sorted by descending similarity. Each tuple is
/// (target_memory_id, EdgeType::Associative). The weight of each edge
/// should be set to the similarity score.
pub fn auto_link(
    new_memory: MemoryId,
    candidates: &[(MemoryId, f32)],
    threshold: f32,
    new_tags: &[String],
    existing_tags: &dyn Fn(&MemoryId) -> Vec<String>,
    max_links: usize,
    graph: &RelationshipGraph,
) -> Vec<(MemoryId, EdgeType)> {
    // Enforce hard floor
    let effective_threshold = threshold.max(THRESHOLD_HARD_FLOOR);

    let mut accepted: Vec<AutoLinkCandidate> = Vec::new();

    for &(ref candidate_id, similarity) in candidates {
        // Skip self
        if *candidate_id == new_memory {
            continue;
        }

        // Skip if any edge already exists between new_memory and candidate
        if graph.contains(candidate_id) && graph.contains(&new_memory) {
            if graph.has_typed_edge_between_ids(&new_memory, candidate_id, EdgeType::Associative) {
                continue;
            }
        }

        // Tag-aware threshold adjustment
        let candidate_tags = existing_tags(candidate_id);
        let shares_tags = new_tags.iter().any(|t| candidate_tags.contains(t));
        let adjusted_threshold = if shares_tags {
            (effective_threshold - TAG_THRESHOLD_ADJUSTMENT).max(THRESHOLD_HARD_FLOOR)
        } else {
            effective_threshold
        };

        if similarity >= adjusted_threshold {
            accepted.push(AutoLinkCandidate {
                memory_id: *candidate_id,
                similarity,
                shared_tags: shares_tags,
            });
        }
    }

    // Sort by similarity descending, take top max_links
    accepted.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    accepted.truncate(max_links);

    accepted
        .into_iter()
        .map(|c| (c.memory_id, EdgeType::Associative))
        .collect()
}


// ═══════════════════════════════════════════════════════════════════════
// AutoLinkError
// ═══════════════════════════════════════════════════════════════════════

/// Errors that can occur during the auto-link orchestration.
#[derive(Debug, thiserror::Error)]
pub enum AutoLinkError {
    /// Vector similarity search failed.
    #[error("vector search failed: {0}")]
    VectorSearch(String),
    /// Graph mutation error (e.g., missing node or duplicate edge).
    #[error("graph error: {0}")]
    Graph(#[from] crate::graph::GraphError),
    /// Storage persistence error.
    #[error("storage error: {0}")]
    Storage(String),
    /// A shared lock was poisoned by a panicking thread.
    #[error("lock poisoned: {0}")]
    LockPoisoned(String),
}

// ═══════════════════════════════════════════════════════════════════════
// persist_edges — shared helper for edge persistence + graph insertion
// ═══════════════════════════════════════════════════════════════════════

/// Persist a batch of edges to storage, insert them into the in-memory
/// graph, and update the edge count on the source memory.
///
/// This helper consolidates the boilerplate that was previously duplicated
/// across `perform_autolink`, `perform_entity_link`, and `perform_temporal_link`.
///
/// # Returns
///
/// The number of edges successfully inserted into the graph.
pub(crate) async fn persist_edges(
    new_memory_id: MemoryId,
    persisted_edges: &[PersistedEdge],
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) -> Result<usize, AutoLinkError> {
    if persisted_edges.is_empty() {
        return Ok(0);
    }

    // Step 1: Persist edges to edges.db.
    {
        let storage_r = storage
            .read()
            .map_err(|e| AutoLinkError::LockPoisoned(format!("storage lock poisoned: {e}")))?;
        storage_r
            .batch_add_edges(persisted_edges)
            .map_err(|e| AutoLinkError::Storage(e.to_string()))?;
    }

    // Step 2: Write-lock graph for edge insertion.
    let edges_created = {
        let mut graph_w = graph.write().await;
        let mut created = 0usize;
        for pe in persisted_edges {
            match graph_w.add_edge(pe.source, pe.target, pe.edge_type, pe.weight, true) {
                Ok(_) => created += 1,
                Err(crate::graph::GraphError::EdgeExists(_, _)) => {}
                Err(crate::graph::GraphError::MemoryNotFound(_)) => {}
                Err(e) => return Err(AutoLinkError::Graph(e)),
            }
        }
        created
    };

    // Step 3: Additive edge_count update.
    if edges_created > 0 {
        let current_count = cache
            .get(new_memory_id)
            .await
            .map(|r| r.edge_count)
            .unwrap_or(0);
        let new_count = current_count + edges_created as u16;
        {
            let storage_r = storage
                .read()
                .map_err(|e| AutoLinkError::LockPoisoned(format!("storage lock poisoned: {e}")))?;
            let _ = storage_r.update_edge_count(new_memory_id, new_count);
        }
        cache.update_edge_count(new_memory_id, new_count).await;
    }

    Ok(edges_created)
}

// ═══════════════════════════════════════════════════════════════════════
// perform_autolink
// ═══════════════════════════════════════════════════════════════════════

/// Orchestrate auto-link discovery and persistence for a newly stored memory.
///
/// This function:
/// 1. Searches the vector index for similar candidates
/// 2. Calls `auto_link()` to filter and rank them
/// 3. Persists edges to edges.db via `batch_add_edges()`
/// 4. Inserts edges into the in-memory graph
/// 5. Updates `edge_count` on the new memory's DiskRecord
///
/// Returns the number of edges created.
///
/// # Lock ordering
///
/// The graph `RwLock` is acquired twice:
/// - First as a **read** lock for `auto_link()` candidate discovery
/// - Then as a **write** lock for edge insertion
///
/// The read lock is explicitly dropped before the write lock is acquired
/// to prevent deadlocks.
pub async fn perform_autolink(
    new_memory_id: MemoryId,
    namespace_id: NamespaceId,
    embedding: &[f32],
    tags: &[String],
    threshold: f32,
    max_links: usize,
    vector_index: &Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) -> Result<usize, AutoLinkError> {
    // Step 1: Search vector index for top candidates.
    let search_limit = max_links * 2 + 1;
    let candidates: Vec<(MemoryId, f32)> = {
        let index = vector_index.read().await;
        let filter = SearchFilter {
            namespace_id: Some(namespace_id),
            decay_phases: Some(vec![DecayPhase::Full.as_u8(), DecayPhase::Summary.as_u8()]),
            ..SearchFilter::default()
        };
        let results = index
            .search(embedding, search_limit, &filter)
            .map_err(|e| AutoLinkError::VectorSearch(e.to_string()))?;
        results.into_iter().map(|r| (r.id, r.score)).collect()
    };

    if candidates.is_empty() {
        return Ok(0);
    }

    // Step 2: Build tag lookup closure from cache.
    // Collect candidate tags up front to avoid holding locks in the closure.
    let candidate_ids: Vec<MemoryId> = candidates.iter().map(|(id, _)| *id).collect();
    let mut candidate_tags_map = std::collections::HashMap::new();
    for cid in &candidate_ids {
        if let Some(cached) = cache.get(*cid).await {
            candidate_tags_map.insert(
                *cid,
                cached
                    .tags
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>(),
            );
        }
    }

    let tag_lookup =
        |id: &MemoryId| -> Vec<String> { candidate_tags_map.get(id).cloned().unwrap_or_default() };

    // Step 3: Read-lock graph for auto_link() discovery, then DROP the lock.
    let edges_to_create = {
        let graph_r = graph.read().await;
        auto_link(
            new_memory_id,
            &candidates,
            threshold,
            tags,
            &tag_lookup,
            max_links,
            &graph_r,
        )
    }; // graph read lock dropped here

    if edges_to_create.is_empty() {
        return Ok(0);
    }

    // Step 4: Build PersistedEdge records with similarity scores as weights.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Build a lookup for candidate similarity scores.
    let similarity_map: std::collections::HashMap<MemoryId, f32> =
        candidates.iter().map(|(id, score)| (*id, *score)).collect();

    let persisted_edges: Vec<PersistedEdge> = edges_to_create
        .iter()
        .map(|(target_id, edge_type)| PersistedEdge {
            source: new_memory_id,
            target: *target_id,
            edge_type: *edge_type,
            weight: similarity_map.get(target_id).copied().unwrap_or(0.0),
            auto_created: true,
            created_at: now_ms,
        })
        .collect();

    // Steps 5-7: Persist edges, insert into graph, update edge count.
    persist_edges(new_memory_id, &persisted_edges, graph, storage, cache).await
}

// ═══════════════════════════════════════════════════════════════════════
// perform_entity_link
// ═══════════════════════════════════════════════════════════════════════

/// Create Entity edges between a newly stored memory and other memories
/// sharing named entities (e.g., both mention "Caroline").
///
/// Edge weight = Jaccard similarity (shared_count / union_count).
pub async fn perform_entity_link(
    new_memory_id: MemoryId,
    _namespace_id: NamespaceId,
    entities: &[String],
    max_entity_links: usize,
    entity_index: &tokio::sync::RwLock<EntityIndex>,
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) -> Result<usize, AutoLinkError> {
    if entities.is_empty() {
        return Ok(0);
    }

    let candidates = {
        let idx = entity_index.read().await;
        idx.find_by_entities(entities, new_memory_id)
    };

    if candidates.is_empty() {
        return Ok(0);
    }

    let new_entity_count = entities.len();

    // Pre-fetch candidate entity counts from cache for Jaccard weight calculation.
    let candidate_entity_counts: std::collections::HashMap<MemoryId, usize> = {
        let mut counts = std::collections::HashMap::with_capacity(candidates.len());
        for (cid, _) in &candidates {
            if let Some(cached) = cache.get(*cid).await {
                counts.insert(*cid, cached.entities.len());
            }
        }
        counts
    };

    // Read-lock graph to filter out existing edges.
    let edges_to_create: Vec<(MemoryId, f32)> = {
        let graph_r = graph.read().await;
        candidates
            .iter()
            .filter(|(cid, _)| {
                !graph_r.has_typed_edge_between_ids(&new_memory_id, cid, EdgeType::Entity)
            })
            .take(max_entity_links)
            .map(|(cid, shared)| {
                // Jaccard similarity: shared / union for order-independent weight.
                // Look up candidate's entity count from cache; fall back to
                // shared count (which gives weight=1.0, a safe overestimate).
                let candidate_entity_count =
                    candidate_entity_counts.get(cid).copied().unwrap_or(*shared);
                let union = new_entity_count + candidate_entity_count - *shared;
                let weight = if union > 0 {
                    (*shared as f32 / union as f32).min(1.0)
                } else {
                    0.0
                };
                (*cid, weight)
            })
            .collect()
    };

    if edges_to_create.is_empty() {
        return Ok(0);
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let persisted_edges: Vec<PersistedEdge> = edges_to_create
        .iter()
        .map(|(target_id, weight)| PersistedEdge {
            source: new_memory_id,
            target: *target_id,
            edge_type: EdgeType::Entity,
            weight: *weight,
            auto_created: true,
            created_at: now_ms,
        })
        .collect();

    persist_edges(new_memory_id, &persisted_edges, graph, storage, cache).await
}

// ═══════════════════════════════════════════════════════════════════════
// perform_temporal_link
// ═══════════════════════════════════════════════════════════════════════

/// Create Temporal edges between a newly stored memory and other memories
/// created within a time window.
///
/// `recent_memories` must be sorted by timestamp ascending. Edge weight
/// = 1.0 - (time_delta / window), floored at 0.05.
pub async fn perform_temporal_link(
    new_memory_id: MemoryId,
    _namespace_id: NamespaceId,
    created_at_ms: i64,
    temporal_window_ms: u64,
    max_temporal_links: usize,
    recent_memories: &[(MemoryId, i64)],
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) -> Result<usize, AutoLinkError> {
    if temporal_window_ms == 0 || recent_memories.is_empty() {
        return Ok(0);
    }

    let window = temporal_window_ms as i64;

    // Find memories within the time window.
    let candidates: Vec<(MemoryId, f32)> = recent_memories
        .iter()
        .filter(|(mid, _)| *mid != new_memory_id)
        .filter_map(|(mid, ts)| {
            let delta = (created_at_ms - ts).abs();
            if delta <= window {
                let weight = (1.0 - delta as f32 / window as f32).max(0.05);
                Some((*mid, weight))
            } else {
                None
            }
        })
        .collect();

    if candidates.is_empty() {
        return Ok(0);
    }

    // Read-lock graph to filter out existing edges.
    let edges_to_create: Vec<(MemoryId, f32)> = {
        let graph_r = graph.read().await;
        candidates
            .iter()
            .filter(|(cid, _)| {
                !graph_r.has_typed_edge_between_ids(&new_memory_id, cid, EdgeType::Temporal)
            })
            .take(max_temporal_links)
            .cloned()
            .collect()
    };

    if edges_to_create.is_empty() {
        return Ok(0);
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let persisted_edges: Vec<PersistedEdge> = edges_to_create
        .iter()
        .map(|(target_id, weight)| PersistedEdge {
            source: new_memory_id,
            target: *target_id,
            edge_type: EdgeType::Temporal,
            weight: *weight,
            auto_created: true,
            created_at: now_ms,
        })
        .collect();

    persist_edges(new_memory_id, &persisted_edges, graph, storage, cache).await
}
