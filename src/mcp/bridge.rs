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

    /// Delete many memories in one pass.
    ///
    /// Returns one flag per input id, in input order. `false` means the
    /// memory was missing **or** already tombstoned — the same
    /// conflation [`delete_memory`](Self::delete_memory) already makes,
    /// kept rather than replaced with a third state a caller would have
    /// to learn for the batch alone.
    async fn delete_memories(&self, ids: &[MemoryId]) -> Result<Vec<bool>, BridgeError>;

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

    /// Tag vocabulary with per-label memory counts.
    ///
    /// On `StorageEngine` rather than `NamespaceRegistry` because
    /// `list_tags` already lives on the storage traits and the tag index
    /// is storage.
    async fn list_tags(&self, input: ListTagsInput) -> Result<ListTagsResponse, BridgeError>;
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

    /// Destroy a namespace and every memory in it.
    ///
    /// **Destructive and irreversible.** There is no undo and no backup.
    /// Implementations must refuse the default namespace outright and
    /// must refuse a namespace still holding live memories unless
    /// `force` is set.
    async fn delete_namespace(
        &self,
        input: DeleteNamespaceInput,
    ) -> Result<DeleteNamespaceResult, BridgeError>;
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
    /// Whether to check the new memory against existing ones and report
    /// near-duplicates. Absent means enabled.
    ///
    /// `Option<bool>` rather than `bool` with a `default_true` function
    /// is the load-bearing choice. This struct is deserialized straight
    /// off the daemon socket, so an older client sends a payload with no
    /// `checkDuplicates` key; serde already decodes a missing
    /// `Option<T>` as `None` — as the un-attributed `embedding` and
    /// `initial_stability` beside it demonstrate — so "unset" arrives
    /// intact and means "on". A `bool` would need
    /// `#[serde(default = "…")]` and a bare `#[serde(default)]` on one
    /// would yield `false`, silently disabling the feature for every
    /// pre-existing client.
    ///
    /// The `#[serde(default)]` below is therefore redundant, and kept
    /// only so the attribute set matches [`DeleteNamespaceInput::force`],
    /// where it IS required. Do not read it as evidence that a
    /// non-`Option` field is safe without one.
    #[serde(default)]
    pub check_duplicates: Option<bool>,
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
    /// What became of the requested `supersedes` link, when one was asked
    /// for. Absent when the store named no target.
    ///
    /// `skip_serializing_if` is the load-bearing half: it keeps the key
    /// OUT of the response when there is nothing to report, so existing
    /// clients see the shape they saw before this field existed. The
    /// `default` is redundant — serde decodes a missing `Option<T>` as
    /// `None` on its own — and is kept only for symmetry with the pair
    /// below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<crate::model::SupersedesOutcome>,
    /// Existing memories that closely resemble this one.
    ///
    /// Absent when nothing crossed the threshold, when the caller passed
    /// `checkDuplicates: false`, or when the namespace's vectors are not
    /// L2-normalized. Advisory only: the memory was stored regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near_duplicates: Option<crate::model::NearDuplicateReport>,
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

/// Input for a tag-vocabulary listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTagsInput {
    /// Namespace to count within, or `None` for every namespace.
    pub namespace: Option<String>,
    /// Maximum labels returned *per bucket*. Each bucket also reports the
    /// untruncated total, so a truncated answer still states its own size.
    pub limit: usize,
}

/// One label and the number of live memories carrying it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagCount {
    /// The label. In the `entities`, `topics` and `emotions` buckets the
    /// derived prefix is **stripped**, so the value can be passed
    /// straight back to `store_memory` or `list_memories` — which is not
    /// true of the prefixed form, since those tools add the prefix
    /// themselves.
    pub name: String,
    /// Live memories carrying this label.
    pub count: u64,
}

/// One bucket of a tag listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagBucket {
    /// Distinct labels in this bucket *before* truncation, so a truncated
    /// answer still states its own size.
    pub total: u64,
    /// Whether `items` is shorter than `total`.
    pub truncated: bool,
    /// Count descending, ties broken on name ascending.
    pub items: Vec<TagCount>,
}

/// Tag vocabulary split into plain and derived buckets.
///
/// Split rather than flat because in a real corpus every memory carries
/// several `entity/` tags, so a single top-50 list is nearly all entities
/// and useless for discovering the plain tags a caller would filter on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTagsResponse {
    /// The namespace counted, or `null` when every namespace was counted.
    pub namespace: Option<String>,
    /// Live memories inspected. `null` in global scope, where the tag
    /// index is read directly and no memory records are touched.
    pub memories_counted: Option<u64>,
    /// Labels that are not derived from `entities`, `topics` or
    /// `emotions`.
    pub tags: TagBucket,
    /// Labels derived from `entities`, prefix stripped.
    pub entities: TagBucket,
    /// Labels derived from `topics`, prefix stripped.
    pub topics: TagBucket,
    /// Labels derived from `emotions`, prefix stripped.
    pub emotions: TagBucket,
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

/// Input for destroying a namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteNamespaceInput {
    /// Name of the namespace to destroy.
    pub name: String,
    /// Destroy the namespace even if it still holds live memories.
    ///
    /// `#[serde(default)]` is required: this struct is deserialized
    /// straight off the daemon socket, an older client sends a payload
    /// with no `force` key, and `bool` has no "missing" representation —
    /// so without the attribute such a call is a hard parse failure.
    /// (`Option` fields elsewhere in this module get that for free from
    /// serde and carry the attribute only for symmetry; this one does
    /// not.) Defaulting to `false` is also the safe direction — a
    /// missing key can only ever make the operation more cautious.
    #[serde(default)]
    pub force: bool,
}

/// What a namespace deletion destroyed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteNamespaceResult {
    /// Name of the namespace that was destroyed.
    pub name: String,
    /// Its numeric id. Never reused.
    pub id: u32,
    /// Always true on success; present so the response shape matches
    /// `forget_memory`.
    pub deleted: bool,
    /// Live memories destroyed.
    pub memories_deleted: u64,
    /// Tombstoned records reclaimed. These held no content — they had
    /// already been forgotten — but nothing else in the system ever
    /// removes them, so they are reported separately rather than folded
    /// into `memoriesDeleted`.
    pub tombstones_purged: u64,
    /// Graph edges removed, including edges that pointed into the
    /// namespace from outside it.
    pub edges_removed: u64,
    /// Full-text search rows removed.
    pub fts_rows_removed: u64,
    /// Whether the namespace's vector file was removed from disk.
    pub vector_file_removed: bool,
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
    /// Number of live memories. Excludes tombstoned records, and always
    /// equals `phase_counts.full + .summary + .ghost`.
    pub memory_count: u64,
    /// Number of tombstoned records still occupying a metadata row.
    ///
    /// Reported rather than silently dropped: a namespace showing one
    /// live memory where eleven were stored has ten tombstones, and an
    /// agent auditing it needs to see that rather than guess whether
    /// something was lost to a bug.
    ///
    /// `#[serde(default)]` is required here, unlike the `Option` fields
    /// elsewhere in this module: `u64` has no "missing" representation,
    /// so without the attribute a new client decoding an old daemon's
    /// response — which has no `tombstoneCount` — fails outright, which
    /// is strictly worse than the zero it reports instead.
    #[serde(default)]
    pub tombstone_count: u64,
    /// Memory counts broken down by decay phase.
    pub phase_counts: PhaseCounts,
    /// Number of live memories in permastore.
    pub permastore_count: u64,
    /// Average memory strength across the live memories.
    pub avg_strength: f32,
    /// Total number of graph edges on the live memories.
    pub edge_count: u64,
    /// Logical bytes of live vector data
    /// (`memory_count` x `embeddingDim` x 4). NOT the size of
    /// `vectors.dat`, which retains freed slots until compaction.
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

    /// The request or response exceeded the daemon's frame size limit.
    #[error("Too large: {0}")]
    TooLarge(String),

    /// The operation failed after it had already changed something, and
    /// the change is not undoable.
    ///
    /// Distinct from [`Storage`](Self::Storage) because the two demand
    /// opposite reactions from the agent reading the message: a storage
    /// failure means "nothing happened, try again or give up", and this
    /// means "something was destroyed, re-run to finish the job". A
    /// caller that renders both as a bare failure tells its user the
    /// destroyed records are still there.
    #[error("Partially applied: {0}")]
    PartiallyApplied(String),
}

impl BridgeError {
    /// Builds a [`BridgeError::TooLarge`] describing an oversized message.
    ///
    /// The payload is a plain string rather than structured fields so the
    /// message survives the `DaemonRpcError` wire round trip intact.
    pub fn too_large(size: usize, limit: usize) -> Self {
        Self::TooLarge(format!(
            "request is {size} bytes, which exceeds the {limit} byte daemon message limit. \
             Reduce fullText (max 1 MiB), shorten tags/entities/topics/emotions, \
             or split this into multiple memories."
        ))
    }
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

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::{MemoryId, SupersedesOutcome, SupersedesStatus};

    fn stored_memory_json() -> serde_json::Value {
        json!({
            "id": "0195e2c0-0000-7000-8000-000000000001",
            "namespace": "default",
            "phase": "Full",
            "strength": 1.0,
            "stability": 3.7145,
            "createdAt": "2026-08-11T12:00:00Z",
        })
    }

    /// Back-compat guard for the daemon protocol. `StoredMemory` is
    /// round-tripped through serde between client and daemon, so a new
    /// client must still decode a response from a daemon that predates
    /// the `supersedes` field. (Serde gives an `Option` field that for
    /// free; the test is here because the guarantee matters, not because
    /// an attribute is doing the work.)
    #[test]
    fn stored_memory_decodes_a_payload_without_the_supersedes_field() {
        let decoded: StoredMemory =
            serde_json::from_value(stored_memory_json()).expect("old daemon payload decodes");

        assert!(decoded.supersedes.is_none());
        assert_eq!(decoded.namespace, "default");
    }

    /// A store that named no target must serialise exactly as before, so
    /// existing clients see an unchanged response shape.
    #[test]
    fn absent_supersedes_outcome_is_omitted_from_the_output() {
        let stored: StoredMemory =
            serde_json::from_value(stored_memory_json()).expect("payload decodes");

        let encoded = serde_json::to_value(&stored).expect("encodes");
        assert!(encoded.get("supersedes").is_none());
    }

    /// The status names are part of the wire contract in both directions.
    #[test]
    fn supersedes_status_round_trips_through_its_camel_case_names() {
        let cases = [
            (SupersedesStatus::Applied, "applied"),
            (SupersedesStatus::AlreadyApplied, "alreadyApplied"),
            (SupersedesStatus::AppliedNotDurable, "appliedNotDurable"),
            (SupersedesStatus::Failed, "failed"),
        ];

        for (status, name) in cases {
            assert_eq!(serde_json::to_value(status).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<SupersedesStatus>(json!(name)).unwrap(),
                status
            );
        }
    }

    /// A populated outcome survives the daemon round trip intact.
    #[test]
    fn supersedes_outcome_round_trips_on_a_stored_memory() {
        let target = MemoryId::new();
        let mut stored: StoredMemory =
            serde_json::from_value(stored_memory_json()).expect("payload decodes");
        stored.supersedes = Some(SupersedesOutcome::with_detail(
            target,
            SupersedesStatus::AppliedNotDurable,
            "disk write failed",
        ));

        let encoded = serde_json::to_value(&stored).expect("encodes");
        assert_eq!(encoded["supersedes"]["status"], json!("appliedNotDurable"));

        let decoded: StoredMemory = serde_json::from_value(encoded).expect("decodes");
        let outcome = decoded.supersedes.expect("outcome preserved");
        assert_eq!(outcome.target, target);
        assert_eq!(outcome.status, SupersedesStatus::AppliedNotDurable);
        assert_eq!(outcome.detail.as_deref(), Some("disk write failed"));
    }

    // ── near-duplicate wire format ───────────────────────────────────

    fn store_input_json() -> serde_json::Value {
        json!({
            "summary": "a fact",
            "fullText": null,
            "tags": [],
            "namespace": "default",
            "embedding": null,
            "initialStability": null,
            "parentId": null,
            "supersedes": null,
        })
    }

    /// Response-side back-compat, matching the `supersedes` guard above: a
    /// new client must decode a response from a daemon that predates the
    /// field.
    #[test]
    fn b11_stored_memory_decodes_a_payload_without_near_duplicates() {
        let decoded: StoredMemory =
            serde_json::from_value(stored_memory_json()).expect("old daemon payload decodes");

        assert!(decoded.near_duplicates.is_none());
    }

    /// A store with nothing to report serialises exactly as before, so an
    /// existing client sees an unchanged response shape rather than a new
    /// `"nearDuplicates": null`.
    #[test]
    fn b12_an_absent_report_is_omitted_from_the_output() {
        let stored: StoredMemory =
            serde_json::from_value(stored_memory_json()).expect("payload decodes");

        let encoded = serde_json::to_value(&stored).expect("encodes");
        assert!(encoded.get("nearDuplicates").is_none());
    }

    /// The key names are the wire contract; camelCase on every level.
    #[test]
    fn b13_a_report_round_trips_with_camel_case_keys() {
        let target = MemoryId::new();
        let mut stored: StoredMemory =
            serde_json::from_value(stored_memory_json()).expect("payload decodes");
        stored.near_duplicates = crate::model::NearDuplicateReport::from_matches(
            0.85,
            vec![crate::model::NearDuplicateMatch {
                id: target,
                score: 0.93,
                summary: "an older phrasing of the same fact".to_string(),
            }],
        );

        let encoded = serde_json::to_value(&stored).expect("encodes");
        let report = &encoded["nearDuplicates"];
        // `threshold` is `f64`, so the round number stays round. `score`
        // is a genuine `f32` similarity and widens on the way out, which
        // is why only this one is compared approximately.
        assert_eq!(report["threshold"], json!(0.85));
        assert_eq!(report["matches"][0]["id"], json!(target.to_string()));
        let score = report["matches"][0]["score"]
            .as_f64()
            .expect("score is a number");
        assert!((score - 0.93).abs() < 1e-6, "{score}");
        assert_eq!(
            report["matches"][0]["summary"],
            json!("an older phrasing of the same fact")
        );

        let decoded: StoredMemory = serde_json::from_value(encoded).expect("decodes");
        let report = decoded.near_duplicates.expect("report preserved");
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].id, target);
    }

    /// The REQUEST-side guard. `StoreInput` is deserialized straight off
    /// the daemon socket, so an older client sends a payload without
    /// `checkDuplicates`, and it must decode as "unset" — which means
    /// enabled. The `Option` is what carries that: a `bool` with a bare
    /// `#[serde(default)]` would decode as `false` and silently disable
    /// the feature for every pre-existing client.
    #[test]
    fn b14_store_input_decodes_a_payload_without_check_duplicates() {
        let decoded: StoreInput =
            serde_json::from_value(store_input_json()).expect("old client payload decodes");

        assert!(decoded.check_duplicates.is_none());
        assert_eq!(decoded.summary, "a fact");
    }

    /// The executable form of the note on `check_duplicates`: serde
    /// decodes a MISSING `Option<T>` as `None` with no attribute at all.
    /// `embedding`, `initialStability`, `parentId` and `supersedes` carry
    /// no `#[serde(default)]` and are absent here.
    ///
    /// Written down because the comments used to claim the attribute was
    /// what made these decode, which would tell the next author that a
    /// non-`Option` field is equally safe without one. It is not — see
    /// `DeleteNamespaceInput::force` and `NamespaceStats::tombstone_count`,
    /// where the attribute is the only thing standing between an old
    /// payload and a hard parse failure.
    #[test]
    fn w68_a_missing_option_field_needs_no_serde_default() {
        let decoded: StoreInput = serde_json::from_value(json!({
            "summary": "a fact",
            "tags": [],
            "namespace": "default",
        }))
        .expect("a payload with every optional key absent decodes");

        assert!(decoded.embedding.is_none());
        assert!(decoded.initial_stability.is_none());
        assert!(decoded.parent_id.is_none());
        assert!(decoded.supersedes.is_none());
        assert!(decoded.full_text.is_none());
        assert!(decoded.check_duplicates.is_none());
    }

    /// The case where `#[serde(default)]` is genuinely load-bearing:
    /// `force` is a `bool`, not an `Option<bool>`, so a missing key has
    /// no representation and the decode fails outright without it. An
    /// older client sends no `force` key, and with the attribute that
    /// missing key can only ever make the operation more cautious.
    #[test]
    fn w44_delete_namespace_input_decodes_a_payload_without_force() {
        let decoded: DeleteNamespaceInput = serde_json::from_value(json!({ "name": "scratch" }))
            .expect("old client payload decodes");

        assert_eq!(decoded.name, "scratch");
        assert!(
            !decoded.force,
            "a missing force key must mean the safe path"
        );

        let decoded: DeleteNamespaceInput =
            serde_json::from_value(json!({ "name": "scratch", "force": true })).expect("decodes");
        assert!(decoded.force);
    }

    #[test]
    fn b15_check_duplicates_round_trips_on_a_store_input() {
        let mut payload = store_input_json();
        payload["checkDuplicates"] = json!(false);

        let decoded: StoreInput = serde_json::from_value(payload).expect("decodes");
        assert_eq!(decoded.check_duplicates, Some(false));

        let encoded = serde_json::to_value(&decoded).expect("encodes");
        assert_eq!(encoded["checkDuplicates"], json!(false));
    }

    // ── namespace_stats wire format ──────────────────────────────────

    /// Back-compat guard for the daemon protocol. `NamespaceStats` is
    /// round-tripped through serde in BOTH directions, so a new client
    /// must still decode a response from a daemon that predates
    /// `tombstoneCount`. `#[serde(default)]` really is doing the work
    /// here — `tombstone_count` is a `u64`, which has no "missing"
    /// representation — so without it the call fails outright.
    #[test]
    fn namespace_stats_decodes_a_payload_without_the_tombstone_count() {
        let decoded: NamespaceStats = serde_json::from_value(json!({
            "name": "default",
            "memoryCount": 3,
            "phaseCounts": { "full": 3, "summary": 0, "ghost": 0 },
            "permastoreCount": 0,
            "avgStrength": 1.0,
            "edgeCount": 0,
            "vectorBytes": 128,
        }))
        .expect("old daemon payload decodes");

        assert_eq!(decoded.tombstone_count, 0);
        assert_eq!(decoded.memory_count, 3);
    }

    #[test]
    fn namespace_stats_round_trips_the_tombstone_count() {
        let stats = NamespaceStats {
            name: "default".into(),
            memory_count: 1,
            tombstone_count: 10,
            phase_counts: PhaseCounts {
                full: 1,
                summary: 0,
                ghost: 0,
            },
            permastore_count: 0,
            avg_strength: 0.8,
            edge_count: 0,
            vector_bytes: 16,
        };

        let encoded = serde_json::to_value(&stats).expect("encodes");
        assert_eq!(encoded["tombstoneCount"], json!(10));

        let decoded: NamespaceStats = serde_json::from_value(encoded).expect("decodes");
        assert_eq!(decoded.tombstone_count, 10);
        assert_eq!(decoded.memory_count, 1);
    }

    // ── list_tags wire format ────────────────────────────────────────

    fn bucket(items: &[(&str, u64)], total: u64, truncated: bool) -> TagBucket {
        TagBucket {
            total,
            truncated,
            items: items
                .iter()
                .map(|(n, c)| TagCount {
                    name: (*n).to_string(),
                    count: *c,
                })
                .collect(),
        }
    }

    fn empty_bucket() -> TagBucket {
        bucket(&[], 0, false)
    }

    #[test]
    fn list_tags_response_round_trips_in_camel_case() {
        let response = ListTagsResponse {
            namespace: Some("work".into()),
            memories_counted: Some(128),
            tags: bucket(&[("type/decision", 12)], 41, false),
            entities: bucket(&[("recalld", 30)], 120, true),
            topics: empty_bucket(),
            emotions: empty_bucket(),
        };

        let encoded = serde_json::to_value(&response).expect("encodes");
        assert_eq!(encoded["namespace"], json!("work"));
        assert_eq!(encoded["memoriesCounted"], json!(128));
        assert_eq!(encoded["entities"]["truncated"], json!(true));
        assert_eq!(encoded["entities"]["total"], json!(120));
        assert_eq!(encoded["entities"]["items"][0]["name"], json!("recalld"));
        assert_eq!(encoded["entities"]["items"][0]["count"], json!(30));

        let decoded: ListTagsResponse = serde_json::from_value(encoded).expect("decodes");
        assert_eq!(decoded.tags.items[0].name, "type/decision");
        assert_eq!(decoded.memories_counted, Some(128));
    }

    /// Global scope emits EXPLICIT nulls rather than omitting the keys,
    /// so the response always states its own scope instead of leaving the
    /// caller to infer it from an absence.
    #[test]
    fn list_tags_response_states_global_scope_with_explicit_nulls() {
        let response = ListTagsResponse {
            namespace: None,
            memories_counted: None,
            tags: empty_bucket(),
            entities: empty_bucket(),
            topics: empty_bucket(),
            emotions: empty_bucket(),
        };

        let encoded = serde_json::to_value(&response).expect("encodes");
        assert_eq!(encoded["namespace"], json!(null));
        assert_eq!(encoded["memoriesCounted"], json!(null));
        let obj = encoded.as_object().expect("object");
        assert!(obj.contains_key("namespace"));
        assert!(obj.contains_key("memoriesCounted"));
    }

    #[test]
    fn list_tags_input_round_trips_a_global_scope() {
        let input = ListTagsInput {
            namespace: None,
            limit: 50,
        };
        let encoded = serde_json::to_value(&input).expect("encodes");
        assert_eq!(encoded["namespace"], json!(null));

        let decoded: ListTagsInput = serde_json::from_value(encoded).expect("decodes");
        assert_eq!(decoded.namespace, None);
        assert_eq!(decoded.limit, 50);
    }
}
