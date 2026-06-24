use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::mcp::bridge::BridgeError;

// ── Wire types ───────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request sent to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRequest {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Request identifier for correlating responses.
    pub id: u64,
    /// RPC method name.
    pub method: String,
    /// Method parameters.
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Matching request identifier.
    pub id: u64,
    /// Success payload, if the call succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error payload, if the call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DaemonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRpcError {
    /// Numeric error code (see `ERR_*` constants).
    pub code: i32,
    /// Human-readable error description.
    pub message: String,
}

// ── Param structs ────────────────────────────────────────────────────

/// Parameters for the `find_similar` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindSimilarParams {
    /// Source memory ID.
    pub id: String,
    /// Maximum number of results.
    pub limit: usize,
    /// Minimum similarity threshold.
    pub min_score: Option<f32>,
    /// Whether to restrict results to the same namespace.
    pub same_namespace: bool,
}

/// Parameters for the `get_memory` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetMemoryParams {
    /// Memory ID to retrieve.
    pub id: String,
}

/// Parameters for the `delete_memory` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteMemoryParams {
    /// Memory ID to delete.
    pub id: String,
}

/// Parameters for the `reinforce_memory` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReinforceParams {
    /// Memory ID to reinforce.
    pub id: String,
    /// Quality rating (1-4).
    pub quality: u8,
}

/// Parameters for the `namespace_stats` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceStatsParams {
    /// Namespace name to query.
    pub name: String,
}

// ── Error codes ──────────────────────────────────────────────────────

/// RPC error code: resource not found.
pub const ERR_NOT_FOUND: i32 = -32001;
/// RPC error code: invalid input parameters.
pub const ERR_INVALID_INPUT: i32 = -32602;
/// RPC error code: storage layer failure.
pub const ERR_STORAGE: i32 = -32003;
/// RPC error code: search subsystem failure.
pub const ERR_SEARCH: i32 = -32004;
/// RPC error code: internal server error.
pub const ERR_INTERNAL: i32 = -32603;

// ── BridgeError conversion ───────────────────────────────────────────

impl From<&BridgeError> for DaemonRpcError {
    fn from(e: &BridgeError) -> Self {
        match e {
            BridgeError::NotFound(msg) => DaemonRpcError {
                code: ERR_NOT_FOUND,
                message: msg.clone(),
            },
            BridgeError::InvalidInput(msg) => DaemonRpcError {
                code: ERR_INVALID_INPUT,
                message: msg.clone(),
            },
            BridgeError::Storage(msg) => DaemonRpcError {
                code: ERR_STORAGE,
                message: msg.clone(),
            },
            BridgeError::Search(msg) => DaemonRpcError {
                code: ERR_SEARCH,
                message: msg.clone(),
            },
            BridgeError::Internal(msg) => DaemonRpcError {
                code: ERR_INTERNAL,
                message: msg.clone(),
            },
        }
    }
}

impl DaemonRpcError {
    /// Converts this RPC error into the corresponding `BridgeError` variant.
    pub fn into_bridge_error(self) -> BridgeError {
        match self.code {
            ERR_NOT_FOUND => BridgeError::NotFound(self.message),
            ERR_INVALID_INPUT => BridgeError::InvalidInput(self.message),
            ERR_STORAGE => BridgeError::Storage(self.message),
            ERR_SEARCH => BridgeError::Search(self.message),
            _ => BridgeError::Internal(self.message),
        }
    }
}

// ── DaemonResponse helpers ───────────────────────────────────────────

impl DaemonResponse {
    /// Creates a successful response with the given result payload.
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Creates an error response with the given RPC error.
    pub fn error(id: u64, error: DaemonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

// ── Framing ──────────────────────────────────────────────────────────
//
// Length-prefixed JSON: [4-byte big-endian length][JSON payload].

const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1 MB

/// Maximum number of concurrent client connections the daemon will accept.
/// Additional connections are dropped immediately until an existing one closes.
pub const MAX_CONNECTIONS: u32 = 32;

/// Reads a length-prefixed JSON-RPC request from the stream.
pub async fn read_framed_message<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<DaemonRequest>> {
    let len = match reader.read_u32().await {
        Ok(n) => n as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    if len > MAX_MESSAGE_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message too large: {len} bytes (max {MAX_MESSAGE_SIZE})"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let request = serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(request))
}

/// Writes a length-prefixed JSON-RPC response to the stream.
pub async fn write_framed_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    response: &DaemonResponse,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(response)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

/// Reads a length-prefixed JSON-RPC response from the stream.
pub async fn read_framed_response<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<DaemonResponse>> {
    let len = match reader.read_u32().await {
        Ok(n) => n as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    if len > MAX_MESSAGE_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message too large: {len} bytes (max {MAX_MESSAGE_SIZE})"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let response = serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(response))
}

/// Writes a length-prefixed JSON-RPC request to the stream.
pub async fn write_framed_request<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    request: &DaemonRequest,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(request)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}
