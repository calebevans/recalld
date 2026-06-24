use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::mcp::bridge::BridgeError;

// ── Wire types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DaemonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRpcError {
    pub code: i32,
    pub message: String,
}

// ── Param structs ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindSimilarParams {
    pub id: String,
    pub limit: usize,
    pub min_score: Option<f32>,
    pub same_namespace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetMemoryParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteMemoryParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReinforceParams {
    pub id: String,
    pub quality: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceStatsParams {
    pub name: String,
}

// ── Error codes ──────────────────────────────────────────────────────

pub const ERR_NOT_FOUND: i32 = -32001;
pub const ERR_INVALID_INPUT: i32 = -32602;
pub const ERR_STORAGE: i32 = -32003;
pub const ERR_SEARCH: i32 = -32004;
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
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

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

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024; // 16 MB

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
    let request = serde_json::from_slice(&buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    Ok(Some(request))
}

pub async fn write_framed_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    response: &DaemonResponse,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(response).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

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
    let response = serde_json::from_slice(&buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    Ok(Some(response))
}

pub async fn write_framed_request<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    request: &DaemonRequest,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(request).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}
