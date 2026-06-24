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
    pub query: String,
    pub namespace: String,
    pub limit: usize,
    pub tags: Vec<String>,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
    pub min_strength: Option<f32>,
    pub depth: u32,
    pub time_range_start: Option<i64>,
    pub time_range_end: Option<i64>,
}

/// A relationship edge to another memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedMemory {
    pub id: String,
    pub edge_type: String,
    pub weight: f32,
}

/// A graph neighbor of a search result, not itself a result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeighborMemory {
    pub id: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emotions: Vec<String>,
    pub edge_type: String,
    pub weight: f32,
    pub connected_to: String,
}

/// Full search response including neighbor context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub neighbors: Vec<NeighborMemory>,
}

/// A single search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub id: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,
    pub score: f32,
    pub namespace: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emotions: Vec<String>,
    pub phase: String,
    pub strength: f32,
    pub created_at: i64,
    pub last_accessed_at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedMemory>,
}

/// Input for storing a new memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreInput {
    pub summary: String,
    pub full_text: Option<String>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub emotions: Vec<String>,
    pub namespace: String,
    pub embedding: Option<Vec<f32>>,
    pub initial_stability: Option<f32>,
    pub parent_id: Option<MemoryId>,
    pub supersedes: Option<MemoryId>,
}

/// Result of storing a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMemory {
    pub id: String,
    pub namespace: String,
    pub phase: String,
    pub strength: f32,
    pub stability: f32,
    pub created_at: i64,
}

/// A full memory record returned by get.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    pub id: String,
    pub namespace: String,
    pub summary: String,
    pub full_text: Option<String>,
    pub tags: Vec<String>,
    pub phase: String,
    pub strength: f32,
    pub stability: f32,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub is_permastore: bool,
    pub edge_count: u16,
}

/// Result of reinforcing a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReinforceResult {
    pub id: String,
    pub strength: f32,
    pub stability: f32,
    pub phase: String,
    pub is_permastore: bool,
}

/// Input for creating a namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateNamespaceInput {
    pub name: String,
    pub embedding_dim: Option<u16>,
    pub initial_stability: Option<f32>,
    pub desired_retention: Option<f32>,
}

/// Namespace information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceInfo {
    pub id: u32,
    pub name: String,
    pub embedding_dim: u16,
    pub memory_count: u64,
    pub created_at: i64,
}

/// Namespace statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceStats {
    pub name: String,
    pub memory_count: u64,
    pub phase_counts: PhaseCounts,
    pub permastore_count: u64,
    pub avg_strength: f32,
    pub edge_count: u64,
    pub vector_bytes: u64,
}

/// Memory counts by decay phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseCounts {
    pub full: u64,
    pub summary: u64,
    pub ghost: u64,
}

/// System health status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthStatus {
    pub status: String,
    pub uptime_secs: u64,
    pub subsystems: Vec<SubsystemHealth>,
}

/// Health of a single subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubsystemHealth {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Bridge-level error.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Search error: {0}")]
    Search(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

// ═══════════════════════════════════════════════════════════════════════
// McpBridge
// ═══════════════════════════════════════════════════════════════════════

/// Domain bridge connecting MCP protocol to Recalld subsystems.
///
/// Holds `Arc` references to every subsystem. Implements `McpHandler`
/// by delegating to the tool and resource handler functions.
pub struct McpBridge {
    pub search: Arc<dyn SearchPipeline>,
    pub storage: Arc<dyn StorageEngine>,
    pub namespaces: Arc<dyn NamespaceRegistry>,
    pub health: Arc<dyn HealthChecker>,
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
