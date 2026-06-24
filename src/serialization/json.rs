//! JSON wire format types for the Recalld HTTP API.
//!
//! All types follow these conventions:
//! - `camelCase` field names via `#[serde(rename_all = "camelCase")]`
//! - `None` fields omitted via `#[serde(skip_serializing_if = "Option::is_none")]`
//! - Timestamps as integer milliseconds since epoch (not ISO 8601)
//! - Enums as snake_case strings via `#[serde(rename_all = "snake_case")]`

use serde::{Deserialize, Serialize};

use crate::model::decay::DecayPhase;
use crate::model::id::MemoryId;
use crate::model::memory::AccessEvent;
use crate::model::record::CachedRecord;
use crate::model::tag::Tag;

// ═══════════════════════════════════════════════════════════════════════
// Response Wrappers
// ═══════════════════════════════════════════════════════════════════════

/// Standard success envelope for single-resource responses.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse<T: Serialize> {
    /// The response payload.
    pub data: T,

    /// Server-side processing time in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub took_us: Option<u64>,
}

/// Standard error envelope.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    /// Machine-readable error code (e.g., "DIMENSION_MISMATCH").
    pub error: String,

    /// Human-readable description.
    pub message: String,

    /// The request field that caused the error, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// Paginated response wrapper for list endpoints.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginatedResponse<T: Serialize> {
    /// The page of results.
    pub data: Vec<T>,
    /// Total number of records matching the query.
    pub total: u64,
    /// Offset into the full result set.
    pub offset: u64,
    /// Maximum number of results per page.
    pub limit: u64,

    /// Server-side processing time in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub took_us: Option<u64>,
}

/// Pagination query parameters (deserialized from query string).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginationParams {
    /// Offset into the result set. Default: 0.
    #[serde(default = "default_offset")]
    pub offset: u64,

    /// Maximum results to return. Default: 50.
    #[serde(default = "default_limit")]
    pub limit: u64,
}

fn default_offset() -> u64 {
    0
}

fn default_limit() -> u64 {
    50
}

/// Response wrapper for search results, including relevance scores.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// The matching memories with scores.
    pub hits: Vec<SearchHit>,
    /// Total number of matches.
    pub total: u64,

    /// Server-side processing time in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub took_us: Option<u64>,
}

/// A single search result with its similarity score.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// The memory record (without embedding by default).
    pub memory: MemoryResponse,

    /// Cosine similarity score (0.0 to 1.0 for normalized vectors).
    pub score: f32,
}

// ═══════════════════════════════════════════════════════════════════════
// MemoryResponse
// ═══════════════════════════════════════════════════════════════════════

/// The JSON representation of a memory returned by the API.
///
/// Maps 1:1 to the `Memory` type in CS-01 but defined here
/// to keep serde derives isolated to the serialization module.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryResponse {
    /// Unique identifier (UUID v7).
    pub id: MemoryId,
    /// Resolved namespace name.
    pub namespace: String,
    /// Created-at timestamp (millis since epoch).
    pub created_at: i64,
    /// Last-accessed-at timestamp (millis since epoch).
    pub last_accessed_at: i64,
    /// Short description of the memory.
    pub summary: String,

    /// Full content (present only when requested and in Phase 1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,

    /// Validated tags attached to this memory.
    pub tags: Vec<Tag>,
    /// Current decay phase.
    pub phase: DecayPhase,
    /// Raw FSRS retrievability R, in [0.0, 1.0].
    pub strength: f32,
    /// Effective retrievability including connection bonus.
    pub decay_strength: f32,
    /// FSRS stability S in days.
    pub stability: f32,
    /// FSRS difficulty D.
    pub difficulty: f32,
    /// Whether stability exceeds the permastore threshold.
    pub is_permastore: bool,
    /// Cached count of outgoing edges.
    pub edge_count: u16,

    /// Embedding vector (included only when explicitly requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,

    /// Access history (included only when explicitly requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_history: Option<Vec<AccessEvent>>,
}

impl MemoryResponse {
    /// Constructs a `MemoryResponse` from a `CachedRecord` and
    /// resolved namespace name. Embedding and access history
    /// are set to `None` -- the caller populates them if requested.
    pub fn from_cached(record: &CachedRecord, namespace_name: String) -> Self {
        Self {
            id: record.id,
            namespace: namespace_name,
            created_at: record.created_at,
            last_accessed_at: record.last_accessed_at,
            summary: record.summary.clone(),
            full_text: None, // loaded on demand from text.log
            tags: record.tags.clone(),
            phase: record.phase,
            strength: record.strength,
            decay_strength: record.decay_strength,
            stability: record.stability,
            difficulty: record.difficulty,
            is_permastore: record.is_permastore,
            edge_count: record.edge_count,
            embedding: None,
            access_history: None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Request Types
// ═══════════════════════════════════════════════════════════════════════

/// POST /memories -- Create a new memory.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMemoryRequest {
    /// Target namespace name. Defaults to "default".
    #[serde(default = "default_namespace")]
    pub namespace: String,

    /// Short description of the memory (max 2,000 bytes).
    pub summary: String,

    /// Optional detailed content (max 1 MB).
    #[serde(default)]
    pub full_text: Option<String>,

    /// Tags as raw strings. Validated and lowercased server-side.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Pre-computed embedding vector. If omitted, the server
    /// generates one using the namespace's embedding provider.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub embedding: Option<Vec<f32>>,

    /// Override the namespace's default initial stability (days).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub initial_stability: Option<f32>,
}

fn default_namespace() -> String {
    "default".to_string()
}

/// PATCH /memories/{id} -- Update mutable fields.
/// Only provided fields are updated; omitted fields are untouched.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMemoryRequest {
    /// Replace the summary.
    #[serde(default)]
    pub summary: Option<String>,

    /// Replace tags entirely (not additive).
    #[serde(default)]
    pub tags: Option<Vec<String>>,

    /// Force a manual reinforcement (equivalent to an access
    /// with AccessKind::ManualReinforcement).
    #[serde(default)]
    pub reinforce: Option<bool>,
}

/// POST /search -- Search for memories.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    /// Natural-language query text. The server generates an
    /// embedding from this for similarity search.
    #[serde(default)]
    pub query: Option<String>,

    /// Pre-computed query embedding. Mutually exclusive with `query`.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,

    /// Filter to a specific namespace. Omit to search the default.
    #[serde(default = "default_namespace")]
    pub namespace: String,

    /// Tag filter expressions. All must match (AND semantics).
    #[serde(default)]
    pub tags: Vec<String>,

    /// Minimum decay strength to include in results.
    #[serde(default)]
    pub min_strength: Option<f32>,

    /// Maximum number of results to return. Default: 10, max: 100.
    #[serde(default = "default_search_limit")]
    pub limit: u32,

    /// How many hops of related memories to include (0 = direct
    /// matches only, 1 = direct neighbors, etc.). Default: 0.
    #[serde(default)]
    pub depth: u8,

    /// Whether to include the embedding vectors in results.
    /// Default: false (saves bandwidth).
    #[serde(default)]
    pub include_embeddings: bool,

    /// Whether to include access history in results.
    /// Default: false.
    #[serde(default)]
    pub include_history: bool,
}

fn default_search_limit() -> u32 {
    10
}

/// POST /namespaces -- Create a new namespace.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceRequest {
    /// Namespace name: 1-64 chars, alphanumeric + hyphens + underscores.
    pub name: String,

    /// Embedding dimensionality. Fixed at creation time.
    pub embedding_dim: u32,

    /// Initial stability for new memories in this namespace (days).
    #[serde(default = "default_initial_stability")]
    pub initial_stability: f32,

    /// Desired retention rate. Default: 0.9.
    #[serde(default = "default_desired_retention")]
    pub desired_retention: f32,
}

fn default_initial_stability() -> f32 {
    3.7145
}

fn default_desired_retention() -> f32 {
    0.9
}

/// GET /namespaces/{name} response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceResponse {
    /// Sequential namespace identifier.
    pub id: u32,
    /// Human-readable namespace name.
    pub name: String,
    /// Embedding vector dimensionality.
    pub embedding_dim: u32,
    /// Initial FSRS stability for new memories (days).
    pub initial_stability: f32,
    /// Default FSRS difficulty for new memories.
    pub default_difficulty: f32,
    /// Permastore stability threshold (days).
    pub permastore_threshold: f32,
    /// Target retention rate.
    pub desired_retention: f32,
    /// Creation timestamp (millis since epoch).
    pub created_at: i64,
    /// Total number of memories in this namespace.
    pub memory_count: u64,
}
