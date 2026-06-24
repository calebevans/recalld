//! Response types for the search pipeline.

use serde::{Deserialize, Serialize};

use crate::model::{DecayPhase, MemoryId, Tag};

// ---------------------------------------------------------------------------
// SearchResult
// ---------------------------------------------------------------------------

/// A single memory in the search response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// Unique identifier for the memory.
    pub memory_id: MemoryId,

    /// Created-at timestamp (millis since epoch).
    pub created_at: i64,

    /// Last-accessed-at timestamp (millis since epoch).
    pub last_accessed_at: i64,

    /// Cosine similarity to the query embedding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,

    /// FTS5 keyword relevance score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_score: Option<f32>,

    /// Final composite ranking score used for ordering results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composite_score: Option<f32>,

    /// Raw FSRS retrievability R(t,S) at query time.
    pub retrievability: f32,

    /// Effective retrievability after connection bonus.
    pub effective_r: f32,

    /// Current decay phase.
    pub phase: DecayPhase,

    /// The memory's summary text. Absent for Ghost phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    /// Full text of the memory. Only present when phase is Full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,

    /// Tags associated with this memory.
    pub tags: Vec<Tag>,

    /// Number of graph edges.
    pub edge_count: u16,

    /// Spreading activation score for graph-discovered results.
    /// None for direct matches (vector, FTS, entity recall, metadata).
    /// Some(score) for results found via graph expansion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation_score: Option<f32>,

    /// Whether this memory is in the permastore.
    pub is_permastore: bool,

    /// FSRS stability in days.
    pub stability: f32,

    /// Namespace name this memory belongs to.
    pub namespace: String,
}

// ---------------------------------------------------------------------------
// SearchResponse
// ---------------------------------------------------------------------------

/// The complete response from a search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// Ranked results ordered by descending composite score.
    pub results: Vec<SearchResult>,

    /// Total candidates that passed filters before limit was applied.
    pub total_matches: usize,

    /// Wall-clock query time in milliseconds.
    pub query_time_ms: f64,

    /// Namespace the query targeted.
    pub namespace: String,

    /// Per-stage latency breakdown (when requested or in debug mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<StageTimings>,
}

// ---------------------------------------------------------------------------
// MemoryResponse
// ---------------------------------------------------------------------------

/// Full memory record returned by `get_memory`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryResponse {
    /// Unique identifier.
    pub id: MemoryId,
    /// Namespace name.
    pub namespace: String,
    /// Created-at timestamp (millis since epoch).
    pub created_at: i64,
    /// Last-accessed-at timestamp (millis since epoch).
    pub last_accessed_at: i64,
    /// Summary text (absent for Ghost phase).
    pub summary: Option<String>,
    /// Full text (absent for Summary and Ghost phases).
    pub full_text: Option<String>,
    /// Tags.
    pub tags: Vec<Tag>,
    /// Current decay phase.
    pub phase: DecayPhase,
    /// Raw FSRS retrievability.
    pub strength: f32,
    /// Effective retrievability with connection bonus.
    pub decay_strength: f32,
    /// FSRS stability in days.
    pub stability: f32,
    /// Whether stability exceeds permastore threshold.
    pub is_permastore: bool,
    /// Cached outgoing edge count.
    pub edge_count: u16,
    /// Number of access events.
    pub access_count: usize,
}

// ---------------------------------------------------------------------------
// StageTimings
// ---------------------------------------------------------------------------

/// Per-stage latency breakdown for the 9-step search pipeline (in microseconds).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageTimings {
    /// Stage 1: Query parsing and validation.
    pub parse_us: u64,
    /// Stage 2: Text-to-embedding.
    pub embed_us: u64,
    /// Stage 3: Flat SIMD vector search.
    pub vector_search_us: u64,
    /// Stage 3b: FTS5 keyword search.
    pub fts_search_us: u64,
    /// Stage 3c: Convex combination score fusion of vector and FTS results.
    pub score_fusion_us: u64,
    /// Stage 3d: Entity index recall.
    pub entity_recall_us: u64,
    /// Stage 4: Load metadata from cache or meta.db.
    pub load_metadata_us: u64,
    /// Stage 4b: Graph expansion (spreading activation + neighbor metadata load).
    pub graph_expansion_us: u64,
    /// Stage 5: Apply post-retrieval filters.
    pub apply_filters_us: u64,
    /// Stage 6: Calculate effective R.
    pub calculate_r_us: u64,
    /// Stage 7: Apply retrieval-induced forgetting.
    pub apply_rif_us: u64,
    /// Stage 8: Final ranking.
    pub rank_us: u64,
    /// Stage 8b: Build response.
    pub build_response_us: u64,
    /// Total pipeline wall-clock time.
    pub total_us: u64,
}
