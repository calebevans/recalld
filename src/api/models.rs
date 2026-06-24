//! API-specific request/response types for the HTTP server.
//!
//! These types are specific to the API layer. The core wire types
//! ([`CreateMemoryRequest`], [`SearchRequest`], [`MemoryResponse`],
//! [`ApiResponse`], [`SearchResponse`], etc.) are defined in
//! [`crate::serialization`] (CS-02) and re-used here.
//!
//! This module adds HTTP-endpoint-specific types that only make sense
//! in the context of the axum server (e.g. `CreateMemoryApiRequest`
//! with `parent_id`, `ReinforceRequest`, health check types).

use serde::{Deserialize, Serialize};

use crate::model::MemoryId;

// ═══════════════════════════════════════════════════════════════════════
// Memory Endpoints
// ═══════════════════════════════════════════════════════════════════════

/// POST /memories -- extended request type.
///
/// Wraps CS-02's `CreateMemoryRequest` with the optional `parent_id`
/// field for establishing hierarchical edges on creation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMemoryApiRequest {
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

    /// Pre-computed embedding vector. If omitted, the server generates
    /// one using the namespace's configured embedding provider.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub embedding: Option<Vec<f32>>,

    /// Override the namespace's default initial stability (days).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub initial_stability: Option<f32>,

    /// Optional parent memory ID. If provided, creates a parent->child
    /// edge from `parent_id` to the new memory.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_id: Option<MemoryId>,
}

/// Default namespace name for request types.
fn default_namespace() -> String {
    "default".to_string()
}

/// Response for DELETE /memories/:id.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResponse {
    /// The ID of the deleted memory.
    pub id: MemoryId,

    /// Whether the memory was found and deleted (`true`) vs. already
    /// absent (`false`).
    pub deleted: bool,
}

// ═══════════════════════════════════════════════════════════════════════
// Reinforcement Endpoint
// ═══════════════════════════════════════════════════════════════════════

/// POST /memories/:id/reinforce -- manual reinforcement request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReinforceRequest {
    /// Quality of the reinforcement. Maps to FSRS rating:
    ///   1 = "again" (forgot), 2 = "hard", 3 = "good", 4 = "easy".
    /// Default: 3 (good).
    #[serde(default = "default_quality")]
    pub quality: u8,
}

/// Default FSRS quality rating (3 = "good").
fn default_quality() -> u8 {
    3
}

/// POST /memories/:id/reinforce -- response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReinforceResponse {
    /// The ID of the reinforced memory.
    pub id: MemoryId,

    /// Updated decay strength after reinforcement (0.0--1.0).
    pub strength: f32,

    /// Updated stability in days.
    pub stability: f32,

    /// New decay phase after reinforcement.
    pub phase: String,

    /// Whether the memory crossed into permastore.
    pub is_permastore: bool,
}

// ═══════════════════════════════════════════════════════════════════════
// Similar Memories Endpoint
// ═══════════════════════════════════════════════════════════════════════

/// POST /similar/:id -- find memories similar to a given memory.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindSimilarRequest {
    /// Maximum number of similar memories to return. Default: 10, max: 100.
    #[serde(default = "default_similar_limit")]
    pub limit: u32,

    /// Minimum similarity score threshold. Default: `None` (no threshold).
    #[serde(default)]
    pub min_score: Option<f32>,

    /// Whether to search only within the same namespace as the source
    /// memory. Default: `true`.
    #[serde(default = "default_same_namespace")]
    pub same_namespace: bool,

    /// Whether to include embedding vectors in results. Default: `false`.
    #[serde(default)]
    pub include_embeddings: bool,
}

/// Default limit for similar-memories queries.
fn default_similar_limit() -> u32 {
    10
}

/// Default for `same_namespace` filter.
fn default_same_namespace() -> bool {
    true
}

// ═══════════════════════════════════════════════════════════════════════
// Namespace Endpoints
// ═══════════════════════════════════════════════════════════════════════

/// GET /namespaces/:id/stats -- namespace statistics response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceStatsResponse {
    /// Namespace name.
    pub name: String,

    /// Namespace integer ID.
    pub id: u32,

    /// Total memory count in this namespace.
    pub memory_count: u64,

    /// Breakdown by decay phase.
    pub phase_counts: PhaseCounts,

    /// Number of permastore memories.
    pub permastore_count: u64,

    /// Average decay strength across all memories.
    pub avg_strength: f32,

    /// Total edge count for memories in this namespace.
    pub edge_count: u64,

    /// Embedding dimensionality.
    pub embedding_dim: u32,

    /// Disk space used by this namespace's vectors in bytes.
    pub vector_bytes: u64,
}

/// Memory count breakdown by decay phase.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseCounts {
    /// Memories in Full phase (phase 1).
    pub full: u64,
    /// Memories in Summary phase (phase 2).
    pub summary: u64,
    /// Memories in Ghost phase (phase 3).
    pub ghost: u64,
}

/// GET /namespaces -- list response item.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceListItem {
    /// Namespace integer ID.
    pub id: u32,
    /// Human-readable namespace name.
    pub name: String,
    /// Embedding vector dimensionality.
    pub embedding_dim: u32,
    /// Total memory count in this namespace.
    pub memory_count: u64,
    /// Creation timestamp (millis since epoch).
    pub created_at: i64,
}

// ═══════════════════════════════════════════════════════════════════════
// Health & Metrics
// ═══════════════════════════════════════════════════════════════════════

/// GET /health -- health check response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    /// Overall status: `"healthy"`, `"degraded"`, or `"unhealthy"`.
    pub status: HealthStatus,

    /// Server uptime in seconds.
    pub uptime_secs: u64,

    /// Individual subsystem health.
    pub subsystems: SubsystemHealth,
}

/// Overall health status of the API server.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// All subsystems are operational.
    Healthy,
    /// Non-critical subsystems are down (e.g. embedding provider).
    Degraded,
    /// Critical subsystems are down (storage, cache).
    Unhealthy,
}

/// Per-subsystem health checks.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsystemHealth {
    /// Storage engine: can read/write meta.db.
    pub storage: ComponentHealth,

    /// RAM cache: operational, reports hit rate.
    pub cache: ComponentHealth,

    /// Vector index: loaded and searchable.
    pub vector_index: ComponentHealth,

    /// Embedding provider: reachable (last check).
    pub embedding: ComponentHealth,

    /// Decay engine: sweep thread alive.
    pub decay: ComponentHealth,
}

/// Health status for a single subsystem component.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentHealth {
    /// `"up"` or `"down"`.
    pub status: String,

    /// Optional diagnostic message (e.g. "last sweep: 3m ago").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Latency of the last health probe in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_us: Option<u64>,
}
