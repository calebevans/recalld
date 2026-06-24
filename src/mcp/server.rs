//! MCP server with method dispatch and lifecycle management.
//!
//! The `McpServer` manages initialization state and dispatches
//! JSON-RPC methods to the `McpHandler` trait, which is implemented
//! by the domain layer (CS-22).

use std::sync::Arc;

use async_trait::async_trait;

use crate::mcp::protocol::*;

// ── McpHandler Trait ────────────────────────────────────────────────

/// Handler for MCP tool calls and resource reads.
///
/// Implemented by `McpBridge` in CS-22. The server holds a
/// `Arc<dyn McpHandler>` and delegates all domain logic to it.
#[async_trait]
pub trait McpHandler: Send + Sync {
    /// Return the list of tools this server exposes.
    fn tools(&self) -> Vec<ToolInfo>;

    /// Return the list of static resources.
    fn resources(&self) -> Vec<ResourceInfo>;

    /// Return the list of resource URI templates.
    fn resource_templates(&self) -> Vec<ResourceTemplate>;

    /// Execute a tool call and return the result.
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> ToolCallResult;

    /// Read a resource by URI and return its content.
    async fn read_resource(&self, uri: &str) -> Result<ResourceReadResult, String>;
}

// ── McpServer ───────────────────────────────────────────────────────

/// MCP protocol server.
///
/// Manages lifecycle state (initialized/not) and dispatches
/// JSON-RPC methods to the appropriate handler. Stateless
/// between requests except for the `initialized` flag.
pub struct McpServer {
    handler: Arc<dyn McpHandler>,
    initialized: bool,
}

impl McpServer {
    /// Create a new MCP server with the given handler.
    pub fn new(handler: Arc<dyn McpHandler>) -> Self {
        Self {
            handler,
            initialized: false,
        }
    }

    /// Dispatch a JSON-RPC message to the appropriate handler.
    ///
    /// Returns `Some(response)` for requests, `None` for notifications.
    pub async fn handle_message(&mut self, msg: JsonRpcMessage) -> Option<JsonRpcResponse> {
        let id = msg.id.clone();

        // Notifications (no id) don't get a response.
        let id = match id {
            Some(id) => id,
            None => {
                self.handle_notification(&msg.method, msg.params).await;
                return None;
            }
        };

        // Before initialization, only `initialize` and `ping` are allowed.
        if !self.initialized && msg.method != "initialize" && msg.method != "ping" {
            return Some(JsonRpcResponse::error(
                id,
                INVALID_REQUEST,
                "Server not initialized. Send 'initialize' first.",
            ));
        }

        let result = match msg.method.as_str() {
            "initialize" => self.handle_initialize(msg.params).await,
            "ping" => self.handle_ping().await,
            "tools/list" => self.handle_tools_list().await,
            "tools/call" => self.handle_tools_call(msg.params).await,
            "resources/list" => self.handle_resources_list().await,
            "resources/templates/list" => self.handle_resource_templates_list().await,
            "resources/read" => self.handle_resources_read(msg.params).await,
            other => Err(DispatchError::MethodNotFound(other.to_string())),
        };

        Some(match result {
            Ok(value) => JsonRpcResponse::success(id, value),
            Err(e) => JsonRpcResponse::error(id, e.code(), e.to_string()),
        })
    }

    /// Handle a notification (no response expected).
    async fn handle_notification(&mut self, method: &str, _params: Option<serde_json::Value>) {
        match method {
            "notifications/initialized" => {
                tracing::info!("Client confirmed initialization");
            }
            "notifications/cancelled" => {
                tracing::debug!("Client cancelled a request");
            }
            other => {
                tracing::debug!(method = other, "Unknown notification, ignoring");
            }
        }
    }

    async fn handle_initialize(
        &mut self,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, DispatchError> {
        let params: InitializeParams = params
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| DispatchError::InvalidParams(e.to_string()))?
            .unwrap_or_else(|| InitializeParams {
                protocol_version: PROTOCOL_VERSION.to_string(),
                capabilities: ClientCapabilities::default(),
                client_info: Implementation {
                    name: "unknown".to_string(),
                    version: "0.0.0".to_string(),
                },
            });

        tracing::info!(
            client = %params.client_info.name,
            version = %params.client_info.version,
            protocol = %params.protocol_version,
            "MCP client initializing"
        );

        self.initialized = true;

        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability { list_changed: false }),
                resources: Some(ResourcesCapability {
                    subscribe: false,
                    list_changed: false,
                }),
            },
            server_info: Implementation {
                name: SERVER_NAME.to_string(),
                version: SERVER_VERSION.to_string(),
            },
            instructions: Some(
                "Recalld is an AI memory system with human-like forgetting. \
                 Use store_memory to save observations, recall_memories to \
                 search by semantic similarity, and reinforce_memory to \
                 strengthen useful memories. Memories decay naturally over \
                 time unless reinforced."
                    .to_string(),
            ),
        };

        Ok(serde_json::to_value(result)?)
    }

    async fn handle_ping(&self) -> Result<serde_json::Value, DispatchError> {
        Ok(serde_json::json!({}))
    }

    async fn handle_tools_list(&self) -> Result<serde_json::Value, DispatchError> {
        let result = ToolsListResult {
            tools: self.handler.tools(),
        };
        Ok(serde_json::to_value(result)?)
    }

    async fn handle_tools_call(
        &self,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, DispatchError> {
        let params: ToolCallParams = params
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| DispatchError::InvalidParams(e.to_string()))?
            .ok_or_else(|| DispatchError::InvalidParams("Missing params".to_string()))?;

        let result = self.handler.call_tool(&params.name, params.arguments).await;
        Ok(serde_json::to_value(result)?)
    }

    async fn handle_resources_list(&self) -> Result<serde_json::Value, DispatchError> {
        let result = ResourcesListResult {
            resources: self.handler.resources(),
        };
        Ok(serde_json::to_value(result)?)
    }

    async fn handle_resource_templates_list(&self) -> Result<serde_json::Value, DispatchError> {
        let result = ResourceTemplatesListResult {
            resource_templates: self.handler.resource_templates(),
        };
        Ok(serde_json::to_value(result)?)
    }

    async fn handle_resources_read(
        &self,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, DispatchError> {
        let params: ResourceReadParams = params
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| DispatchError::InvalidParams(e.to_string()))?
            .ok_or_else(|| DispatchError::InvalidParams("Missing params".to_string()))?;

        let result = self
            .handler
            .read_resource(&params.uri)
            .await
            .map_err(DispatchError::Internal)?;
        Ok(serde_json::to_value(result)?)
    }
}

// ── DispatchError ───────────────────────────────────────────────────

/// Internal dispatch errors mapped to JSON-RPC error codes.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("Method not found: {0}")]
    MethodNotFound(String),

    #[error("Invalid params: {0}")]
    InvalidParams(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl DispatchError {
    /// Map to a JSON-RPC error code.
    pub fn code(&self) -> i32 {
        match self {
            Self::MethodNotFound(_) => METHOD_NOT_FOUND,
            Self::InvalidParams(_) => INVALID_PARAMS,
            Self::Internal(_) | Self::Serialization(_) => INTERNAL_ERROR,
        }
    }
}
