//! Domain bridge connecting MCP protocol to Recalld subsystems.
//!
//! Defines subsystem trait stubs (to be replaced with real imports during
//! assembly), domain types for MCP tool/resource handlers, and the
//! `McpBridge` struct that implements `McpHandler`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::model::MemoryId;

// ═══════════════════════════════════════════════════════════════════════
// MCP bridge dependency-injection traits
// ═══════════════════════════════════════════════════════════════════════
//
// These traits define the MCP bridge's contracts against subsystem
// dependencies. They are intentionally different from the API traits
// (different method signatures, different return types). Adapter
// implementations connecting these to real subsystems live in
// `mcp/bridge_adapters.rs`.

/// Search pipeline interface.
#[async_trait]
pub trait SearchPipeline: Send + Sync {
    /// Execute a semantic search query.
    async fn search(&self, query: SearchInput) -> Result<SearchResponse, BridgeError>;

    /// Find memories similar to an existing memory by its ID.
    async fn find_similar(
        &self,
        id: MemoryId,
        limit: usize,
        min_score: Option<f32>,
        same_namespace: bool,
    ) -> Result<Vec<SearchHit>, BridgeError>;

    /// Scan a namespace for clusters of near-duplicate memories.
    ///
    /// Samples up to `max_memories` from the namespace, runs pairwise
    /// similarity checks, and groups memories whose similarity exceeds
    /// `threshold` into clusters.
    async fn scan_duplicates(
        &self,
        namespace: &str,
        threshold: f32,
        max_memories: usize,
    ) -> Result<Vec<DuplicateCluster>, BridgeError>;
}

/// Storage engine interface.
#[async_trait]
pub trait StorageEngine: Send + Sync {
    /// Store a new memory and return a summary of the stored record.
    async fn store_memory(&self, input: StoreInput) -> Result<StoredMemory, BridgeError>;

    /// Retrieve a full memory record by ID, or `None` if not found.
    async fn get_memory(&self, id: MemoryId) -> Result<Option<MemoryRecord>, BridgeError>;

    /// Delete a memory by ID. Returns `true` if the memory existed and was deleted.
    async fn delete_memory(&self, id: MemoryId) -> Result<bool, BridgeError>;

    /// Reinforce a memory with the given FSRS quality rating (1-4).
    async fn reinforce_memory(
        &self,
        id: MemoryId,
        quality: u8,
    ) -> Result<ReinforceResult, BridgeError>;

    /// List memories in a namespace with pagination and optional filters.
    async fn list_memories(
        &self,
        input: ListMemoriesInput,
    ) -> Result<ListMemoriesResponse, BridgeError>;
}

/// Namespace registry interface.
#[async_trait]
pub trait NamespaceRegistry: Send + Sync {
    /// List all namespaces.
    async fn list_namespaces(&self) -> Result<Vec<NamespaceInfo>, BridgeError>;

    /// Create a new namespace.
    async fn create_namespace(
        &self,
        input: CreateNamespaceInput,
    ) -> Result<NamespaceInfo, BridgeError>;

    /// Get detailed statistics for a namespace by name.
    async fn namespace_stats(&self, name: &str) -> Result<NamespaceStats, BridgeError>;
}

/// Health check interface.
#[async_trait]
pub trait HealthChecker: Send + Sync {
    /// Check the health of all subsystems.
    async fn check_health(&self) -> HealthStatus;
}

// ═══════════════════════════════════════════════════════════════════════
// Domain types
// ═══════════════════════════════════════════════════════════════════════

/// Input for a search operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchInput {
    /// Natural language search query.
    pub query: String,
    /// Namespace to search within.
    pub namespace: String,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Tags to filter results by.
    pub tags: Vec<String>,
    /// Entity names to filter results by.
    pub entities: Vec<String>,
    /// Topic keywords to filter results by.
    pub topics: Vec<String>,
    /// Emotional tones to filter results by.
    pub emotions: Vec<String>,
    /// Minimum memory strength threshold.
    pub min_strength: Option<f32>,
    /// Number of graph hops for related memories.
    pub depth: u32,
    /// Lower bound timestamp in milliseconds since epoch.
    pub time_range_start: Option<i64>,
    /// Upper bound timestamp in milliseconds since epoch.
    pub time_range_end: Option<i64>,
}

/// A relationship edge to another memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedMemory {
    /// Memory ID of the related memory.
    pub id: String,
    /// Type of graph edge connecting the memories.
    pub edge_type: String,
    /// Edge weight indicating relationship strength.
    pub weight: f32,
}

/// A graph neighbor of a search result, not itself a result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeighborMemory {
    /// Memory ID of the neighbor.
    pub id: String,
    /// Short summary of the neighbor memory.
    pub summary: String,
    /// Full text content, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,
    /// Topic keywords associated with this neighbor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    /// Emotional tones associated with this neighbor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emotions: Vec<String>,
    /// Type of graph edge connecting to the search result.
    pub edge_type: String,
    /// Edge weight indicating relationship strength.
    pub weight: f32,
    /// ID of the search result this neighbor is connected to.
    pub connected_to: String,
}

/// Full search response including neighbor context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// Direct search result hits.
    pub hits: Vec<SearchHit>,
    /// Graph neighbors of the search results.
    pub neighbors: Vec<NeighborMemory>,
}

/// A single search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// Memory ID.
    pub id: String,
    /// Short summary of the memory.
    pub summary: String,
    /// Full text content, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,
    /// Combined relevance score.
    pub score: f32,
    /// Namespace the memory belongs to.
    pub namespace: String,
    /// Tags associated with this memory.
    pub tags: Vec<String>,
    /// Named entities mentioned in this memory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
    /// Topic keywords for this memory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    /// Emotional tones for this memory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emotions: Vec<String>,
    /// Current decay phase (Full, Summary, or Ghost).
    pub phase: String,
    /// Current memory strength (0.0-1.0).
    pub strength: f32,
    /// Creation timestamp as ISO 8601 string.
    pub created_at: String,
    /// Last access timestamp as ISO 8601 string.
    pub last_accessed_at: String,
    /// Related memories connected by graph edges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedMemory>,
}

/// A cluster of near-duplicate memories found during namespace scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateCluster {
    /// Summaries of the memories in this cluster, each with its ID and similarity score.
    pub memories: Vec<DuplicateEntry>,
    /// The highest pairwise similarity score within this cluster.
    pub max_similarity: f32,
}

/// A single memory entry within a duplicate cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateEntry {
    /// Memory ID.
    pub id: String,
    /// Short summary of the memory.
    pub summary: String,
}

/// Input for storing a new memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreInput {
    /// Short description of the memory.
    pub summary: String,
    /// Detailed content, removed when memory decays to ghost phase.
    pub full_text: Option<String>,
    /// Categorization tags.
    pub tags: Vec<String>,
    /// Named entities mentioned in the memory.
    #[serde(default)]
    pub entities: Vec<String>,
    /// Topic keywords.
    #[serde(default)]
    pub topics: Vec<String>,
    /// Emotional tones.
    #[serde(default)]
    pub emotions: Vec<String>,
    /// Target namespace.
    pub namespace: String,
    /// Pre-computed embedding vector, if available.
    pub embedding: Option<Vec<f32>>,
    /// Initial stability in days for the new memory.
    pub initial_stability: Option<f32>,
    /// Parent memory ID for hierarchical linking.
    pub parent_id: Option<MemoryId>,
    /// ID of an older memory this one replaces.
    pub supersedes: Option<MemoryId>,
}

/// Result of storing a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMemory {
    /// Assigned memory ID.
    pub id: String,
    /// Namespace the memory was stored in.
    pub namespace: String,
    /// Initial decay phase.
    pub phase: String,
    /// Initial memory strength.
    pub strength: f32,
    /// Initial stability in days.
    pub stability: f32,
    /// Creation timestamp as ISO 8601 string.
    pub created_at: String,
}

/// A full memory record returned by get.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    /// Memory ID.
    pub id: String,
    /// Namespace the memory belongs to.
    pub namespace: String,
    /// Short summary of the memory.
    pub summary: String,
    /// Full text content, if available.
    pub full_text: Option<String>,
    /// Tags associated with this memory.
    pub tags: Vec<String>,
    /// Current decay phase.
    pub phase: String,
    /// Current memory strength (0.0-1.0).
    pub strength: f32,
    /// Current stability in days.
    pub stability: f32,
    /// Creation timestamp as ISO 8601 string.
    pub created_at: String,
    /// Last access timestamp as ISO 8601 string.
    pub last_accessed_at: String,
    /// Whether this memory is in permastore.
    pub is_permastore: bool,
    /// Number of graph edges connected to this memory.
    pub edge_count: u16,
}

/// Result of reinforcing a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReinforceResult {
    /// Memory ID that was reinforced.
    pub id: String,
    /// Updated memory strength.
    pub strength: f32,
    /// Updated stability in days.
    pub stability: f32,
    /// Current decay phase after reinforcement.
    pub phase: String,
    /// Whether the memory is in permastore.
    pub is_permastore: bool,
}

/// Input for listing memories with pagination and filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMemoriesInput {
    /// Namespace to list memories from.
    pub namespace: String,
    /// Maximum number of results to return (max 200).
    pub limit: usize,
    /// Number of results to skip for pagination.
    pub offset: usize,
    /// Only return memories with ALL of these tags (AND semantics).
    pub tags: Vec<String>,
    /// Only return memories mentioning ALL of these entities.
    pub entities: Vec<String>,
    /// Lower bound timestamp in milliseconds since epoch.
    pub time_range_start: Option<i64>,
    /// Upper bound timestamp in milliseconds since epoch.
    pub time_range_end: Option<i64>,
}

/// A single memory entry in a list response (lightweight, no full_text).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMemoryEntry {
    /// Memory ID.
    pub id: String,
    /// Short summary of the memory.
    pub summary: String,
    /// Named entities mentioned in this memory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
    /// Topic keywords for this memory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    /// Creation timestamp as ISO 8601 string.
    pub created_at: String,
}

/// Response for list_memories.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMemoriesResponse {
    /// The memory entries for this page.
    pub memories: Vec<ListMemoryEntry>,
    /// Total number of memories matching the filters (before pagination).
    pub total: u64,
    /// The offset used for this page.
    pub offset: usize,
    /// The limit used for this page.
    pub limit: usize,
}

/// Input for creating a namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateNamespaceInput {
    /// Namespace name.
    pub name: String,
    /// Embedding dimensions, fixed after creation.
    pub embedding_dim: Option<u16>,
    /// Starting stability in days for new memories.
    pub initial_stability: Option<f32>,
    /// Target retention rate (0.0-1.0).
    pub desired_retention: Option<f32>,
    /// Decay rate multiplier for this namespace.
    /// None = inherit from global config.
    /// Some(1.0) = normal, Some(0.0) = disabled.
    pub decay_rate_multiplier: Option<f32>,
}

/// Namespace information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceInfo {
    /// Namespace numeric ID.
    pub id: u32,
    /// Namespace name.
    pub name: String,
    /// Embedding vector dimensions.
    pub embedding_dim: u16,
    /// Number of memories in this namespace.
    pub memory_count: u64,
    /// Creation timestamp as ISO 8601 string.
    pub created_at: String,
}

/// Namespace statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceStats {
    /// Namespace name.
    pub name: String,
    /// Total number of memories.
    pub memory_count: u64,
    /// Memory counts broken down by decay phase.
    pub phase_counts: PhaseCounts,
    /// Number of memories in permastore.
    pub permastore_count: u64,
    /// Average memory strength across all memories.
    pub avg_strength: f32,
    /// Total number of graph edges.
    pub edge_count: u64,
    /// Total bytes used by vector storage.
    pub vector_bytes: u64,
}

/// Memory counts by decay phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseCounts {
    /// Memories in the Full phase.
    pub full: u64,
    /// Memories in the Summary phase.
    pub summary: u64,
    /// Memories in the Ghost phase.
    pub ghost: u64,
}

/// System health status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthStatus {
    /// Overall system status (e.g. "ok" or "degraded").
    pub status: String,
    /// Uptime in seconds since server start.
    pub uptime_secs: u64,
    /// Health status of individual subsystems.
    pub subsystems: Vec<SubsystemHealth>,
}

/// Health of a single subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubsystemHealth {
    /// Subsystem name.
    pub name: String,
    /// Subsystem status (e.g. "ok" or "error").
    pub status: String,
    /// Optional status message with details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Bridge-level error.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// The requested entity was not found.
    #[error("Not found: {0}")]
    NotFound(String),

    /// The input was invalid or malformed.
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// A storage subsystem error occurred.
    #[error("Storage error: {0}")]
    Storage(String),

    /// A search subsystem error occurred.
    #[error("Search error: {0}")]
    Search(String),

    /// An unexpected internal error occurred.
    #[error("Internal error: {0}")]
    Internal(String),
}

// ═══════════════════════════════════════════════════════════════════════
// Time range value (supports both millis and ISO 8601 input)
// ═══════════════════════════════════════════════════════════════════════

/// Time range value: either raw epoch millis or an ISO 8601 string.
///
/// Uses serde's `untagged` representation so JSON callers can pass either
/// `1719187200000` or `"2024-06-24T00:00:00Z"` for the same field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TimeRangeValue {
    /// Raw Unix epoch milliseconds.
    Millis(i64),
    /// ISO 8601 string (e.g. `"2024-06-24T10:00:00Z"`).
    Iso8601(String),
}

impl TimeRangeValue {
    /// Convert to Unix epoch milliseconds.
    pub fn to_millis(&self) -> Result<i64, BridgeError> {
        match self {
            TimeRangeValue::Millis(ms) => Ok(*ms),
            TimeRangeValue::Iso8601(s) => {
                crate::time::parse_iso8601_to_millis(s).map_err(|e| BridgeError::InvalidInput(e))
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// McpBridge
// ═══════════════════════════════════════════════════════════════════════

/// Domain bridge connecting MCP protocol to Recalld subsystems.
///
/// Holds `Arc` references to every subsystem. Implements `McpHandler`
/// by delegating to the tool and resource handler functions.
pub struct McpBridge {
    /// Search pipeline for semantic queries and similarity search.
    pub search: Arc<dyn SearchPipeline>,
    /// Storage engine for CRUD operations on memories.
    pub storage: Arc<dyn StorageEngine>,
    /// Namespace registry for listing and creating namespaces.
    pub namespaces: Arc<dyn NamespaceRegistry>,
    /// Health checker for subsystem status.
    pub health: Arc<dyn HealthChecker>,
    /// Default namespace for this MCP session (from per-dir config).
    /// Set once at initialization, immutable thereafter.
    pub default_namespace: String,
    /// Resolved display timezone for formatted timestamps.
    pub timezone: chrono_tz::Tz,
}

impl McpBridge {
    /// Get the default namespace for this session.
    ///
    /// Derived from the nearest `.recalld.toml` file, or `"default"` when
    /// no per-directory config is found.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }
}

#[async_trait]
impl crate::mcp::server::McpHandler for McpBridge {
    fn tools(&self) -> Vec<crate::mcp::protocol::ToolInfo> {
        crate::mcp::tools::tool_definitions()
    }

    fn resources(&self) -> Vec<crate::mcp::protocol::ResourceInfo> {
        crate::mcp::resources::resource_definitions()
    }

    fn resource_templates(&self) -> Vec<crate::mcp::protocol::ResourceTemplate> {
        crate::mcp::resources::resource_template_definitions()
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> crate::mcp::protocol::ToolCallResult {
        crate::mcp::tools::dispatch_tool(self, name, arguments).await
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> Result<crate::mcp::protocol::ResourceReadResult, String> {
        crate::mcp::resources::dispatch_resource(self, uri).await
    }
}
