//! JSON-RPC 2.0 and MCP message types.
//!
//! All wire-format types for the Model Context Protocol, including
//! lifecycle, tool, and resource messages.

use serde::{Deserialize, Serialize};

// ── Constants ───────────────────────────────────────────────────────

/// MCP protocol version this server implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// JSON-RPC version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Server name reported in initialize response.
pub const SERVER_NAME: &str = "recalld";

/// Server version reported in initialize response.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── JSON-RPC error codes ────────────────────────────────────────────

/// JSON-RPC parse error code.
pub const PARSE_ERROR: i32 = -32700;
/// JSON-RPC invalid request error code.
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC method not found error code.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC invalid params error code.
pub const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC internal error code.
pub const INTERNAL_ERROR: i32 = -32603;

// ── Core JSON-RPC Types ─────────────────────────────────────────────

/// JSON-RPC 2.0 request or notification.
///
/// Notifications have `id: None`. Requests have `id: Some(...)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcMessage {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Request ID, or `None` for notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
    /// Method name to invoke.
    pub method: String,
    /// Method parameters, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC message ID -- either an integer or a string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric request ID.
    Number(i64),
    /// String request ID.
    String(String),
}

/// JSON-RPC 2.0 success or error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Request ID this response corresponds to.
    pub id: JsonRpcId,
    /// Success result payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error object, present on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Additional error data, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    /// Build a success response.
    pub fn success(id: JsonRpcId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response.
    pub fn error(id: JsonRpcId, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// ── MCP Lifecycle Types ─────────────────────────────────────────────

/// Parameters for `initialize` request (client -> server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Requested MCP protocol version.
    pub protocol_version: String,
    /// Client capability declarations.
    pub capabilities: ClientCapabilities,
    /// Client name and version.
    pub client_info: Implementation,
}

/// Result for `initialize` response (server -> client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Negotiated MCP protocol version.
    pub protocol_version: String,
    /// Server capability declarations.
    pub capabilities: ServerCapabilities,
    /// Server name and version.
    pub server_info: Implementation,
    /// Optional usage instructions for the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Name and version of a client or server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    /// Name of the client or server.
    pub name: String,
    /// Version string.
    pub version: String,
}

/// Client capabilities (we accept but don't inspect these).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Unstructured capability fields from the client.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Server capabilities declared during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Tool capabilities, if the server exposes tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    /// Resource capabilities, if the server exposes resources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
}

/// Capability flags for tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// Whether the server supports `tools/list_changed` notifications.
    #[serde(default)]
    pub list_changed: bool,
}

/// Capability flags for resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    /// Whether the server supports resource subscriptions.
    #[serde(default)]
    pub subscribe: bool,
    /// Whether the server supports `resources/list_changed` notifications.
    #[serde(default)]
    pub list_changed: bool,
}

// ── Tool Protocol Types ─────────────────────────────────────────────

/// Tool definition returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    /// Tool name used in `tools/call`.
    pub name: String,
    /// Human-readable title for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
    /// Behavioral hints for tool UIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Behavioral hints for tool UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// Whether the tool only reads data without side effects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// Whether the tool performs destructive operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Whether repeated calls with the same input are safe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// Whether the tool interacts with external systems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// Parameters for `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallParams {
    /// Name of the tool to call.
    pub name: String,
    /// Tool input arguments.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Result of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    /// Content blocks in the result.
    pub content: Vec<ContentBlock>,
    /// Set to `true` if the tool call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A content block in a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    /// Plain text content.
    Text { text: String },
}

impl ToolCallResult {
    /// Build a success result with a single text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            is_error: None,
        }
    }

    /// Build a JSON result (serialized to text).
    pub fn json(value: &impl Serialize) -> Result<Self, serde_json::Error> {
        let text = serde_json::to_string_pretty(value)?;
        Ok(Self::text(text))
    }

    /// Build an error result.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text {
                text: message.into(),
            }],
            is_error: Some(true),
        }
    }
}

// ── Resource Protocol Types ─────────────────────────────────────────

/// Resource definition returned by `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    /// Resource URI.
    pub uri: String,
    /// Human-readable resource name.
    pub name: String,
    /// Description of the resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type of the resource content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Resource template returned by `resources/templates/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    /// URI template with `{param}` placeholders.
    pub uri_template: String,
    /// Human-readable template name.
    pub name: String,
    /// Description of the resource template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type of the resource content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Parameters for `resources/read`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReadParams {
    /// URI of the resource to read.
    pub uri: String,
}

/// Result of a resource read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReadResult {
    /// Resource content blobs.
    pub contents: Vec<ResourceContent>,
}

/// A single resource content blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContent {
    /// URI identifying this content.
    pub uri: String,
    /// MIME type of the content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Text content of the resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// ── List Response Wrappers ──────────────────────────────────────────

/// Response wrapper for `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    /// List of available tools.
    pub tools: Vec<ToolInfo>,
}

/// Response wrapper for `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcesListResult {
    /// List of available resources.
    pub resources: Vec<ResourceInfo>,
}

/// Response wrapper for `resources/templates/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceTemplatesListResult {
    /// List of available resource templates.
    pub resource_templates: Vec<ResourceTemplate>,
}
