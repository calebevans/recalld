use std::sync::Arc;

use async_trait::async_trait;

use super::client::DaemonClient;
use super::protocol;
use crate::mcp::bridge::{
    self, BridgeError, CreateNamespaceInput, DuplicateCluster, HealthStatus, MemoryRecord,
    NamespaceInfo, NamespaceStats, ReinforceResult, SearchHit, SearchInput, SearchPipeline,
    SearchResponse, StoreInput, StoredMemory, SubsystemHealth,
};
use crate::model::MemoryId;

// ═══════════════════════════════════════════════════════════════════════
// RemoteSearchAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Delegates search operations to the daemon over a Unix socket.
pub struct RemoteSearchAdapter {
    client: Arc<DaemonClient>,
}

impl RemoteSearchAdapter {
    /// Creates a new adapter backed by the given daemon client.
    pub fn new(client: Arc<DaemonClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl SearchPipeline for RemoteSearchAdapter {
    async fn search(&self, query: SearchInput) -> Result<SearchResponse, BridgeError> {
        let params =
            serde_json::to_value(&query).map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("search", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn find_similar(
        &self,
        id: MemoryId,
        limit: usize,
        min_score: Option<f32>,
        same_namespace: bool,
    ) -> Result<Vec<SearchHit>, BridgeError> {
        let params = serde_json::to_value(protocol::FindSimilarParams {
            id: id.to_string(),
            limit,
            min_score,
            same_namespace,
        })
        .map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("find_similar", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn scan_duplicates(
        &self,
        namespace: &str,
        threshold: f32,
        max_memories: usize,
    ) -> Result<Vec<DuplicateCluster>, BridgeError> {
        let params = serde_json::to_value(protocol::ScanDuplicatesParams {
            namespace: namespace.to_string(),
            threshold,
            max_memories,
        })
        .map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("scan_duplicates", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RemoteStorageAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Delegates storage operations to the daemon over a Unix socket.
pub struct RemoteStorageAdapter {
    client: Arc<DaemonClient>,
}

impl RemoteStorageAdapter {
    /// Creates a new adapter backed by the given daemon client.
    pub fn new(client: Arc<DaemonClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl bridge::StorageEngine for RemoteStorageAdapter {
    async fn store_memory(&self, input: StoreInput) -> Result<StoredMemory, BridgeError> {
        let params =
            serde_json::to_value(&input).map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("store_memory", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn get_memory(&self, id: MemoryId) -> Result<Option<MemoryRecord>, BridgeError> {
        let params = serde_json::to_value(protocol::GetMemoryParams { id: id.to_string() })
            .map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("get_memory", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn delete_memory(&self, id: MemoryId) -> Result<bool, BridgeError> {
        let params = serde_json::to_value(protocol::DeleteMemoryParams { id: id.to_string() })
            .map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("delete_memory", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn reinforce_memory(
        &self,
        id: MemoryId,
        quality: u8,
    ) -> Result<ReinforceResult, BridgeError> {
        let params = serde_json::to_value(protocol::ReinforceParams {
            id: id.to_string(),
            quality,
        })
        .map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("reinforce_memory", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn list_memories(
        &self,
        input: bridge::ListMemoriesInput,
    ) -> Result<bridge::ListMemoriesResponse, BridgeError> {
        let params =
            serde_json::to_value(&input).map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("list_memories", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RemoteNamespaceAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Delegates namespace operations to the daemon over a Unix socket.
pub struct RemoteNamespaceAdapter {
    client: Arc<DaemonClient>,
}

impl RemoteNamespaceAdapter {
    /// Creates a new adapter backed by the given daemon client.
    pub fn new(client: Arc<DaemonClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl bridge::NamespaceRegistry for RemoteNamespaceAdapter {
    async fn list_namespaces(&self) -> Result<Vec<NamespaceInfo>, BridgeError> {
        let result = self
            .client
            .call("list_namespaces", serde_json::Value::Null)
            .await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn create_namespace(
        &self,
        input: CreateNamespaceInput,
    ) -> Result<NamespaceInfo, BridgeError> {
        let params =
            serde_json::to_value(&input).map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("create_namespace", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }

    async fn namespace_stats(&self, name: &str) -> Result<NamespaceStats, BridgeError> {
        let params = serde_json::to_value(protocol::NamespaceStatsParams {
            name: name.to_string(),
        })
        .map_err(|e| BridgeError::Internal(e.to_string()))?;
        let result = self.client.call("namespace_stats", params).await?;
        serde_json::from_value(result)
            .map_err(|e| BridgeError::Internal(format!("response decode: {e}")))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RemoteHealthAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Delegates health checks to the daemon over a Unix socket.
pub struct RemoteHealthAdapter {
    client: Arc<DaemonClient>,
}

impl RemoteHealthAdapter {
    /// Creates a new adapter backed by the given daemon client.
    pub fn new(client: Arc<DaemonClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl bridge::HealthChecker for RemoteHealthAdapter {
    async fn check_health(&self) -> HealthStatus {
        match self
            .client
            .call("check_health", serde_json::Value::Null)
            .await
        {
            Ok(result) => serde_json::from_value(result).unwrap_or_else(|e| HealthStatus {
                status: "degraded".to_string(),
                uptime_secs: 0,
                subsystems: vec![SubsystemHealth {
                    name: "daemon".to_string(),
                    status: "error".to_string(),
                    message: Some(format!("response decode: {e}")),
                }],
            }),
            Err(e) => HealthStatus {
                status: "degraded".to_string(),
                uptime_secs: 0,
                subsystems: vec![SubsystemHealth {
                    name: "daemon".to_string(),
                    status: "error".to_string(),
                    message: Some(e.to_string()),
                }],
            },
        }
    }
}
