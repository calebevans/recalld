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
    /// Vector similarity search only; skips metadata filtering.
    EmbeddingOnly,
    /// Vector search with post-hoc metadata filters (default).
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

/// Post-retrieval filters applied after vector search and metadata loading.
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
    /// Free-text query, embedded via the namespace's provider.
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

    /// Post-retrieval filters.
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
                if self.text.as_ref().is_none_or(|t| t.trim().is_empty()) {
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
