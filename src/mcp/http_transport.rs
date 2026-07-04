//! Streamable HTTP transport for MCP.
//!
//! Implements the MCP Streamable HTTP transport as an axum router
//! mountable alongside the REST API. Each session gets its own
//! `McpServer` instance; the underlying `McpHandler` is shared.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use dashmap::DashMap;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::mcp::protocol::*;
use crate::mcp::server::{McpHandler, McpServer};

const MCP_SESSION_ID: &str = "mcp-session-id";

#[derive(Clone)]
struct McpHttpState {
    handler: Arc<dyn McpHandler>,
    sessions: Arc<DashMap<String, Arc<Mutex<McpServer>>>>,
}

/// Build a self-contained `Router` for the MCP HTTP transport.
///
/// Handles POST and DELETE at `/mcp`. Carries its own state and
/// intentionally applies no middleware (no timeout, no CORS, no
/// request-ID) so MCP tool calls are not subject to REST API limits.
pub fn mcp_router(handler: Arc<dyn McpHandler>) -> Router {
    let state = McpHttpState {
        handler,
        sessions: Arc::new(DashMap::new()),
    };

    Router::new()
        .route("/mcp", post(handle_post).delete(handle_delete))
        .with_state(state)
}

async fn handle_post(
    State(state): State<McpHttpState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let message: JsonRpcMessage = match serde_json::from_slice(&body) {
        Ok(msg) => msg,
        Err(e) => {
            let err_resp = JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: JsonRpcId::Number(0),
                result: None,
                error: Some(JsonRpcError {
                    code: PARSE_ERROR,
                    message: format!("Parse error: {e}"),
                    data: None,
                }),
            };
            return (StatusCode::BAD_REQUEST, Json(err_resp)).into_response();
        }
    };

    if message.method == "initialize" {
        handle_initialize(state, message).await
    } else {
        handle_session_message(state, headers, message).await
    }
}

async fn handle_initialize(state: McpHttpState, message: JsonRpcMessage) -> Response {
    let session_id = Uuid::new_v4().to_string();
    let server = Arc::new(Mutex::new(McpServer::new(state.handler.clone())));

    let response = {
        let mut srv = server.lock().await;
        srv.handle_message(message).await
    };

    state.sessions.insert(session_id.clone(), server);
    tracing::info!(session_id = %session_id, "MCP HTTP session created");

    match response {
        Some(resp) => {
            let mut http_resp = Json(resp).into_response();
            if let Ok(val) = HeaderValue::from_str(&session_id) {
                http_resp.headers_mut().insert(MCP_SESSION_ID, val);
            }
            http_resp
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn handle_session_message(
    state: McpHttpState,
    headers: HeaderMap,
    message: JsonRpcMessage,
) -> Response {
    let session_id = match headers.get(MCP_SESSION_ID).and_then(|v| v.to_str().ok()) {
        Some(id) => id.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(JsonRpcResponse::error(
                    JsonRpcId::Number(0),
                    INVALID_REQUEST,
                    "Missing Mcp-Session-Id header",
                )),
            )
                .into_response();
        }
    };

    let server = match state.sessions.get(&session_id) {
        Some(entry) => entry.value().clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let response = {
        let mut srv = server.lock().await;
        srv.handle_message(message).await
    };

    match response {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn handle_delete(State(state): State<McpHttpState>, headers: HeaderMap) -> Response {
    let session_id = match headers.get(MCP_SESSION_ID).and_then(|v| v.to_str().ok()) {
        Some(id) => id.to_string(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    match state.sessions.remove(&session_id) {
        Some(_) => {
            tracing::info!(session_id = %session_id, "MCP HTTP session terminated");
            StatusCode::OK.into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
