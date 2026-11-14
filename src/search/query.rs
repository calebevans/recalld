//! Search query types and validation.

use serde::{Deserialize, Serialize};

use super::error::SearchError;
use crate::model::{DecayPhase, Tag};

// ---------------------------------------------------------------------------
// QueryMode
// ---------------------------------------------------------------------------

/// Controls which retrieval paths the pipeline activates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// Embedding-driven retrieval; all supplied hard filters still apply.
    EmbeddingOnly,
    /// Embedding-driven retrieval with explicit metadata criteria (default).
    EmbeddingPlusMetadata,
    /// Tag and attribute filtering without vector search.
    MetadataOnly,
}

impl Default for QueryMode {
    fn default() -> Self {
        Self::EmbeddingPlusMetadata
    }
}

// ---------------------------------------------------------------------------
// SearchFilter (pipeline-level)
// ---------------------------------------------------------------------------

/// Canonical hard filters apply in every query mode, including EmbeddingOnly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilter {
    /// Require ALL of these tags to be present on the memory.
    #[serde(default)]
    pub require_tags: Vec<Tag>,

    /// Exclude memories that have ANY of these tags.
    #[serde(default)]
    pub exclude_tags: Vec<Tag>,

    /// Only include memories in these phases. Empty = all phases pass.
    #[serde(default)]
    pub phases: Vec<DecayPhase>,

    /// Minimum raw retrievability R.
    #[serde(default)]
    pub min_strength: Option<f32>,

    /// Only include permastore memories.
    #[serde(default)]
    pub permastore_only: bool,
}

// ---------------------------------------------------------------------------
// SearchQuery
// ---------------------------------------------------------------------------

/// A fully-specified search request passed to `QueryEngine::search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    /// Permit legacy exposure recording. False also bypasses behavioral record caches.
    #[serde(default = "default_record_access")]
    pub record_access: bool,
    /// Precomputed vector for semantic search. Takes precedence over text for
    /// embedding; text (or fts_query) still supplies the keyword channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_embedding: Option<Vec<f32>>,

    /// Free-text query, embedded via the namespace's provider unless a vector is supplied.
    #[serde(default)]
    pub text: Option<String>,

    /// Optional keyword-focused query for FTS search.
    /// When present, used instead of `text` for FTS5 matching.
    /// Allows the LLM to generate a concise keyword query optimized
    /// for BM25 while keeping `text` for semantic embedding.
    #[serde(default)]
    pub fts_query: Option<String>,

    /// Target namespace name. Defaults to `"default"`.
    #[serde(default = "default_namespace")]
    pub namespace: String,

    /// Hard eligibility filters applied in every query mode.
    #[serde(default)]
    pub filter: SearchFilter,

    /// Maximum number of results to return. Clamped to `[1, 200]`.
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Minimum cosine similarity score threshold.
    #[serde(default)]
    pub min_score: f32,

    /// Include Ghost-phase memories in results.
    #[serde(default)]
    pub include_ghosts: bool,

    /// Which retrieval paths to activate.
    #[serde(default)]
    pub query_mode: QueryMode,

    /// Number of graph hops to include (0 = direct matches only, max 3).
    #[serde(default)]
    pub graph_depth: u8,

    /// Optional inclusive lower bound for temporal boosting (millis since epoch).
    #[serde(default)]
    pub time_range_start: Option<i64>,

    /// Optional inclusive upper bound for temporal boosting (millis since epoch).
    #[serde(default)]
    pub time_range_end: Option<i64>,

    /// Named entities provided by the caller for entity overlap scoring.
    #[serde(default)]
    pub entities: Vec<String>,
}

/// Omitted access control preserves each v1 path's existing behavior.
pub fn default_record_access() -> bool {
    true
}

fn default_namespace() -> String {
    "default".to_string()
}

fn default_limit() -> usize {
    10
}

impl SearchQuery {
    /// Maximum user-facing results per query.
    pub const MAX_USER_LIMIT: usize = 200;

    /// Maximum internal candidate pool size (fetch_k ceiling).
    /// Sized to accommodate 8x over-fetch at the maximum user-facing limit.
    pub const MAX_FETCH_K: usize = 1600;

    /// Maximum graph traversal depth.
    pub const MAX_GRAPH_DEPTH: u8 = 3;

    /// Validate and normalize the query.
    pub fn validate(&mut self) -> std::result::Result<(), SearchError> {
        // Clamp limit
        self.limit = self.limit.clamp(1, Self::MAX_USER_LIMIT);

        // Clamp graph_depth
        self.graph_depth = self.graph_depth.min(Self::MAX_GRAPH_DEPTH);

        // Clamp min_score
        self.min_score = self.min_score.clamp(0.0, 1.0);

        // Mode-specific validation
        match self.query_mode {
            QueryMode::EmbeddingOnly | QueryMode::EmbeddingPlusMetadata => {
                if self.query_embedding.is_none()
                    && self.text.as_ref().is_none_or(|t| t.trim().is_empty())
                {
                    return Err(SearchError::EmptyQuery);
                }
            }
            QueryMode::MetadataOnly => {
                if self.filter.require_tags.is_empty()
                    && self.filter.phases.is_empty()
                    && self.filter.min_strength.is_none()
                    && !self.filter.permastore_only
                {
                    return Err(SearchError::EmptyQuery);
                }
            }
        }

        Ok(())
    }
}

/// Validate and normalize only explicitly supplied query vectors. Keep provider
/// output handling unchanged for text-only legacy requests.
pub(crate) fn normalize_query_embedding(
    embedding: &mut [f32],
    dimensions: usize,
) -> Result<(), SearchError> {
    if embedding.len() != dimensions {
        return Err(SearchError::InvalidEmbedding(format!(
            "embedding has {} dimensions, namespace requires {dimensions}",
            embedding.len(),
        )));
    }
    if embedding.iter().any(|x| !x.is_finite()) {
        return Err(SearchError::InvalidEmbedding(
            "embedding elements must be finite".into(),
        ));
    }
    // f64 accumulation handles every finite f32, including huge values and
    // subnormals, without the overflow/underflow of a squared f32 norm.
    let norm_squared: f64 = embedding.iter().map(|&x| f64::from(x).powi(2)).sum();
    if !norm_squared.is_finite() || norm_squared <= 0.0 {
        return Err(SearchError::InvalidEmbedding(
            "embedding must have a finite positive norm".into(),
        ));
    }
    let norm = norm_squared.sqrt();
    for x in embedding {
        *x = (f64::from(*x) / norm) as f32;
    }
    Ok(())
}
