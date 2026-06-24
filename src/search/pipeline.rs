//! QueryEngine — 9-step search pipeline orchestration.
//!
//! Orchestrates embedding, vector search, metadata loading, filtering,
//! FSRS retrievability calculation, RIF suppression, ranking, and
//! response assembly.

use std::sync::Arc;

use tokio::time::Instant;
use tracing::{debug, instrument, warn};

use super::error::{Result, SearchError};
use super::query::{QueryMode, SearchFilter, SearchQuery};
use super::response::{MemoryResponse, SearchResponse, SearchResult, StageTimings};
use crate::model::{AccessKind, CachedRecord, DecayPhase, MemoryId, NamespaceConfig, NamespaceId};

// ---------------------------------------------------------------------------
// Subsystem Traits (DI contracts for QueryEngine)
// ---------------------------------------------------------------------------
// These traits define the dependency-injection contracts the QueryEngine
// holds against its subsystem dependencies. Adapter implementations
// connecting these traits to real subsystems live in `search/adapters.rs`.

/// Resolve namespace name to configuration.
#[async_trait::async_trait]
pub trait NamespaceResolver: Send + Sync {
    /// Look up a namespace by name, returning its configuration.
    async fn resolve(&self, name: &str) -> Result<NamespaceConfig>;
}

/// Per-namespace embedding provider registry.
#[async_trait::async_trait]
pub trait EmbeddingProviderRegistry: Send + Sync {
    /// Embed document text using the provider configured for a namespace.
    async fn embed(&self, namespace: &str, text: &str) -> Result<Vec<f32>>;

    /// Embed text in query context (applies query prefix for asymmetric retrieval).
    async fn embed_query(&self, namespace: &str, text: &str) -> Result<Vec<f32>> {
        self.embed(namespace, text).await
    }
}

/// Per-namespace vector index registry.
pub trait VectorIndexRegistry: Send + Sync {
    /// Search the namespace's vector index for the top-K nearest neighbors.
    fn search(
        &self,
        namespace_id: NamespaceId,
        query_vec: &[f32],
        k: usize,
    ) -> Result<Vec<ScoredResult>>;
}

/// Scored result from vector index search.
#[derive(Debug, Clone, Copy)]
pub struct ScoredResult {
    /// The memory ID this result refers to.
    pub memory_id: MemoryId,
    /// Similarity score from dot product.
    pub score: f32,
}

/// Full-text search index registry (SQLite FTS5).
pub trait FtsIndexRegistry: Send + Sync {
    /// Search the namespace's FTS index for the top-K keyword matches.
    fn search(&self, namespace_id: NamespaceId, query: &str, k: usize) -> Result<Vec<FtsResult>>;
}

/// A single result from FTS5 keyword search.
#[derive(Debug, Clone, Copy)]
pub struct FtsResult {
    /// The memory ID this result refers to.
    pub memory_id: MemoryId,
    /// FTS5 BM25 relevance score (positive float, higher = more relevant).
    pub score: f32,
}

/// RAM cache for CachedRecord lookups.
pub trait RecordCache: Send + Sync {
    /// Look up a record by memory ID in the cache.
    fn get(&self, id: &MemoryId) -> Option<CachedRecord>;
}

/// On-disk metadata storage fallback.
#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync {
    /// Get a single record by ID.
    async fn get(&self, id: &MemoryId) -> Result<Option<CachedRecord>>;
    /// Batch load multiple records. Missing IDs are silently skipped.
    async fn get_batch(&self, ids: &[MemoryId]) -> Result<Vec<CachedRecord>>;
}

/// RIF processor -- applies retrieval-induced forgetting.
pub trait RifProcessor: Send + Sync {
    /// Given the set of retrieved memory IDs, compute suppression adjustments.
    fn compute_suppressions(
        &self,
        retrieved: &[MemoryId],
        neighbor_ids: &[MemoryId],
    ) -> Vec<RifSuppression>;
}

/// A RIF suppression event for a competitor memory.
#[derive(Debug, Clone)]
pub struct RifSuppression {
    /// The memory being suppressed.
    pub target: MemoryId,
    /// Multiplicative factor applied to effective R (e.g., 0.85 = 15% reduction).
    pub suppression_factor: f32,
}

/// Records access events on memories.
#[async_trait::async_trait]
pub trait AccessRecorder: Send + Sync {
    /// Record that a memory was accessed.
    async fn record_access(&self, id: MemoryId, kind: AccessKind) -> Result<()>;
    /// Batch record access for multiple memories.
    async fn record_access_batch(&self, accesses: &[(MemoryId, AccessKind)]) -> Result<()>;
}

/// Shared graph handle for read-locked graph access.
///
/// The QueryEngine uses this for computing connection bonuses and
/// identifying RIF competitors. In production, this wraps an
/// `Arc<tokio::sync::RwLock<RelationshipGraph>>`.
pub trait GraphReader: Send + Sync {
    /// Get the neighbor IDs for a given memory.
    /// Used by RIF computation (line 601) to find competitors.
    fn neighbors(&self, id: &MemoryId) -> Vec<MemoryId>;

    /// Run spreading activation from seed memories.
    ///
    /// Seeds are (memory_id, initial_activation) pairs, typically
    /// (candidate_id, relevance_score) from vector/FTS search.
    /// Returns (memory_id, activation_score) for all activated nodes
    /// that are NOT in the seed set.
    ///
    /// Default returns empty (no graph expansion).
    fn spreading_activation(
        &self,
        seeds: &[(MemoryId, f32)],
        namespace_id: NamespaceId,
        graph_depth: u8,
    ) -> Vec<(MemoryId, f32)> {
        let _ = (seeds, namespace_id, graph_depth);
        Vec::new()
    }

    /// If this memory has been superseded, return the ID of the memory
    /// that replaces it. Follows the chain to the latest version.
    fn superseded_by(&self, id: &MemoryId) -> Option<MemoryId> {
        let _ = id;
        None
    }
}

/// Per-namespace entity index reader for entity-based recall.
pub trait EntityIndexReader: Send + Sync {
    /// Look up memories sharing entities with the given query entities.
    ///
    /// Returns `EntityRecallResult` entries sorted by descending shared
    /// count. `exclude_id` is excluded from results.
    /// At most `k` results are returned.
    fn find_by_entities(
        &self,
        namespace_id: NamespaceId,
        entities: &[String],
        exclude_id: MemoryId,
        k: usize,
    ) -> Result<Vec<EntityRecallResult>>;
}

/// A single result from entity index recall.
#[derive(Debug, Clone)]
pub struct EntityRecallResult {
    /// The memory ID sharing entities with the query.
    pub memory_id: MemoryId,
    /// Number of query entities this memory shares.
    pub shared_count: usize,
}

// ---------------------------------------------------------------------------
// Candidate (internal)
// ---------------------------------------------------------------------------

/// Internal candidate struct used during pipeline processing.
struct Candidate {
    record: CachedRecord,
    /// Fused relevance score via convex combination of normalized
    /// vector similarity and FTS keyword scores.
    /// `alpha * norm_vector + (1-alpha) * norm_fts`.
    /// None for MetadataOnly results and graph-expanded candidates.
    relevance_score: Option<f32>,
    /// Raw cosine similarity from vector search (for diagnostics).
    raw_vector_score: Option<f32>,
    /// Raw FTS score (for diagnostics / response).
    raw_fts_score: Option<f32>,
    /// Effective retrievability after connection bonus and RIF.
    effective_r: f32,
    /// Entity overlap score between query and memory entities [0,1].
    entity_score: f32,
    /// Entity recall score: shared_entity_count / query_entity_count.
    /// Non-zero only for candidates discovered via entity index recall
    /// (not found by vector or FTS search). Range: (0.0, 1.0].
    entity_recall_score: f32,
    /// Final ranking score.
    composite_score: f32,
    /// Spreading activation level for graph-discovered candidates.
    /// None for candidates found by vector search, FTS, entity recall,
    /// or metadata scan (i.e., direct matches).
    /// Some(score) for candidates discovered via graph expansion,
    /// where score encodes edge weights, fan attenuation, hop decay,
    /// and neighbor retrievability.
    activation_score: Option<f32>,
}

// ---------------------------------------------------------------------------
// FTS boost constants (used in both search() and compute_composite_score())
// ---------------------------------------------------------------------------

/// Maximum FTS boost added to vector similarity in the relevance score.
const FTS_BOOST_CAP: f32 = 0.05;

/// Rate parameter controlling how quickly FTS boost saturates.
/// Higher values = faster saturation. At 0.5 with FTS_BOOST_CAP=0.15:
///   FTS=1.0 -> boost ~0.059 (39% of cap)
///   FTS=2.0 -> boost ~0.095 (63% of cap)
///   FTS=3.0 -> boost ~0.117 (78% of cap)
///   FTS=5.0 -> boost ~0.138 (92% of cap)
///   FTS=8.0 -> boost ~0.147 (98% of cap)
const FTS_BOOST_RATE: f32 = 0.5;

/// Maximum relevance score for FTS-only candidates (no vector match).
/// Set in the lower range of typical vector similarity (~0.4-0.8) so
/// strong FTS-only results can compete with moderate vector hits.
const FTS_ONLY_CAP: f32 = 0.30;

/// Rate parameter for FTS-only saturating curve. At 0.3 with FTS_ONLY_CAP=0.50:
///   FTS=1.0  -> 0.130  (26% of cap)
///   FTS=2.0  -> 0.225  (45% of cap)
///   FTS=3.0  -> 0.296  (59% of cap)
///   FTS=5.0  -> 0.388  (78% of cap)
///   FTS=8.0  -> 0.455  (91% of cap)
///   FTS=15.0 -> 0.495  (99% of cap)
const FTS_ONLY_RATE: f32 = 0.3;

// ---------------------------------------------------------------------------
// QueryEngine
// ---------------------------------------------------------------------------

/// Orchestrates the 9-step search pipeline.
///
/// Holds `Arc` references to all subsystems. Cheap to clone, safe to
/// share across request handlers.
pub struct QueryEngine {
    /// Resolves namespace names to configs.
    namespace_resolver: Arc<dyn NamespaceResolver>,
    /// Per-namespace embedding providers.
    embedding_providers: Arc<dyn EmbeddingProviderRegistry>,
    /// Per-namespace vector indexes.
    vector_indexes: Arc<dyn VectorIndexRegistry>,
    /// FTS5 full-text search index.
    fts_index: Arc<dyn FtsIndexRegistry>,
    /// Entity index reader for entity-based recall.
    entity_index: Arc<dyn EntityIndexReader>,
    /// RAM cache for metadata records.
    cache: Arc<dyn RecordCache>,
    /// On-disk metadata storage fallback.
    meta_store: Arc<dyn MetadataStore>,
    /// Graph reader for connection bonus and RIF.
    graph: Arc<dyn GraphReader>,
    /// RIF engine.
    rif_engine: Arc<dyn RifProcessor>,
    /// Access event recorder.
    access_recorder: Arc<dyn AccessRecorder>,
}

impl QueryEngine {
    /// Create a new QueryEngine with all subsystem dependencies.
    pub fn new(
        namespace_resolver: Arc<dyn NamespaceResolver>,
        embedding_providers: Arc<dyn EmbeddingProviderRegistry>,
        vector_indexes: Arc<dyn VectorIndexRegistry>,
        fts_index: Arc<dyn FtsIndexRegistry>,
        entity_index: Arc<dyn EntityIndexReader>,
        cache: Arc<dyn RecordCache>,
        meta_store: Arc<dyn MetadataStore>,
        graph: Arc<dyn GraphReader>,
        rif_engine: Arc<dyn RifProcessor>,
        access_recorder: Arc<dyn AccessRecorder>,
    ) -> Self {
        Self {
            namespace_resolver,
            embedding_providers,
            vector_indexes,
            fts_index,
            entity_index,
            cache,
            meta_store,
            graph,
            rif_engine,
            access_recorder,
        }
    }

    /// Execute the full 9-step search pipeline.
    ///
    /// # Pipeline stages
    ///
    /// 1. **Parse**: Validate and normalize the query.
    /// 2. **Embed**: Generate query embedding via the namespace's provider.
    /// 3. **Vector search**: Flat SIMD scan for top-K candidates.
    /// 4. **Load metadata**: Fetch CachedRecords from cache or meta.db.
    /// 5. **Apply filters**: Tag, phase, strength, ghost filtering.
    /// 6. **Calculate effective R**: FSRS retrievability + connection bonus.
    /// 7. **Apply RIF**: Identify competitors, compute suppressions.
    /// 8. **Rank**: Composite score, sort descending, truncate to limit.
    /// 9. **Return**: Assemble SearchResponse, record access events async.
    #[instrument(skip(self, query), fields(namespace = %query.namespace, mode = ?query.query_mode))]
    pub async fn search(&self, mut query: SearchQuery) -> Result<SearchResponse> {
        let pipeline_start = Instant::now();
        let mut timings = StageTimings::default();

        // -- Stage 1: Parse & Validate -----------------------------------
        let stage_start = Instant::now();
        query.validate()?;
        let ns_config = self.namespace_resolver.resolve(&query.namespace).await?;
        timings.parse_us = stage_start.elapsed().as_micros() as u64;

        // -- Stage 2: Embed ----------------------------------------------
        let stage_start = Instant::now();
        let query_embedding = match query.query_mode {
            QueryMode::MetadataOnly => None,
            _ => {
                let text = query.text.as_ref().ok_or(SearchError::EmptyQuery)?;
                let embedding = self
                    .embedding_providers
                    .embed_query(&query.namespace, text)
                    .await?;
                Some(embedding)
            }
        };
        timings.embed_us = stage_start.elapsed().as_micros() as u64;

        // -- Stage 3: Vector Search + FTS + Score Fusion --------------------
        let stage_start = Instant::now();
        // Over-fetch by 8x (floor 100) to leave room for multi-stage
        // attrition (entity scoring, tag/phase/strength filters, RIF
        // suppression). The floor ensures small limits still produce
        // a meaningful candidate pool.
        let fetch_k = match query.query_mode {
            QueryMode::MetadataOnly => 0,
            _ => (query.limit * 8).max(100).min(SearchQuery::MAX_FETCH_K),
        };
        let scored_candidates: Vec<ScoredResult> = if let Some(ref emb) = query_embedding {
            self.vector_indexes.search(ns_config.id, emb, fetch_k)?
        } else {
            Vec::new()
        };
        timings.vector_search_us = stage_start.elapsed().as_micros() as u64;

        // FTS5 keyword search. Prefer fts_query (keyword-optimized) over text.
        let fts_stage_start = Instant::now();
        let fts_text = query.fts_query.as_ref().or(query.text.as_ref());
        let fts_results: Vec<FtsResult> = match fts_text {
            Some(text) if query.query_mode != QueryMode::MetadataOnly => self
                .fts_index
                .search(ns_config.id, text, fetch_k)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        timings.fts_search_us = fts_stage_start.elapsed().as_micros() as u64;

        // -- Score fusion: raw vector + bounded FTS boost --
        let fusion_stage_start = Instant::now();
        let vector_ids: Vec<MemoryId> = scored_candidates.iter().map(|s| s.memory_id).collect();
        let fts_ids: Vec<MemoryId> = fts_results.iter().map(|s| s.memory_id).collect();

        // Build lookup maps for raw scores.
        let vector_score_map: std::collections::HashMap<MemoryId, f32> = scored_candidates
            .iter()
            .map(|s| (s.memory_id, s.score))
            .collect();
        let fts_score_map: std::collections::HashMap<MemoryId, f32> =
            fts_results.iter().map(|s| (s.memory_id, s.score)).collect();

        timings.score_fusion_us = fusion_stage_start.elapsed().as_micros() as u64;

        let query_entities = query.entities.clone();

        // -- Stage 3d: Entity Index Recall --------------------------------
        let entity_recall_start = Instant::now();
        let entity_recall_cap = query.limit * 2;
        let mut entity_only_ids: Vec<(MemoryId, f32)> = Vec::new(); // (id, entity_recall_score)

        // Union vector and FTS results into the candidate pool.
        let mut all_candidate_ids: Vec<MemoryId> = vector_ids.clone();
        let mut candidate_id_set: std::collections::HashSet<MemoryId> =
            vector_ids.iter().copied().collect();
        for &fts_id in &fts_ids {
            if candidate_id_set.insert(fts_id) {
                all_candidate_ids.push(fts_id);
            }
        }

        // Build set for deduplication of entity recall results.
        let existing_candidate_set: std::collections::HashSet<MemoryId> =
            all_candidate_ids.iter().copied().collect();

        if !query_entities.is_empty() && query.query_mode != QueryMode::MetadataOnly {
            let entity_results = self.entity_index.find_by_entities(
                ns_config.id,
                &query_entities,
                MemoryId::nil(),
                entity_recall_cap + all_candidate_ids.len(), // over-fetch to account for dedup
            )?;

            let query_entity_count = query_entities.len() as f32;

            for result in entity_results {
                // Skip candidates already found by vector or FTS.
                if existing_candidate_set.contains(&result.memory_id) {
                    continue;
                }
                if entity_only_ids.len() >= entity_recall_cap {
                    break;
                }
                let recall_score = result.shared_count as f32 / query_entity_count;
                entity_only_ids.push((result.memory_id, recall_score));
            }
        }
        timings.entity_recall_us = entity_recall_start.elapsed().as_micros() as u64;

        // Add entity-only candidate IDs to the pool.
        for &(eid, _) in &entity_only_ids {
            all_candidate_ids.push(eid);
        }

        // -- Stage 4: Load Metadata --------------------------------------
        let stage_start = Instant::now();
        let records = self.load_records(&all_candidate_ids).await?;

        // Build a lookup for entity recall scores.
        let entity_recall_map: std::collections::HashMap<MemoryId, f32> =
            entity_only_ids.iter().cloned().collect();

        let mut candidates: Vec<Candidate> = Vec::with_capacity(records.len());
        for record in records {
            if let Some(&recall_score) = entity_recall_map.get(&record.id) {
                // Entity-index-only candidate (not found by vector/FTS).
                candidates.push(Candidate {
                    record,
                    relevance_score: None,
                    raw_vector_score: None,
                    raw_fts_score: None,
                    effective_r: 0.0,
                    entity_score: 0.0,
                    entity_recall_score: recall_score,
                    composite_score: 0.0,
                    activation_score: None,
                });
            } else {
                let raw_vector = vector_score_map.get(&record.id).copied();
                let raw_fts = fts_score_map.get(&record.id).copied();

                // Raw vector score + bounded FTS boost.
                // FTS-only candidates get a small fixed relevance.
                let relevance = match (raw_vector, raw_fts) {
                    (Some(vs), Some(fts)) => {
                        // Scaled FTS boost: saturating exponential curve.
                        // Weak matches (~1.0 FTS) -> ~0.01 boost
                        // Medium matches (~3.0 FTS) -> ~0.04 boost
                        // Strong matches (>=5.0 FTS) -> asymptotes to 0.05
                        let fts_boost = FTS_BOOST_CAP * (1.0 - (-fts * FTS_BOOST_RATE).exp());
                        Some(vs + fts_boost)
                    }
                    (Some(vs), None) => Some(vs),
                    (None, Some(fts)) => {
                        // FTS-only: saturating curve bounded below vector-hit range.
                        // Preserves ordering: strong FTS matches rank above weak ones.
                        Some(FTS_ONLY_CAP * (1.0 - (-fts * FTS_ONLY_RATE).exp()))
                    }
                    (None, None) => None,
                };

                candidates.push(Candidate {
                    record,
                    relevance_score: relevance,
                    raw_vector_score: raw_vector,
                    raw_fts_score: raw_fts,
                    effective_r: 0.0,
                    entity_score: 0.0,
                    entity_recall_score: 0.0,
                    composite_score: 0.0,
                    activation_score: None,
                });
            }
        }

        // For MetadataOnly mode, scan metadata store for matching records.
        if query.query_mode == QueryMode::MetadataOnly {
            let metadata_results = self
                .scan_by_metadata(ns_config.id, &query.filter, query.limit * 4)
                .await?;
            for record in metadata_results {
                candidates.push(Candidate {
                    record,
                    relevance_score: None,
                    raw_vector_score: None,
                    raw_fts_score: None,
                    effective_r: 0.0,
                    entity_score: 0.0,
                    entity_recall_score: 0.0,
                    composite_score: 0.0,
                    activation_score: None,
                });
            }
        }
        timings.load_metadata_us = stage_start.elapsed().as_micros() as u64;

        // -- Stage 4b: Spreading Activation Graph Expansion ---------------
        {
            let stage_start = Instant::now();
            if query.graph_depth > 0 && !candidates.is_empty() {
                // Build seed set: only candidates with a relevance_score
                // (from vector/FTS), not entity-recall-only or metadata-only.
                let seeds: Vec<(MemoryId, f32)> = candidates
                    .iter()
                    .filter_map(|c| c.relevance_score.map(|s| (c.record.id, s)))
                    .collect();

                if !seeds.is_empty() {
                    let activated =
                        self.graph
                            .spreading_activation(&seeds, ns_config.id, query.graph_depth);

                    // Collect existing candidate IDs for dedup.
                    let all_candidate_ids: std::collections::HashSet<MemoryId> =
                        candidates.iter().map(|c| c.record.id).collect();

                    // Filter out candidates we already have.
                    let new_ids: Vec<(MemoryId, f32)> = activated
                        .into_iter()
                        .filter(|(mid, _)| !all_candidate_ids.contains(mid))
                        .collect();

                    if !new_ids.is_empty() {
                        let load_ids: Vec<MemoryId> = new_ids.iter().map(|(mid, _)| *mid).collect();
                        let activation_map: std::collections::HashMap<MemoryId, f32> =
                            new_ids.into_iter().collect();
                        let expand_records = self.load_records(&load_ids).await?;

                        for record in expand_records {
                            let activation = activation_map.get(&record.id).copied().unwrap_or(0.0);
                            candidates.push(Candidate {
                                record,
                                relevance_score: None,
                                raw_vector_score: None,
                                raw_fts_score: None,
                                effective_r: 0.0,
                                entity_score: 0.0,
                                entity_recall_score: 0.0,
                                composite_score: 0.0,
                                activation_score: Some(activation),
                            });
                        }
                    }
                }
            }
            timings.graph_expansion_us = stage_start.elapsed().as_micros() as u64;
        }

        // -- Entity overlap scoring -----------------------------------------
        // Compute entity overlap with each candidate's cached entities.
        if !query_entities.is_empty() {
            for candidate in &mut candidates {
                candidate.entity_score =
                    crate::model::entity_overlap(&query_entities, &candidate.record.entities);
            }
        }

        // -- Stage 5: Apply Filters --------------------------------------
        let stage_start = Instant::now();
        candidates.retain(|c| Self::passes_filters(c, &query));
        timings.apply_filters_us = stage_start.elapsed().as_micros() as u64;

        // -- Stage 6: Calculate Effective R --------------------------------
        let stage_start = Instant::now();
        for candidate in &mut candidates {
            candidate.effective_r = Self::compute_effective_r(&candidate.record);
        }
        timings.calculate_r_us = stage_start.elapsed().as_micros() as u64;

        // -- Stage 7: Apply RIF -------------------------------------------
        let stage_start = Instant::now();
        let retrieved_ids: Vec<MemoryId> = candidates.iter().map(|c| c.record.id).collect();

        // Collect all neighbor IDs for RIF computation.
        let mut all_neighbor_ids: Vec<MemoryId> = Vec::new();
        for id in &retrieved_ids {
            let neighbors = self.graph.neighbors(id);
            all_neighbor_ids.extend(neighbors);
        }

        let suppressions = self
            .rif_engine
            .compute_suppressions(&retrieved_ids, &all_neighbor_ids);

        for suppression in &suppressions {
            if let Some(candidate) = candidates
                .iter_mut()
                .find(|c| c.record.id == suppression.target)
            {
                candidate.effective_r *= suppression.suppression_factor;
            }
        }
        timings.apply_rif_us = stage_start.elapsed().as_micros() as u64;

        // -- Stage 8: Composite Score + Sort --------------------------------
        let stage_start = Instant::now();
        let total_matches = candidates.len();
        for candidate in &mut candidates {
            candidate.composite_score = Self::compute_composite_score(candidate);
        }

        // Temporal boost: Gaussian falloff centered on the query's time range.
        // Gives ~1.5x at center, ~1.3x at the edges, decaying to ~1.0 beyond.
        if query.time_range_start.is_some() || query.time_range_end.is_some() {
            let range_start = query.time_range_start.unwrap_or(i64::MIN) as f64;
            let range_end = query.time_range_end.unwrap_or(i64::MAX) as f64;
            let range_mid = (range_start + range_end) / 2.0;
            let sigma = ((range_end - range_start) / 2.0).max(86_400_000.0);
            for candidate in &mut candidates {
                let distance = (candidate.record.created_at as f64 - range_mid).abs();
                let boost = 1.0 + 0.5 * (-0.5 * (distance / sigma).powi(2)).exp();
                candidate.composite_score *= boost as f32;
            }
        }

        // Stable sort preserves insertion order for equal scores.
        candidates.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        timings.rank_us = stage_start.elapsed().as_micros() as u64;

        // -- Stage 8c: Supersedes Resolution ---------------------------------
        // Drop candidates that have been superseded by a newer memory.
        // If the replacement isn't already in the result set, load it and
        // insert it so the user gets the most current version.
        {
            use std::collections::HashSet;

            let candidate_id_set: HashSet<MemoryId> =
                candidates.iter().map(|c| c.record.id).collect();

            let mut to_remove: HashSet<usize> = HashSet::new();
            let mut replacements_to_inject: Vec<(MemoryId, f32)> = Vec::new();

            for (idx, candidate) in candidates.iter().enumerate() {
                if let Some(replacement_id) = self.graph.superseded_by(&candidate.record.id) {
                    to_remove.insert(idx);
                    if !candidate_id_set.contains(&replacement_id) {
                        if let Some(entry) = replacements_to_inject
                            .iter_mut()
                            .find(|(id, _)| *id == replacement_id)
                        {
                            entry.1 = entry.1.max(candidate.composite_score);
                        } else {
                            replacements_to_inject
                                .push((replacement_id, candidate.composite_score));
                        }
                    }
                }
            }

            if !replacements_to_inject.is_empty() {
                let load_ids: Vec<MemoryId> =
                    replacements_to_inject.iter().map(|(id, _)| *id).collect();

                if let Ok(records) = self.load_records(&load_ids).await {
                    let score_map: std::collections::HashMap<MemoryId, f32> =
                        replacements_to_inject.iter().cloned().collect();

                    for record in records {
                        if record.phase == DecayPhase::Ghost && !query.include_ghosts {
                            continue;
                        }

                        let inherited_score = score_map.get(&record.id).copied().unwrap_or(0.0);

                        let effective_r = Self::compute_effective_r(&record);

                        let entity_score = if !query_entities.is_empty() {
                            crate::model::entity_overlap(&query_entities, &record.entities)
                        } else {
                            0.0
                        };

                        let mut replacement = Candidate {
                            record,
                            relevance_score: None,
                            raw_vector_score: None,
                            raw_fts_score: None,
                            effective_r,
                            entity_score,
                            entity_recall_score: 0.0,
                            composite_score: 0.0,
                            activation_score: None,
                        };
                        replacement.composite_score = Self::compute_composite_score(&replacement);

                        replacement.composite_score =
                            replacement.composite_score.max(inherited_score);

                        candidates.push(replacement);
                    }
                }
            }

            if !to_remove.is_empty() {
                let mut keep_idx = 0;
                candidates.retain(|_| {
                    let keep = !to_remove.contains(&keep_idx);
                    keep_idx += 1;
                    keep
                });
            }
        }

        candidates.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // -- Stage 8d: Truncate -------------------------------------------
        candidates.truncate(query.limit);

        // -- Stage 9: Build Response & Record Access ----------------------
        let stage_start = Instant::now();
        let results = Self::build_results(&candidates, &query.namespace);
        timings.build_response_us = stage_start.elapsed().as_micros() as u64;

        timings.total_us = pipeline_start.elapsed().as_micros() as u64;
        let query_time_ms = pipeline_start.elapsed().as_secs_f64() * 1000.0;

        // Record access events asynchronously.
        self.record_accesses_async(&candidates);

        let response = SearchResponse {
            results,
            total_matches,
            query_time_ms,
            namespace: query.namespace.clone(),
            timings: Some(timings),
        };

        debug!(
            total_matches,
            returned = response.results.len(),
            query_time_ms = format!("{:.2}", query_time_ms),
            "search complete"
        );

        Ok(response)
    }

    /// Retrieve a single memory by ID.
    #[instrument(skip(self))]
    pub async fn get_memory(&self, id: MemoryId) -> Result<Option<MemoryResponse>> {
        let record = match self.cache.get(&id) {
            Some(r) => r,
            None => match self.meta_store.get(&id).await? {
                Some(r) => r,
                None => return Ok(None),
            },
        };

        // Record access (fire-and-forget).
        let recorder = Arc::clone(&self.access_recorder);
        let access_id = id;
        tokio::spawn(async move {
            if let Err(e) = recorder
                .record_access(access_id, AccessKind::DirectRetrieval)
                .await
            {
                warn!("failed to record access for {access_id:?}: {e}");
            }
        });

        Ok(Some(MemoryResponse {
            id: record.id,
            namespace: String::new(), // Resolved by API layer from namespace_id
            created_at: record.created_at,
            last_accessed_at: record.last_accessed_at,
            summary: if record.phase != DecayPhase::Ghost {
                Some(record.summary.clone())
            } else {
                None
            },
            full_text: None, // Loaded on demand by API layer
            tags: record.tags.clone(),
            phase: record.phase,
            strength: record.strength,
            decay_strength: record.decay_strength,
            stability: record.stability,
            is_permastore: record.is_permastore,
            edge_count: record.edge_count,
            access_count: 0, // Populated by API layer from access_history
        }))
    }

    /// Find the K most similar memories to a given memory.
    #[instrument(skip(self))]
    pub async fn similar(&self, id: MemoryId, k: usize) -> Result<Vec<SearchResult>> {
        let k = k.clamp(1, SearchQuery::MAX_USER_LIMIT);

        let record = match self.cache.get(&id) {
            Some(r) => r,
            None => self
                .meta_store
                .get(&id)
                .await?
                .ok_or(SearchError::MemoryNotFound(id))?,
        };

        // Over-fetch by 1 to account for self-match removal.
        let scored = self
            .vector_indexes
            .search(record.namespace_id, &[], k + 1)?;

        // Remove the source memory from results.
        let filtered: Vec<ScoredResult> = scored
            .into_iter()
            .filter(|s| s.memory_id != id)
            .take(k)
            .collect();

        let result_ids: Vec<MemoryId> = filtered.iter().map(|s| s.memory_id).collect();
        let records = self.load_records(&result_ids).await?;

        let results: Vec<SearchResult> = records
            .iter()
            .filter_map(|rec| {
                let score = filtered
                    .iter()
                    .find(|s| s.memory_id == rec.id)
                    .map(|s| s.score);
                Some(SearchResult {
                    memory_id: rec.id,
                    created_at: rec.created_at,
                    score,
                    fts_score: None,
                    composite_score: score,
                    retrievability: rec.strength,
                    effective_r: rec.decay_strength,
                    phase: rec.phase,
                    summary: if rec.phase != DecayPhase::Ghost {
                        Some(rec.summary.clone())
                    } else {
                        None
                    },
                    full_text: None,
                    tags: rec.tags.clone(),
                    edge_count: rec.edge_count,
                    activation_score: None,
                    is_permastore: rec.is_permastore,
                    stability: rec.stability,
                    namespace: String::new(),
                })
            })
            .collect();

        // Record associative access for returned results.
        let accesses: Vec<(MemoryId, AccessKind)> = results
            .iter()
            .map(|r| (r.memory_id, AccessKind::AssociativeRetrieval))
            .collect();
        if !accesses.is_empty() {
            let recorder = Arc::clone(&self.access_recorder);
            tokio::spawn(async move {
                if let Err(e) = recorder.record_access_batch(&accesses).await {
                    warn!("failed to record associative access: {e}");
                }
            });
        }

        Ok(results)
    }

    // =====================================================================
    // Internal helpers
    // =====================================================================

    /// Load records for a batch of MemoryIds, checking RAM cache first.
    async fn load_records(&self, ids: &[MemoryId]) -> Result<Vec<CachedRecord>> {
        let mut results = Vec::with_capacity(ids.len());
        let mut cache_misses = Vec::new();

        for id in ids {
            if let Some(record) = self.cache.get(id) {
                results.push(record);
            } else {
                cache_misses.push(*id);
            }
        }

        if !cache_misses.is_empty() {
            let disk_records = self.meta_store.get_batch(&cache_misses).await?;
            results.extend(disk_records);
        }

        Ok(results)
    }

    /// MetadataOnly path: scan meta.db for matching records.
    ///
    /// Loads candidate records from the metadata store and filters them
    /// against the query's tag/phase/strength criteria. Without a
    /// dedicated secondary index this is a brute-force scan; a future
    /// optimisation can push the filter down to storage.
    async fn scan_by_metadata(
        &self,
        namespace_id: NamespaceId,
        filter: &SearchFilter,
        max_results: usize,
    ) -> Result<Vec<CachedRecord>> {
        // Use get_batch with an empty slice to signal a full scan.
        // The MetadataStore adapter can implement a storage-level scan
        // path here; for now we return whatever the store provides.
        let all_records = self.meta_store.get_batch(&[]).await?;

        // Apply client-side filtering for namespace and query filters.
        let filtered: Vec<CachedRecord> = all_records
            .into_iter()
            .filter(|r| r.namespace_id == namespace_id)
            .filter(|r| {
                if !filter.phases.is_empty() && !filter.phases.contains(&r.phase) {
                    return false;
                }
                if let Some(min_strength) = filter.min_strength {
                    if r.strength < min_strength {
                        return false;
                    }
                }
                if filter.permastore_only && !r.is_permastore {
                    return false;
                }
                if !filter.require_tags.is_empty() {
                    for required in &filter.require_tags {
                        if !r.tags.contains(required) {
                            return false;
                        }
                    }
                }
                for excluded in &filter.exclude_tags {
                    if r.tags.contains(excluded) {
                        return false;
                    }
                }
                true
            })
            .take(max_results)
            .collect();

        Ok(filtered)
    }

    /// Check if a candidate passes all query filters.
    fn passes_filters(candidate: &Candidate, query: &SearchQuery) -> bool {
        let record = &candidate.record;

        // Ghost filter
        if record.phase == DecayPhase::Ghost && !query.include_ghosts {
            return false;
        }

        // Minimum score filter (applied to fused relevance score).
        if let Some(score) = candidate.relevance_score {
            if score < query.min_score {
                return false;
            }
        }

        let filter = &query.filter;

        // Phase filter
        if !filter.phases.is_empty() && !filter.phases.contains(&record.phase) {
            return false;
        }

        // Strength filter
        if let Some(min_strength) = filter.min_strength {
            if record.strength < min_strength {
                return false;
            }
        }

        // Permastore filter
        if filter.permastore_only && !record.is_permastore {
            return false;
        }

        // Require tags (ALL must be present)
        if !filter.require_tags.is_empty() {
            for required in &filter.require_tags {
                if !record.tags.contains(required) {
                    return false;
                }
            }
        }

        // Exclude tags (NONE may be present)
        for excluded in &filter.exclude_tags {
            if record.tags.contains(excluded) {
                return false;
            }
        }

        true
    }

    /// Compute effective retrievability for a memory.
    ///
    /// Uses the pre-computed decay_strength which already includes the
    /// connection bonus from spreading activation.
    fn compute_effective_r(record: &CachedRecord) -> f32 {
        record.decay_strength.clamp(0.0, 1.0)
    }

    /// Compute the final composite ranking score.
    ///
    fn compute_composite_score(candidate: &Candidate) -> f32 {
        const FTS_COMPOSITE_THRESHOLD: f32 = 0.5;

        const ACTIVATION_WEIGHT: f32 = 0.70;
        const ACTIVATION_ENTITY_WEIGHT: f32 = 0.30;

        const ENTITY_RECALL_WEIGHT: f32 = 0.40;
        const ENTITY_RECALL_ENTITY_WEIGHT: f32 = 0.20;
        const ENTITY_RECALL_R_WEIGHT: f32 = 0.10;
        const ENTITY_RECALL_FTS_WEIGHT: f32 = 0.10;

        // Normalized FTS keyword signal [0, 1). Used as a component in the
        // composite formula where the branch-specific weight (0.05 or 0.10)
        // controls the maximum contribution. NOT scaled by FTS_BOOST_CAP --
        // that would double-attenuate the signal (see Stage 3 fusion for
        // the cap-scaled variant).
        let fts_signal = match candidate.raw_fts_score {
            Some(fts) if fts > FTS_COMPOSITE_THRESHOLD => 1.0_f32 - (-fts * FTS_BOOST_RATE).exp(),
            _ => 0.0,
        };

        match candidate.relevance_score {
            Some(rel) => 0.90 * rel + 0.05 * candidate.entity_score + 0.05 * fts_signal,
            None if candidate.activation_score.is_some() => {
                let activation = candidate.activation_score.unwrap();
                ACTIVATION_WEIGHT * activation + ACTIVATION_ENTITY_WEIGHT * candidate.entity_score
            }
            None if candidate.entity_recall_score > 0.0 => {
                ENTITY_RECALL_WEIGHT * candidate.entity_recall_score
                    + ENTITY_RECALL_ENTITY_WEIGHT * candidate.entity_score
                    + ENTITY_RECALL_R_WEIGHT * candidate.effective_r
                    + ENTITY_RECALL_FTS_WEIGHT * fts_signal
            }
            None => candidate.effective_r * 0.1,
        }
    }

    /// Assemble SearchResult structs from ranked candidates.
    fn build_results(candidates: &[Candidate], namespace: &str) -> Vec<SearchResult> {
        candidates
            .iter()
            .map(|c| {
                let record = &c.record;
                SearchResult {
                    memory_id: record.id,
                    created_at: record.created_at,
                    score: c.raw_vector_score,
                    fts_score: c.raw_fts_score,
                    composite_score: Some(c.composite_score),
                    retrievability: record.strength,
                    effective_r: c.effective_r,
                    phase: record.phase,
                    summary: if record.phase != DecayPhase::Ghost {
                        Some(record.summary.clone())
                    } else {
                        None
                    },
                    full_text: None,
                    tags: record.tags.clone(),
                    edge_count: record.edge_count,
                    activation_score: c.activation_score,
                    is_permastore: record.is_permastore,
                    stability: record.stability,
                    namespace: namespace.to_string(),
                }
            })
            .collect()
    }

    /// Fire-and-forget access recording.
    fn record_accesses_async(&self, candidates: &[Candidate]) {
        let accesses: Vec<(MemoryId, AccessKind)> = candidates
            .iter()
            .map(|c| {
                let kind = if c.activation_score.is_some() {
                    AccessKind::AssociativeRetrieval
                } else {
                    AccessKind::DirectRetrieval
                };
                (c.record.id, kind)
            })
            .collect();

        if accesses.is_empty() {
            return;
        }

        let recorder = Arc::clone(&self.access_recorder);
        tokio::spawn(async move {
            if let Err(e) = recorder.record_access_batch(&accesses).await {
                warn!("failed to record access events: {e}");
            }
        });
    }
}
