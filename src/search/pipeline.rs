//! QueryEngine — 9-step search pipeline orchestration.
//!
//! Orchestrates embedding, vector search, metadata loading, filtering,
//! FSRS retrievability calculation, RIF suppression, ranking, and
//! response assembly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::time::Instant;
use tracing::{debug, instrument, warn};

use super::error::{Result, SearchError};
use super::query::{QueryMode, SearchFilter, SearchQuery};
use super::response::{MemoryResponse, SearchResponse, SearchResult, StageTimings};
use super::{EligibleFtsResults, FtsCoverage};
use crate::model::{AccessKind, CachedRecord, DecayPhase, MemoryId, NamespaceConfig, NamespaceId};

#[path = "diagnostics.rs"]
mod diagnostics;
#[path = "semantics.rs"]
mod semantics;
pub use diagnostics::{
    ChannelDiagnostics, ChannelParticipation, ResultProvenance, RetrievalChannel,
};
#[cfg(test)]
#[path = "semantics_tests.rs"]
mod semantics_tests;
pub use semantics::{
    FtsAvailability, FtsDiagnostics, RetrievalSemantics, SemanticsDiagnostics, SemanticsResponse,
    UnresolvedReason, VersionMode, VersionStatus,
};

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
    /// Search the namespace's full vector index for the top-K nearest neighbors.
    /// When supplied, canonical eligible IDs must be applied before top-K selection.
    fn search(
        &self,
        namespace_id: NamespaceId,
        query_vec: &[f32],
        k: usize,
        eligible_ids: Option<&HashSet<MemoryId>>,
    ) -> Result<Vec<ScoredResult>>;

    /// Retrieve the raw embedding vector for a given memory ID, if present.
    fn get_vector(&self, id: MemoryId) -> Option<Vec<f32>>;
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

    /// Stable eligible refill. Unsupported adapters fail explicitly in v2;
    /// a legacy top-K call cannot certify exhaustion or refill coverage.
    fn search_eligible(
        &self,
        _namespace_id: NamespaceId,
        _query: &str,
        _k: usize,
        _eligible_ids: &HashSet<MemoryId>,
    ) -> Result<EligibleFtsResults> {
        Err(SearchError::Internal(
            "eligible FTS search unavailable".into(),
        ))
    }
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
    /// Enumerate all namespace members without a candidate/scan cap.
    /// IDs are discovery only; load canonical records before checking eligibility.
    async fn list_namespace_memories(&self, namespace_id: NamespaceId) -> Result<Vec<MemoryId>>;
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
    /// Internal v2 structural read. Unsupported adapters must fail, never prove currentness.
    fn supersession_links(
        &self,
        _id: MemoryId,
        _allowance: usize,
    ) -> Result<crate::graph::query::SupersessionLinks> {
        Err(SearchError::Internal(
            "semantics2 graph reads unavailable".into(),
        ))
    }

    fn expand_eligible(
        &self,
        _seeds: &[(MemoryId, f32)],
        _namespace: NamespaceId,
        _depth: u8,
        _eligible: &HashMap<MemoryId, CachedRecord>,
    ) -> Result<crate::graph::query::GraphExpansion> {
        Err(SearchError::Internal(
            "semantics2 graph expansion unavailable".into(),
        ))
    }
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
    /// `eligible_ids` is canonical membership in `namespace_id`, including all
    /// hard filters, and must be applied before the cap.
    fn find_by_entities(
        &self,
        namespace_id: NamespaceId,
        entities: &[String],
        exclude_id: MemoryId,
        k: usize,
        eligible_ids: &HashSet<MemoryId>,
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
#[derive(Clone)]
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
    /// Internal v2 provenance; never a transport field in this slice.
    graph_depth: Option<u8>,
    /// V2 graph seed evidence, merged independently of discovery order.
    seed_score: f32,
    /// Diagnostic evidence only; never read by ranking or source selection.
    provenance: ResultProvenance,
}

impl Candidate {
    /// Create a new candidate with default scores.
    fn new(record: CachedRecord) -> Self {
        Self {
            record,
            relevance_score: None,
            raw_vector_score: None,
            raw_fts_score: None,
            effective_r: 0.0,
            entity_score: 0.0,
            entity_recall_score: 0.0,
            composite_score: 0.0,
            activation_score: None,
            graph_depth: None,
            seed_score: 0.0,
            provenance: ResultProvenance::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// FTS boost constants (used in both search() and compute_composite_score())
// ---------------------------------------------------------------------------

/// Maximum FTS boost added to vector similarity in the relevance score.
const FTS_BOOST_CAP: f32 = 0.05;

/// Rate parameter controlling how quickly FTS boost saturates.
const FTS_BOOST_RATE: f32 = 0.5;

/// Maximum relevance score for FTS-only candidates (no vector match).
const FTS_ONLY_CAP: f32 = 0.30;

/// Rate parameter for FTS-only saturating curve.
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

    /// Retrieve the raw embedding vector for a memory by its ID.
    ///
    /// Delegates to the underlying `VectorIndexRegistry`. Returns `None`
    /// if no embedding is found (e.g., the memory was tombstoned).
    pub fn get_vector(&self, id: MemoryId) -> Option<Vec<f32>> {
        self.vector_indexes.get_vector(id)
    }

    /// Search a namespace's vector index for the top-K nearest neighbors
    /// to the given query vector.
    ///
    /// Delegates to the underlying `VectorIndexRegistry`.
    pub fn vector_search(
        &self,
        namespace_id: NamespaceId,
        query_vec: &[f32],
        k: usize,
    ) -> Result<Vec<ScoredResult>> {
        self.vector_indexes.search(namespace_id, query_vec, k, None)
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
    pub async fn search(&self, query: SearchQuery) -> Result<SearchResponse> {
        Ok(self
            .search_with_semantics(query, RetrievalSemantics::V1)
            .await?
            .response)
    }

    /// Rust-only harness entry point. No transport selects semantics2 until A1/A4 readiness.
    pub async fn search_with_semantics(
        &self,
        mut query: SearchQuery,
        semantics: RetrievalSemantics,
    ) -> Result<SemanticsResponse> {
        let mut diagnostics = SemanticsDiagnostics::new(semantics);
        let pipeline_start = Instant::now();
        let mut timings = StageTimings::default();

        // -- Stage 1: Parse & Validate -----------------------------------
        let stage_start = Instant::now();
        if semantics != RetrievalSemantics::V1 {
            semantics::validate_query(&query)?;
        }
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
        if semantics != RetrievalSemantics::V1
            && query_embedding
                .as_ref()
                .is_some_and(|v| v.iter().any(|x| !x.is_finite()))
        {
            return Err(SearchError::EmbeddingFailed(
                "nonfinite query embedding".into(),
            ));
        }

        // Canonical metadata, not cached/index metadata, decides hard eligibility.
        // ponytail: load every namespace member until A1 supplies certified filtered views.
        let stage_start = Instant::now();
        let namespace_ids = self
            .meta_store
            .list_namespace_memories(ns_config.id)
            .await?;
        let eligible_records: HashMap<MemoryId, CachedRecord> = self
            .meta_store
            .get_batch(&namespace_ids)
            .await?
            .into_iter()
            .filter(|record| Self::passes_hard_filters(record, &query, ns_config.id))
            .map(|record| (record.id, record))
            .collect();
        let eligible_ids: HashSet<MemoryId> = eligible_records.keys().copied().collect();
        timings.eligibility_us = stage_start.elapsed().as_micros() as u64;
        if semantics != RetrievalSemantics::V1 {
            for record in eligible_records.values() {
                semantics::validate_record(record)?;
            }
        }

        // -- Stage 3: Vector Search + FTS + Entity Recall -----------------
        let fetch_k = match query.query_mode {
            QueryMode::MetadataOnly => 0,
            _ => (query.limit * 8).max(100).min(SearchQuery::MAX_FETCH_K),
        };

        let (scored_candidates, vector_search_us) =
            self.run_vector_search(&query_embedding, ns_config.id, fetch_k, &eligible_ids)?;
        timings.vector_search_us = vector_search_us;

        let (fts_results, fts_search_us) = self.run_fts_search(
            &query,
            ns_config.id,
            fetch_k,
            &eligible_ids,
            &mut diagnostics,
        )?;
        timings.fts_search_us = fts_search_us;
        if semantics != RetrievalSemantics::V1
            && (scored_candidates.iter().any(|r| !r.score.is_finite())
                || fts_results.iter().any(|r| !r.score.is_finite()))
        {
            return Err(SearchError::Internal(
                "nonfinite retrieval source score".into(),
            ));
        }

        // -- Score fusion: build lookup maps --
        let fusion_stage_start = Instant::now();
        let vector_ids: Vec<MemoryId> = scored_candidates.iter().map(|s| s.memory_id).collect();
        let fts_ids: Vec<MemoryId> = fts_results.iter().map(|s| s.memory_id).collect();

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
        let mut entity_only_ids: Vec<(MemoryId, f32)> = Vec::new();
        let mut entity_hit_ids = HashSet::new();
        let mut entity_offered = None;

        // Union vector and FTS results into the candidate pool.
        let mut all_candidate_ids: Vec<MemoryId> = vector_ids.clone();
        let mut candidate_id_set: std::collections::HashSet<MemoryId> =
            vector_ids.iter().copied().collect();
        for &fts_id in &fts_ids {
            if candidate_id_set.insert(fts_id) {
                all_candidate_ids.push(fts_id);
            }
        }

        if !query_entities.is_empty() && query.query_mode != QueryMode::MetadataOnly {
            let entity_results = self.entity_index.find_by_entities(
                ns_config.id,
                &query_entities,
                MemoryId::nil(),
                entity_recall_cap + all_candidate_ids.len(),
                &eligible_ids,
            )?;

            if diagnostics.channels.is_some() {
                // Observe overlap before the ranking path skips existing IDs
                // and caps novel entity candidates. This never changes that path.
                entity_hit_ids.extend(entity_results.iter().map(|r| r.memory_id));
                entity_offered = Some(entity_hit_ids.len());
            }
            let query_entity_count = query_entities.len() as f32;

            for result in entity_results {
                if candidate_id_set.contains(&result.memory_id) {
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

        for &(eid, _) in &entity_only_ids {
            all_candidate_ids.push(eid);
        }

        // -- Stage 4: Load Metadata & Build Candidates --------------------
        let stage_start = Instant::now();
        let records = all_candidate_ids
            .iter()
            .filter_map(|id| eligible_records.get(id).cloned());

        let entity_recall_map: std::collections::HashMap<MemoryId, f32> =
            entity_only_ids.iter().cloned().collect();

        let mut candidates: Vec<Candidate> = Vec::with_capacity(all_candidate_ids.len());
        for record in records {
            if let Some(&recall_score) = entity_recall_map.get(&record.id) {
                let mut c = Candidate::new(record);
                c.entity_recall_score = recall_score;
                candidates.push(c);
            } else {
                let raw_vector = vector_score_map.get(&record.id).copied();
                let raw_fts = fts_score_map.get(&record.id).copied();
                let relevance = Self::fuse_scores(raw_vector, raw_fts);
                if semantics != RetrievalSemantics::V1
                    && relevance.is_some_and(|score| !score.is_finite())
                {
                    return Err(SearchError::Internal("nonfinite fused relevance".into()));
                }

                let mut c = Candidate::new(record);
                c.relevance_score = relevance;
                c.raw_vector_score = raw_vector;
                c.raw_fts_score = raw_fts;
                candidates.push(c);
            }
        }

        if query.query_mode == QueryMode::MetadataOnly {
            // Scan through completion; only ranked results may consume the limit.
            for record in eligible_records.values() {
                candidates.push(Candidate::new(record.clone()));
            }
        }
        if diagnostics.channels.is_some() {
            for candidate in &mut candidates {
                let channels = &mut candidate.provenance.own_channels;
                if candidate.raw_vector_score.is_some() {
                    channels.insert(RetrievalChannel::Vector);
                }
                if candidate.raw_fts_score.is_some() {
                    channels.insert(RetrievalChannel::Fts);
                }
                if entity_hit_ids.contains(&candidate.record.id) {
                    channels.insert(RetrievalChannel::Entity);
                }
                if query.query_mode == QueryMode::MetadataOnly {
                    channels.insert(RetrievalChannel::Metadata);
                }
            }
            diagnostics.observe_direct_channels(
                &candidates,
                query_embedding.as_ref().map(|_| vector_score_map.len()),
                entity_offered,
                (query.query_mode == QueryMode::MetadataOnly).then_some(eligible_records.len()),
            );
        }
        timings.load_metadata_us = stage_start.elapsed().as_micros() as u64;

        let total_matches;
        if matches!(semantics, RetrievalSemantics::V2(_)) {
            self.finish_semantics2(
                &mut candidates,
                &query,
                ns_config.id,
                &eligible_records,
                &mut diagnostics,
                &mut timings,
            )
            .await?;
            total_matches = candidates.len();
        } else {
            // -- Stage 4b: Graph Expansion ------------------------------------
            {
                let stage_start = Instant::now();
                self.expand_graph_candidates(&mut candidates, ns_config.id, query.graph_depth)
                    .await?;
                timings.graph_expansion_us = stage_start.elapsed().as_micros() as u64;
            }

            // -- Stage 5: Apply Filters (entity overlap + metadata filters) ---
            let stage_start = Instant::now();
            self.apply_filters(&mut candidates, &query, &query_entities, ns_config.id);
            timings.apply_filters_us = stage_start.elapsed().as_micros() as u64;

            // -- Stage 6: Calculate Effective R --------------------------------
            let stage_start = Instant::now();
            for candidate in &mut candidates {
                candidate.effective_r = Self::compute_effective_r(&candidate.record);
            }
            timings.calculate_r_us = stage_start.elapsed().as_micros() as u64;

            // -- Stage 7: Apply RIF -------------------------------------------
            let stage_start = Instant::now();
            self.apply_rif(&mut candidates);
            timings.apply_rif_us = stage_start.elapsed().as_micros() as u64;

            // -- Stage 8: Composite Score + Boosts + Sort ----------------------
            let stage_start = Instant::now();
            total_matches = candidates.len();
            self.apply_boosts(&mut candidates, &query);

            // UUID ties make scan/cache insertion order irrelevant.
            candidates.sort_by(|a, b| {
                b.composite_score
                    .partial_cmp(&a.composite_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.record.id.cmp(&b.record.id))
            });
            timings.rank_us = stage_start.elapsed().as_micros() as u64;

            // -- Stage 8c: Supersedes Resolution ------------------------------
            self.resolve_supersedes(&mut candidates, &query, &query_entities, ns_config.id)
                .await;

            candidates.sort_by(|a, b| {
                b.composite_score
                    .partial_cmp(&a.composite_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.record.id.cmp(&b.record.id))
            });
        }

        // -- Stage 8d: Truncate -------------------------------------------
        diagnostics.result_limit_applied = candidates.len() > query.limit;
        candidates.truncate(query.limit);
        diagnostics.retain_returned(&candidates);

        // -- Stage 9: Build Response & Record Access ----------------------
        let stage_start = Instant::now();
        let results = Self::build_results(&candidates, &query.namespace);
        timings.build_response_us = stage_start.elapsed().as_micros() as u64;

        timings.total_us = pipeline_start.elapsed().as_micros() as u64;
        let query_time_ms = pipeline_start.elapsed().as_secs_f64() * 1000.0;

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

        Ok(SemanticsResponse {
            response,
            diagnostics,
        })
    }

    /// Stage 3a: Run vector similarity search.
    ///
    /// Returns scored results and elapsed time in microseconds.
    fn run_vector_search(
        &self,
        query_embedding: &Option<Vec<f32>>,
        namespace_id: NamespaceId,
        fetch_k: usize,
        eligible_ids: &HashSet<MemoryId>,
    ) -> Result<(Vec<ScoredResult>, u64)> {
        let stage_start = Instant::now();
        let scored = if let Some(emb) = query_embedding {
            self.vector_indexes
                .search(namespace_id, emb, fetch_k, Some(eligible_ids))?
        } else {
            Vec::new()
        };
        Ok((scored, stage_start.elapsed().as_micros() as u64))
    }

    /// Stage 3b: Run FTS5 full-text keyword search.
    ///
    /// Returns FTS results and elapsed time in microseconds.
    fn run_fts_search(
        &self,
        query: &SearchQuery,
        namespace_id: NamespaceId,
        fetch_k: usize,
        eligible_ids: &HashSet<MemoryId>,
        diagnostics: &mut SemanticsDiagnostics,
    ) -> Result<(Vec<FtsResult>, u64)> {
        let stage_start = Instant::now();
        let fts_text = query.fts_query.as_ref().or(query.text.as_ref());
        let results = match fts_text {
            Some(text) if query.query_mode != QueryMode::MetadataOnly => {
                if diagnostics.semantics == RetrievalSemantics::V1 {
                    match self.fts_index.search(namespace_id, text, fetch_k) {
                        Ok(results) => {
                            diagnostics.fts.availability = FtsAvailability::Available;
                            // Legacy top-K has no exhaustion/examination evidence.
                            results
                        }
                        Err(error) => {
                            diagnostics.fts.availability = FtsAvailability::Unavailable;
                            warn!(%error, "legacy FTS fallback: channel unavailable");
                            Vec::new()
                        }
                    }
                } else {
                    // Fail the query on FTS errors. Degraded partial serving is
                    // optional and requires A1 readiness for independent views.
                    let results = self.fts_index.search_eligible(
                        namespace_id,
                        text,
                        fetch_k,
                        eligible_ids,
                    )?;
                    diagnostics.fts.availability = FtsAvailability::Available;
                    diagnostics.fts.coverage = Some(results.coverage);
                    results
                        .matches
                        .into_iter()
                        .map(|(memory_id, score)| FtsResult { memory_id, score })
                        .collect()
                }
            }
            _ => Vec::new(),
        };
        Ok((results, stage_start.elapsed().as_micros() as u64))
    }

    /// Fuse raw vector and FTS scores into a single relevance score.
    fn fuse_scores(raw_vector: Option<f32>, raw_fts: Option<f32>) -> Option<f32> {
        match (raw_vector, raw_fts) {
            (Some(vs), Some(fts)) => {
                let fts_boost = FTS_BOOST_CAP * (1.0 - (-fts * FTS_BOOST_RATE).exp());
                Some(vs + fts_boost)
            }
            (Some(vs), None) => Some(vs),
            (None, Some(fts)) => Some(FTS_ONLY_CAP * (1.0 - (-fts * FTS_ONLY_RATE).exp())),
            (None, None) => None,
        }
    }

    /// Stage 4b: Expand candidates via spreading activation graph traversal.
    async fn expand_graph_candidates(
        &self,
        candidates: &mut Vec<Candidate>,
        namespace_id: NamespaceId,
        graph_depth: u8,
    ) -> Result<()> {
        if graph_depth == 0 || candidates.is_empty() {
            return Ok(());
        }

        let seeds: Vec<(MemoryId, f32)> = candidates
            .iter()
            .filter_map(|c| c.relevance_score.map(|s| (c.record.id, s)))
            .collect();

        if seeds.is_empty() {
            return Ok(());
        }

        let activated = self
            .graph
            .spreading_activation(&seeds, namespace_id, graph_depth);

        let existing_ids: std::collections::HashSet<MemoryId> =
            candidates.iter().map(|c| c.record.id).collect();

        let new_ids: Vec<(MemoryId, f32)> = activated
            .into_iter()
            .filter(|(mid, _)| !existing_ids.contains(mid))
            .collect();

        if !new_ids.is_empty() {
            let load_ids: Vec<MemoryId> = new_ids.iter().map(|(mid, _)| *mid).collect();
            let activation_map: std::collections::HashMap<MemoryId, f32> =
                new_ids.into_iter().collect();
            let expand_records = self.load_records(&load_ids).await?;

            for record in expand_records {
                let activation = activation_map.get(&record.id).copied().unwrap_or(0.0);
                let mut c = Candidate::new(record);
                c.activation_score = Some(activation);
                candidates.push(c);
            }
        }

        Ok(())
    }

    /// Stage 5: Apply entity overlap scoring and metadata filters.
    fn apply_filters(
        &self,
        candidates: &mut Vec<Candidate>,
        query: &SearchQuery,
        query_entities: &[String],
        namespace_id: NamespaceId,
    ) {
        // Entity overlap scoring.
        if !query_entities.is_empty() {
            for candidate in candidates.iter_mut() {
                candidate.entity_score =
                    crate::model::entity_overlap(query_entities, &candidate.record.entities);
            }
        }

        // Metadata filters (namespace, ghost, min_score, tags, phase, strength).
        candidates.retain(|c| Self::passes_filters(c, query, namespace_id));
    }

    /// Stage 7: Apply retrieval-induced forgetting suppressions.
    fn apply_rif(&self, candidates: &mut Vec<Candidate>) {
        let retrieved_ids: Vec<MemoryId> = candidates.iter().map(|c| c.record.id).collect();

        let mut all_neighbor_ids: Vec<MemoryId> = Vec::new();
        for id in &retrieved_ids {
            let neighbors = self.graph.neighbors(id);
            all_neighbor_ids.extend(neighbors);
        }

        let suppressions = self
            .rif_engine
            .compute_suppressions(&retrieved_ids, &all_neighbor_ids);

        // Build index for O(1) candidate lookup during suppression.
        let candidate_index: std::collections::HashMap<MemoryId, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| (c.record.id, i))
            .collect();

        for suppression in &suppressions {
            if let Some(&idx) = candidate_index.get(&suppression.target) {
                candidates[idx].effective_r *= suppression.suppression_factor;
            }
        }
    }

    /// Stage 8: Compute composite scores and apply temporal boost.
    fn apply_boosts(&self, candidates: &mut Vec<Candidate>, query: &SearchQuery) {
        for candidate in candidates.iter_mut() {
            candidate.composite_score = Self::compute_composite_score(candidate);
        }

        // Temporal boost: Gaussian falloff centered on the query's time range.
        // Requires both bounds; one-sided ranges produce a degenerate uniform boost.
        if let (Some(start), Some(end)) = (query.time_range_start, query.time_range_end) {
            let range_start = start as f64;
            let range_end = end as f64;
            let range_mid = (range_start + range_end) / 2.0;
            let sigma = ((range_end - range_start) / 2.0).max(86_400_000.0);
            for candidate in candidates.iter_mut() {
                let distance = (candidate.record.created_at as f64 - range_mid).abs();
                let boost = 1.0 + 0.5 * (-0.5 * (distance / sigma).powi(2)).exp();
                candidate.composite_score *= boost as f32;
            }
        }
    }

    /// Stage 8c: Replace superseded memories with the versions that
    /// replace them.
    ///
    /// Runs as PLAN -> VALIDATE -> COMMIT, and the ordering is the point:
    /// a superseded memory is dropped **only** once its replacement has been
    /// loaded and cleared every filter the query applies. Removing it any
    /// earlier can leave the caller with neither memory — the correction
    /// invisible and the original silently deleted from the results.
    ///
    /// Replacements are gated by the same [`Self::passes_hard_filters`] that
    /// stage 5 applies to every other candidate. That is what keeps a
    /// replacement in another namespace out of these results: the graph
    /// may contain legacy foreign edges, so without this gate a cross-namespace
    /// `Supersedes` edge would pull a foreign memory into the response.
    ///
    /// `min_score` does not bind inherited scores: the predecessor already
    /// passed score admission. A terminal without own admitted evidence
    /// receives only that inherited score, including the predecessor's boost.
    async fn resolve_supersedes(
        &self,
        candidates: &mut Vec<Candidate>,
        query: &SearchQuery,
        query_entities: &[String],
        namespace_id: NamespaceId,
    ) {
        use std::collections::{HashMap, HashSet};

        // -- Plan: which candidates are superseded, and by what. Nothing is
        // removed here; that decision waits until the replacement is known
        // to be usable.
        let planned: Vec<(usize, MemoryId)> = candidates
            .iter()
            .enumerate()
            .filter_map(|(idx, candidate)| {
                self.graph
                    .superseded_by(&candidate.record.id)
                    .map(|replacement_id| (idx, replacement_id))
            })
            .collect();

        if planned.is_empty() {
            return;
        }

        // Several superseded candidates can share one replacement; it
        // inherits the best of their scores so it ranks at least as high as
        // the memory it stands in for.
        let mut inherited_scores: HashMap<MemoryId, f32> = HashMap::new();
        for (idx, replacement_id) in &planned {
            let score = candidates[*idx].composite_score;
            inherited_scores
                .entry(*replacement_id)
                .and_modify(|best| *best = best.max(score))
                .or_insert(score);
        }

        let candidate_indices: HashMap<MemoryId, usize> = candidates
            .iter()
            .enumerate()
            .map(|(idx, c)| (c.record.id, idx))
            .collect();
        let mut eligible = HashSet::new();
        // Revalidate existing and injected replacements against canonical metadata.
        let to_load: Vec<MemoryId> = inherited_scores.keys().copied().collect();

        // -- Validate: build each replacement, then subject it to the
        // query's filters before letting it displace anything.
        let mut injected: Vec<Candidate> = Vec::with_capacity(to_load.len());
        if !to_load.is_empty() {
            let records = match self.meta_store.get_batch(&to_load).await {
                Ok(records) => records,
                Err(e) => {
                    // Without the replacements there is nothing to put in
                    // the originals' place, so the originals stay.
                    warn!(
                        error = %e,
                        count = to_load.len(),
                        "failed to load supersedes replacements; keeping the superseded memories in the results"
                    );
                    return;
                }
            };

            for record in records {
                if !Self::passes_hard_filters(&record, query, namespace_id) {
                    continue;
                }
                let inherited_score = inherited_scores.get(&record.id).copied().unwrap_or(0.0);
                eligible.insert(record.id);
                if let Some(&idx) = candidate_indices.get(&record.id) {
                    let replacement = &mut candidates[idx];
                    replacement.record = record;
                    replacement.composite_score = replacement.composite_score.max(inherited_score);
                    continue;
                }
                let effective_r = Self::compute_effective_r(&record);
                let entity_score = if query_entities.is_empty() {
                    0.0
                } else {
                    crate::model::entity_overlap(query_entities, &record.entities)
                };

                let mut replacement = Candidate::new(record);
                replacement.effective_r = effective_r;
                replacement.entity_score = entity_score;
                // No own retrieval evidence: inherit the already-boosted score once.
                replacement.composite_score = inherited_score;
                injected.push(replacement);
            }
        }

        // -- Commit: drop only the candidates whose replacement made it.
        let to_remove: HashSet<usize> = planned
            .iter()
            .filter(|(_, replacement_id)| eligible.contains(replacement_id))
            .map(|(idx, _)| *idx)
            .collect();

        *candidates = std::mem::take(candidates)
            .into_iter()
            .enumerate()
            .filter(|(idx, candidate)| {
                !to_remove.contains(idx)
                    && (!inherited_scores.contains_key(&candidate.record.id)
                        || eligible.contains(&candidate.record.id))
            })
            .map(|(_, candidate)| candidate)
            .collect();

        candidates.extend(injected);
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
            summary: if record.phase != DecayPhase::Ghost && record.phase != DecayPhase::Tombstone {
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

        // Load the source memory's embedding from the vector index.
        let source_embedding = self.vector_indexes.get_vector(id).ok_or_else(|| {
            SearchError::Internal(format!("no embedding found for memory {id:?}"))
        })?;

        // Over-fetch by 1 to account for self-match removal.
        let scored =
            self.vector_indexes
                .search(record.namespace_id, &source_embedding, k + 1, None)?;

        // Remove the source memory from results.
        let filtered: Vec<ScoredResult> = scored
            .into_iter()
            .filter(|s| s.memory_id != id)
            .take(k)
            .collect();

        let result_ids: Vec<MemoryId> = filtered.iter().map(|s| s.memory_id).collect();
        let records = self.load_records(&result_ids).await?;

        // Build a lookup map so we can iterate `filtered` (which preserves
        // descending similarity order from the vector search) instead of
        // `records` (whose order is scrambled by cache-hit / disk-load batching).
        let record_map: std::collections::HashMap<MemoryId, &CachedRecord> =
            records.iter().map(|r| (r.id, r)).collect();

        let results: Vec<SearchResult> = filtered
            .iter()
            .filter_map(|scored| {
                let rec = record_map.get(&scored.memory_id)?;
                let score = Some(scored.score);
                Some(SearchResult {
                    memory_id: rec.id,
                    created_at: rec.created_at,
                    last_accessed_at: rec.last_accessed_at,
                    score,
                    fts_score: None,
                    composite_score: score,
                    retrievability: rec.strength,
                    effective_r: rec.decay_strength,
                    phase: rec.phase,
                    summary: if rec.phase != DecayPhase::Ghost && rec.phase != DecayPhase::Tombstone
                    {
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

    /// Check if a record passes the metadata filter criteria.
    ///
    /// Shared filtering logic used by both `passes_filters()` (post-retrieval)
    /// and canonical eligibility scanning (all query modes).
    fn record_passes_filter(record: &CachedRecord, filter: &SearchFilter) -> bool {
        // Tombstone filter: always exclude tombstoned memories.
        if record.phase == DecayPhase::Tombstone {
            return false;
        }

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

    /// Hard eligibility is independent of score admission and retrieval channel.
    fn passes_hard_filters(
        record: &CachedRecord,
        query: &SearchQuery,
        namespace_id: NamespaceId,
    ) -> bool {
        // Namespace filter: discard candidates from other namespaces.
        // Entity recall and graph expansion can inject cross-namespace
        // candidates; this gate ensures they never reach the results.
        if record.namespace_id != namespace_id {
            return false;
        }

        // Tombstone filter: always exclude tombstoned memories from
        // search results. Their content has been stripped; they only
        // exist as graph relay nodes for spreading activation.
        if record.phase == DecayPhase::Tombstone {
            return false;
        }

        // Ghost filter
        if record.phase == DecayPhase::Ghost && !query.include_ghosts {
            return false;
        }

        Self::record_passes_filter(record, &query.filter)
    }

    /// Check hard eligibility, then score admission on fused direct relevance.
    fn passes_filters(
        candidate: &Candidate,
        query: &SearchQuery,
        namespace_id: NamespaceId,
    ) -> bool {
        if let Some(score) = candidate.relevance_score {
            if score < query.min_score {
                return false;
            }
        }

        Self::passes_hard_filters(&candidate.record, query, namespace_id)
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
                    last_accessed_at: record.last_accessed_at,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::model::Tag;

    // -- Stubs ------------------------------------------------------------
    //
    // `resolve_supersedes` touches exactly three of the ten injected
    // subsystems. The rest exist so a `QueryEngine` can be constructed and
    // are wired to panic: if one ever starts being called, the test should
    // say so rather than quietly return an empty answer.

    /// Supersedes chains, pre-resolved to their terminal memory.
    #[derive(Default)]
    struct StubGraph {
        terminal: HashMap<MemoryId, MemoryId>,
    }

    impl GraphReader for StubGraph {
        fn neighbors(&self, _id: &MemoryId) -> Vec<MemoryId> {
            Vec::new()
        }

        fn superseded_by(&self, id: &MemoryId) -> Option<MemoryId> {
            self.terminal.get(id).copied()
        }
    }

    /// Always misses, so every load goes through the metadata store.
    struct StubCache;

    impl RecordCache for StubCache {
        fn get(&self, _id: &MemoryId) -> Option<CachedRecord> {
            None
        }
    }

    /// Backing store for replacement records, with a switch for making the
    /// batch load fail.
    #[derive(Default)]
    struct StubMetaStore {
        records: HashMap<MemoryId, CachedRecord>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl MetadataStore for StubMetaStore {
        async fn list_namespace_memories(
            &self,
            namespace_id: NamespaceId,
        ) -> Result<Vec<MemoryId>> {
            if self.fail {
                return Err(SearchError::MetadataError("meta.db unavailable".into()));
            }
            Ok(self
                .records
                .values()
                .filter(|r| r.namespace_id == namespace_id)
                .map(|r| r.id)
                .collect())
        }
        async fn get(&self, id: &MemoryId) -> Result<Option<CachedRecord>> {
            Ok(self.records.get(id).cloned())
        }

        async fn get_batch(&self, ids: &[MemoryId]) -> Result<Vec<CachedRecord>> {
            if self.fail {
                return Err(SearchError::MetadataError("meta.db unavailable".into()));
            }
            // Missing IDs are skipped, matching the real adapter.
            Ok(ids
                .iter()
                .filter_map(|id| self.records.get(id).cloned())
                .collect())
        }
    }

    struct Unused;

    #[async_trait::async_trait]
    impl NamespaceResolver for Unused {
        async fn resolve(&self, _name: &str) -> Result<NamespaceConfig> {
            unreachable!("stage 8c does not resolve namespaces")
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingProviderRegistry for Unused {
        async fn embed(&self, _namespace: &str, _text: &str) -> Result<Vec<f32>> {
            unreachable!("stage 8c does not embed")
        }
    }

    impl VectorIndexRegistry for Unused {
        fn search(
            &self,
            _ns: NamespaceId,
            _q: &[f32],
            _k: usize,
            _eligible: Option<&HashSet<MemoryId>>,
        ) -> Result<Vec<ScoredResult>> {
            unreachable!("stage 8c does not search vectors")
        }

        fn get_vector(&self, _id: MemoryId) -> Option<Vec<f32>> {
            unreachable!("stage 8c does not read vectors")
        }
    }

    impl FtsIndexRegistry for Unused {
        fn search(&self, _ns: NamespaceId, _q: &str, _k: usize) -> Result<Vec<FtsResult>> {
            unreachable!("stage 8c does not search FTS")
        }
    }

    impl EntityIndexReader for Unused {
        fn find_by_entities(
            &self,
            _ns: NamespaceId,
            _entities: &[String],
            _exclude: MemoryId,
            _k: usize,
            _eligible: &HashSet<MemoryId>,
        ) -> Result<Vec<EntityRecallResult>> {
            unreachable!("stage 8c does not use entity recall")
        }
    }

    impl RifProcessor for Unused {
        fn compute_suppressions(
            &self,
            _retrieved: &[MemoryId],
            _neighbors: &[MemoryId],
        ) -> Vec<RifSuppression> {
            unreachable!("stage 8c does not apply RIF")
        }
    }

    #[async_trait::async_trait]
    impl AccessRecorder for Unused {
        async fn record_access(&self, _id: MemoryId, _kind: AccessKind) -> Result<()> {
            unreachable!("stage 8c does not record access")
        }

        async fn record_access_batch(&self, _a: &[(MemoryId, AccessKind)]) -> Result<()> {
            unreachable!("stage 8c does not record access")
        }
    }

    // -- Fixtures ---------------------------------------------------------

    pub(super) fn ns() -> NamespaceId {
        NamespaceId::new(1)
    }

    pub(super) fn other_ns() -> NamespaceId {
        NamespaceId::new(2)
    }

    /// A Full-phase, strong, untagged record in namespace 1.
    pub(super) fn rec(id: MemoryId) -> CachedRecord {
        CachedRecord {
            id,
            namespace_id: ns(),
            created_at: 1_700_000_000_000,
            last_accessed_at: 1_700_000_000_000,
            phase: DecayPhase::Full,
            strength: 0.9,
            decay_strength: 0.9,
            stability: 10.0,
            difficulty: 5.0,
            is_permastore: false,
            summary: "a memory".to_string(),
            tags: Vec::new(),
            edge_count: 0,
            vector_slot: 0,
            entities: Vec::new(),
        }
    }

    /// A candidate as stage 8 would leave it: scored and ranked.
    fn candidate(record: CachedRecord, score: f32) -> Candidate {
        let mut c = Candidate::new(record);
        c.relevance_score = Some(score);
        c.raw_vector_score = Some(score);
        c.composite_score = score;
        c
    }

    pub(super) fn query(value: serde_json::Value) -> SearchQuery {
        serde_json::from_value(value).expect("valid query")
    }

    fn engine(graph: StubGraph, meta: StubMetaStore) -> QueryEngine {
        QueryEngine::new(
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(StubCache),
            Arc::new(meta),
            Arc::new(graph),
            Arc::new(Unused),
            Arc::new(Unused),
        )
    }

    /// Wire up "B is superseded by A", where A's record is `replacement`.
    fn superseded_setup(old: MemoryId, replacement: CachedRecord) -> (StubGraph, StubMetaStore) {
        let mut graph = StubGraph::default();
        graph.terminal.insert(old, replacement.id);

        let mut meta = StubMetaStore::default();
        meta.records.insert(replacement.id, replacement);

        (graph, meta)
    }

    fn ids(candidates: &[Candidate]) -> Vec<MemoryId> {
        candidates.iter().map(|c| c.record.id).collect()
    }

    // -- Tests ------------------------------------------------------------

    /// The headline behaviour: recall hands back the correction instead of
    /// the memory it corrects, ranked no lower than what it replaced.
    #[tokio::test]
    async fn substitutes_an_eligible_replacement_for_the_superseded_memory() {
        let (old, new) = (MemoryId::new(), MemoryId::new());
        let (graph, meta) = superseded_setup(old, rec(new));
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old), 0.8)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![new]);
        assert!(candidates[0].composite_score >= 0.8);
    }

    /// When the replacement is already a hit in its own right, the old
    /// memory still goes, and the new one is not duplicated.
    #[tokio::test]
    async fn replacement_already_among_the_candidates_appears_exactly_once() {
        let (old, new) = (MemoryId::new(), MemoryId::new());
        let (graph, meta) = superseded_setup(old, rec(new));
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old), 0.8), candidate(rec(new), 0.9)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![new]);
    }

    /// A tombstoned replacement cannot be returned — and because it cannot,
    /// the memory it replaces must survive. Dropping both would lose the
    /// information entirely.
    #[tokio::test]
    async fn tombstoned_replacement_does_not_drop_the_original() {
        let (old, new) = (MemoryId::new(), MemoryId::new());
        let mut replacement = rec(new);
        replacement.phase = DecayPhase::Tombstone;
        let (graph, meta) = superseded_setup(old, replacement);
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old), 0.8)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![old]);
    }

    /// Same reasoning for a ghost the query did not ask for.
    #[tokio::test]
    async fn ghost_replacement_is_kept_out_unless_ghosts_were_requested() {
        let (old, new) = (MemoryId::new(), MemoryId::new());
        let mut replacement = rec(new);
        replacement.phase = DecayPhase::Ghost;
        let (graph, meta) = superseded_setup(old, replacement);
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old), 0.8)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![old]);
    }

    /// ...and when they were requested, the ghost does substitute.
    #[tokio::test]
    async fn ghost_replacement_substitutes_when_ghosts_are_included() {
        let (old, new) = (MemoryId::new(), MemoryId::new());
        let mut replacement = rec(new);
        replacement.phase = DecayPhase::Ghost;
        let (graph, meta) = superseded_setup(old, replacement);
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old), 0.8)];
        engine
            .resolve_supersedes(
                &mut candidates,
                &query(json!({"text": "q", "includeGhosts": true})),
                &[],
                ns(),
            )
            .await;

        assert_eq!(ids(&candidates), vec![new]);
    }

    /// Namespace isolation regression. A `Supersedes` edge can point across
    /// namespaces (older data, or a hand-written edges.db), and the graph
    /// stores no namespace to catch it. Before replacements were run through
    /// `passes_filters`, this stage injected the foreign memory straight
    /// into the response — a cross-namespace data leak.
    #[tokio::test]
    async fn replacement_from_another_namespace_is_not_leaked() {
        let (old, new) = (MemoryId::new(), MemoryId::new());
        let mut replacement = rec(new);
        replacement.namespace_id = other_ns();
        let (graph, meta) = superseded_setup(old, replacement);
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old), 0.8)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![old]);
    }

    /// The rest of the filter set binds replacements too: required tags,
    /// minimum strength, and the phase whitelist.
    #[tokio::test]
    async fn replacement_failing_metadata_filters_is_not_substituted() {
        /// (label, query, how to make the replacement fail that query).
        type FilterCase = (&'static str, serde_json::Value, fn(&mut CachedRecord));

        let cases: Vec<FilterCase> = vec![
            (
                "required tag missing",
                json!({"text": "q", "filter": {"requireTags": ["project/recalld"]}}),
                |_r| {},
            ),
            (
                "below minimum strength",
                json!({"text": "q", "filter": {"minStrength": 0.95}}),
                |r| r.strength = 0.2,
            ),
            (
                "phase not in whitelist",
                json!({"text": "q", "filter": {"phases": ["summary"]}}),
                |r| r.phase = DecayPhase::Full,
            ),
            (
                "not permastore",
                json!({"text": "q", "filter": {"permastoreOnly": true}}),
                |r| r.is_permastore = false,
            ),
        ];

        for (label, q, mutate) in cases {
            let (old, new) = (MemoryId::new(), MemoryId::new());
            let mut replacement = rec(new);
            mutate(&mut replacement);
            let (graph, meta) = superseded_setup(old, replacement);
            let engine = engine(graph, meta);

            // The superseded memory itself satisfies the filter, so only the
            // replacement's eligibility is under test.
            let mut original = rec(old);
            original.tags = vec![Tag::new("project/recalld").unwrap()];
            original.phase = DecayPhase::Summary;
            original.is_permastore = true;

            let mut candidates = vec![candidate(original, 0.8)];
            engine
                .resolve_supersedes(&mut candidates, &query(q), &[], ns())
                .await;

            assert_eq!(ids(&candidates), vec![old], "{label}");
        }
    }

    /// If the replacement cannot be loaded, the original is all we have.
    /// The old code swallowed the error and removed it anyway.
    #[tokio::test]
    async fn metadata_store_failure_keeps_the_superseded_memory() {
        let (old, new) = (MemoryId::new(), MemoryId::new());
        let (graph, mut meta) = superseded_setup(old, rec(new));
        meta.fail = true;
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old), 0.8)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![old]);
    }

    /// Likewise when the replacement's record simply is not there.
    #[tokio::test]
    async fn replacement_with_no_record_anywhere_keeps_the_superseded_memory() {
        let old = MemoryId::new();
        let mut graph = StubGraph::default();
        graph.terminal.insert(old, MemoryId::new());
        let engine = engine(graph, StubMetaStore::default());

        let mut candidates = vec![candidate(rec(old), 0.8)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![old]);
    }

    /// Two memories superseded by the same correction collapse into one
    /// copy of it, carrying the better of the two scores.
    #[tokio::test]
    async fn many_superseded_memories_share_one_replacement() {
        let (old_a, old_b, new) = (MemoryId::new(), MemoryId::new(), MemoryId::new());
        let mut graph = StubGraph::default();
        graph.terminal.insert(old_a, new);
        graph.terminal.insert(old_b, new);
        let mut meta = StubMetaStore::default();
        meta.records.insert(new, rec(new));
        let engine = engine(graph, meta);

        let mut candidates = vec![candidate(rec(old_a), 0.5), candidate(rec(old_b), 0.85)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;

        assert_eq!(ids(&candidates), vec![new]);
        assert!(candidates[0].composite_score >= 0.85);
    }
    pub(super) fn mid(n: u128) -> MemoryId {
        MemoryId::from_bytes(n.to_be_bytes())
    }

    /// Ordered canonical fixture; real vector/entity adapters run inside the pipeline.
    struct SearchFixture(Vec<CachedRecord>);

    #[async_trait::async_trait]
    impl MetadataStore for SearchFixture {
        async fn get(&self, id: &MemoryId) -> Result<Option<CachedRecord>> {
            Ok(self.0.iter().find(|r| r.id == *id).cloned())
        }
        async fn get_batch(&self, ids: &[MemoryId]) -> Result<Vec<CachedRecord>> {
            Ok(self
                .0
                .iter()
                .filter(|r| ids.contains(&r.id))
                .cloned()
                .collect())
        }
        async fn list_namespace_memories(
            &self,
            namespace_id: NamespaceId,
        ) -> Result<Vec<MemoryId>> {
            Ok(self
                .0
                .iter()
                .filter(|r| r.namespace_id == namespace_id)
                .map(|r| r.id)
                .collect())
        }
    }

    #[async_trait::async_trait]
    impl NamespaceResolver for SearchFixture {
        async fn resolve(&self, _name: &str) -> Result<NamespaceConfig> {
            Ok(NamespaceConfig::default_namespace(1_700_000_000_000))
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingProviderRegistry for SearchFixture {
        async fn embed(&self, _namespace: &str, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![1.0, 0.0])
        }
    }

    impl FtsIndexRegistry for SearchFixture {
        fn search(&self, _ns: NamespaceId, _q: &str, _k: usize) -> Result<Vec<FtsResult>> {
            Ok(Vec::new())
        }

        fn search_eligible(
            &self,
            _ns: NamespaceId,
            _q: &str,
            _k: usize,
            _eligible: &HashSet<MemoryId>,
        ) -> Result<EligibleFtsResults> {
            Ok(EligibleFtsResults {
                matches: Vec::new(),
                coverage: FtsCoverage {
                    exhausted: true,
                    ..Default::default()
                },
            })
        }
    }

    impl RifProcessor for SearchFixture {
        fn compute_suppressions(
            &self,
            _retrieved: &[MemoryId],
            _neighbors: &[MemoryId],
        ) -> Vec<RifSuppression> {
            Vec::new()
        }
    }

    #[async_trait::async_trait]
    impl AccessRecorder for SearchFixture {
        async fn record_access(&self, _id: MemoryId, _kind: AccessKind) -> Result<()> {
            Ok(())
        }
        async fn record_access_batch(&self, _accesses: &[(MemoryId, AccessKind)]) -> Result<()> {
            Ok(())
        }
    }

    pub(super) fn search_engine(
        records: Vec<CachedRecord>,
        vectors: &[(MemoryId, f32)],
    ) -> QueryEngine {
        use crate::search::adapters::{SharedEntityIndexReader, SharedVectorIndexRegistry};
        use crate::search::{EntityIndex, FlatVectorIndex, VectorIndex, VectorMetadata};

        let mut index = FlatVectorIndex::new(2);
        for &(id, cosine) in vectors {
            // Deliberately stale derived metadata: canonical records must decide eligibility.
            index
                .add(
                    id,
                    &[cosine, (1.0 - cosine * cosine).sqrt()],
                    VectorMetadata {
                        namespace_id: ns(),
                        decay_phase: 1,
                        tags: Vec::new(),
                    },
                )
                .unwrap();
        }
        let mut entities = EntityIndex::new();
        for record in &records {
            entities.add(record.id, &record.entities);
        }
        let fixture = Arc::new(SearchFixture(records));
        QueryEngine::new(
            fixture.clone(),
            fixture.clone(),
            Arc::new(SharedVectorIndexRegistry::new(Arc::new(
                tokio::sync::RwLock::new(index),
            ))),
            fixture.clone(),
            Arc::new(SharedEntityIndexReader::new(Arc::new(
                tokio::sync::RwLock::new(entities),
            ))),
            Arc::new(StubCache),
            fixture.clone(),
            Arc::new(StubGraph::default()),
            fixture.clone(),
            fixture,
        )
    }

    fn matching_record(n: u128) -> CachedRecord {
        let mut record = rec(mid(n));
        record.tags = vec![
            Tag::new("project/recalld").unwrap(),
            Tag::new("type/test").unwrap(),
        ];
        record.is_permastore = true;
        record
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exact_vector_prefilter_matches_eligible_oracle_beyond_1600_rejections() {
        let mut records = Vec::new();
        let mut vectors = Vec::new();
        for n in 1..=1701 {
            let mut record = matching_record(n);
            match n % 7 {
                0 => record.namespace_id = other_ns(),
                1 => record.phase = DecayPhase::Tombstone,
                2 => record.phase = DecayPhase::Ghost,
                3 => {
                    record.tags.pop();
                } // ALL required tags, not OR
                4 => record.tags.push(Tag::new("blocked").unwrap()),
                5 => record.strength = 0.69,
                _ => record.is_permastore = false,
            }
            vectors.push((record.id, 1.0));
            records.push(record);
        }
        // Missing canonical records cannot consume a vector slot either.
        vectors.push((mid(9999), 1.0));
        let mut oracle = Vec::new();
        for n in 2000..2032 {
            let mut record = matching_record(n);
            record.strength = 0.7; // Inclusive hard-strength boundary.
            let cosine = 0.2 + (n % 8) as f32 * 0.05;
            oracle.push((record.id, cosine));
            vectors.push((record.id, cosine));
            records.push(record);
        }
        oracle.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
        for mode in ["embedding_only", "embedding_plus_metadata"] {
            for reverse in [false, true] {
                if reverse {
                    records.reverse();
                    vectors.reverse();
                }
                let engine = search_engine(records.clone(), &vectors);
                let response = engine
                    .search(query(json!({
                        "text": "q", "queryMode": mode, "limit": 200,
                        "filter": {"requireTags": ["project/recalld", "type/test"],
                            "excludeTags": ["blocked"], "phases": ["full"],
                            "minStrength": 0.7, "permastoreOnly": true}
                    })))
                    .await
                    .unwrap();
                let actual: Vec<_> = response
                    .results
                    .iter()
                    .map(|r| (r.memory_id, r.score.unwrap()))
                    .collect();
                assert_eq!(actual, oracle, "mode={mode}, reverse={reverse}");
                for result in response.results {
                    assert!(
                        (result.composite_score.unwrap() - 0.90 * result.score.unwrap()).abs()
                            < 1e-6
                    );
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn entity_recall_filters_namespace_and_metadata_before_cap() {
        let mut target = matching_record(1000);
        target.entities = vec!["Alice".into()];
        let mut target_two = matching_record(1001);
        target_two.entities = target.entities.clone();
        let mut records = vec![target, target_two];
        let q = json!({"text": "q", "limit": 1, "entities": ["Alice", "Bob"],
            "filter": {"requireTags": ["project/recalld", "type/test"]}});
        let before = search_engine(records.clone(), &[])
            .search(query(q.clone()))
            .await
            .unwrap();
        for n in 1..=100 {
            let mut record = matching_record(n);
            record.entities = vec!["Alice".into(), "Bob".into()];
            match n % 4 {
                0 => record.namespace_id = other_ns(),
                1 => record.phase = DecayPhase::Tombstone,
                2 => record.phase = DecayPhase::Ghost,
                _ => record.tags.clear(),
            }
            records.push(record);
        }
        for reverse in [false, true] {
            if reverse {
                records.reverse();
            }
            let after = search_engine(records.clone(), &[])
                .search(query(q.clone()))
                .await
                .unwrap();
            assert_eq!(after.total_matches, 2);
            assert_eq!(after.results[0].memory_id, mid(1000));
            assert_eq!(
                after.results[0].composite_score,
                before.results[0].composite_score
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metadata_scan_ranks_all_eligible_records_with_temporal_boost_and_uuid_ties() {
        let mut records = Vec::new();
        for n in 1..=20 {
            let mut record = rec(mid(n));
            if n <= 10 {
                record.phase = if n % 2 == 0 {
                    DecayPhase::Tombstone
                } else {
                    DecayPhase::Ghost
                };
            }
            record.decay_strength = 0.1;
            records.push(record);
        }
        let mut stronger = rec(mid(100));
        stronger.decay_strength = 1.0;
        stronger.created_at -= 100 * 86_400_000;
        records.push(stronger);
        for n in [102, 101] {
            let mut boosted = rec(mid(n));
            boosted.decay_strength = 0.8;
            records.push(boosted);
        }
        for reverse in [false, true] {
            if reverse {
                records.reverse();
            }
            let response = search_engine(records.clone(), &[])
                .search(query(json!({
                    "queryMode": "metadata_only", "limit": 2, "filter": {"minStrength": 0.0},
                    "timeRangeStart": 1_699_999_999_000i64, "timeRangeEnd": 1_700_000_001_000i64
                })))
                .await
                .unwrap();
            assert_eq!(
                response
                    .results
                    .iter()
                    .map(|r| r.memory_id)
                    .collect::<Vec<_>>(),
                vec![mid(101), mid(102)]
            );
            assert_eq!(response.total_matches, 13);
            assert!((response.results[0].composite_score.unwrap() - 0.12).abs() < 1e-6);
        }
    }

    #[tokio::test]
    async fn supersession_inheritance_is_invariant_to_discovery_order_and_weaker_own_evidence() {
        for own in [None, Some(0.1), Some(0.95)] {
            for reverse in [false, true] {
                let mut graph = StubGraph::default();
                graph.terminal.insert(mid(1), mid(100));
                graph.terminal.insert(mid(2), mid(100));
                let mut meta = StubMetaStore::default();
                meta.records.insert(mid(100), rec(mid(100)));
                let engine = engine(graph, meta);
                let mut candidates = vec![candidate(rec(mid(1)), 0.9), candidate(rec(mid(2)), 0.8)];
                for n in 10..20 {
                    candidates.push(candidate(rec(mid(n)), 0.5));
                }
                if let Some(score) = own {
                    candidates.push(candidate(rec(mid(100)), score));
                }
                if reverse {
                    candidates.reverse();
                }
                engine.resolve_supersedes(&mut candidates, &query(json!({"text": "q",
                    "timeRangeStart": 1_699_999_999_000i64, "timeRangeEnd": 1_700_000_001_000i64
                })), &[], ns()).await;
                candidates
                    .sort_by(|a, b| b.composite_score.partial_cmp(&a.composite_score).unwrap());
                assert_eq!(candidates[0].record.id, mid(100));
                assert_eq!(candidates[0].composite_score, own.unwrap_or(0.0).max(0.9));
                assert_eq!(candidates[0].raw_vector_score, own);
                assert_eq!(candidates.len(), 11);
            }
        }
    }

    #[tokio::test]
    async fn supersession_can_inherit_after_own_min_score_rejection_without_fabricating_score() {
        let (graph, meta) = superseded_setup(mid(1), rec(mid(2)));
        let engine = engine(graph, meta);
        let q = query(json!({"text": "q", "minScore": 0.6}));
        let mut candidates = vec![candidate(rec(mid(1)), 0.9), candidate(rec(mid(2)), 0.1)];
        engine.apply_filters(&mut candidates, &q, &[], ns());
        engine
            .resolve_supersedes(&mut candidates, &q, &[], ns())
            .await;
        assert_eq!(ids(&candidates), vec![mid(2)]);
        assert_eq!(candidates[0].composite_score, 0.9);
        assert_eq!(candidates[0].raw_vector_score, None);

        // A newly discovered terminal has no independent metadata-channel score.
        let mut candidates = vec![candidate(rec(mid(1)), 0.01)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;
        assert_eq!(candidates[0].composite_score, 0.01);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_namespace_enumeration_failure_is_not_an_empty_success() {
        let mut engine = search_engine(Vec::new(), &[]);
        engine.meta_store = Arc::new(StubMetaStore {
            fail: true,
            ..Default::default()
        });
        assert!(matches!(
            engine.search(query(json!({"text": "q"}))).await,
            Err(SearchError::MetadataError(_))
        ));
    }
    #[tokio::test]
    async fn already_discovered_replacement_is_revalidated_against_canonical_tombstone() {
        let mut tombstone = rec(mid(2));
        tombstone.phase = DecayPhase::Tombstone;
        let (graph, meta) = superseded_setup(mid(1), tombstone);
        let engine = engine(graph, meta);
        let mut candidates = vec![candidate(rec(mid(1)), 0.9), candidate(rec(mid(2)), 0.1)];
        engine
            .resolve_supersedes(&mut candidates, &query(json!({"text": "q"})), &[], ns())
            .await;
        // Legacy fallback keeps the original; stale terminal metadata cannot leak it.
        assert_eq!(ids(&candidates), vec![mid(1)]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_pipeline_supersession_preserves_inherited_temporal_score() {
        for own_cosine in [None, Some(0.1), Some(0.95)] {
            let mut vectors = vec![(mid(1), 0.9)];
            if let Some(score) = own_cosine {
                vectors.push((mid(2), score));
            }
            let mut engine = search_engine(vec![rec(mid(1)), rec(mid(2))], &vectors);
            let mut graph = StubGraph::default();
            graph.terminal.insert(mid(1), mid(2));
            engine.graph = Arc::new(graph);
            let response = engine
                .search(query(json!({"text": "q",
                    "timeRangeStart": 1_699_999_999_000i64, "timeRangeEnd": 1_700_000_001_000i64
                })))
                .await
                .unwrap();
            assert_eq!(response.results.len(), 1);
            let result = &response.results[0];
            assert_eq!(result.memory_id, mid(2));
            let expected = own_cosine.unwrap_or(0.0).max(0.9) * 0.90 * 1.5;
            assert!((result.composite_score.unwrap() - expected).abs() < 1e-6);
            assert_eq!(result.score, own_cosine);
        }
    }

    /// Namespace enumeration must only discover IDs; batch reads decide eligibility.
    struct NamespaceEligibilityProbe {
        fail_batch: bool,
    }

    #[async_trait::async_trait]
    impl MetadataStore for NamespaceEligibilityProbe {
        async fn get(&self, _id: &MemoryId) -> Result<Option<CachedRecord>> {
            unreachable!("eligibility uses batch canonical reads")
        }

        async fn list_namespace_memories(
            &self,
            namespace_id: NamespaceId,
        ) -> Result<Vec<MemoryId>> {
            assert_eq!(namespace_id, ns());
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            Ok(vec![mid(1), mid(2), mid(3)])
        }

        async fn get_batch(&self, ids: &[MemoryId]) -> Result<Vec<CachedRecord>> {
            assert_eq!(ids, &[mid(1), mid(2), mid(3)]);
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
            if self.fail_batch {
                return Err(SearchError::MetadataError("canonical read failed".into()));
            }
            let mut tombstone = rec(mid(2));
            tombstone.phase = DecayPhase::Tombstone;
            // ID 2 was tombstoned and ID 3 disappeared after enumeration.
            Ok(vec![rec(mid(1)), tombstone])
        }
    }

    #[tokio::test(start_paused = true)]
    async fn eligibility_timing_includes_namespace_enumeration_and_canonical_loading() {
        let mut engine = search_engine(Vec::new(), &[]);
        engine.meta_store = Arc::new(NamespaceEligibilityProbe { fail_batch: false });
        let response = engine
            .search(query(json!({
                "queryMode": "metadata_only", "filter": {"minStrength": 0.0}
            })))
            .await
            .unwrap();
        assert_eq!(
            response
                .results
                .iter()
                .map(|r| r.memory_id)
                .collect::<Vec<_>>(),
            vec![mid(1)]
        );
        let timings = response.timings.unwrap();
        assert_eq!(timings.eligibility_us, 5000);
        assert_eq!(timings.total_us, 5000);
        assert_eq!(timings.load_metadata_us, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn namespace_canonical_batch_failure_is_not_an_empty_success() {
        let mut engine = search_engine(Vec::new(), &[]);
        engine.meta_store = Arc::new(NamespaceEligibilityProbe { fail_batch: true });
        assert!(matches!(
            engine
                .search(query(json!({
                    "queryMode": "metadata_only", "filter": {"minStrength": 0.0}
                })))
                .await,
            Err(SearchError::MetadataError(_))
        ));
    }
}
