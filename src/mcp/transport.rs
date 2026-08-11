//! Stdio transport for MCP JSON-RPC communication.
//!
//! Reads newline-delimited JSON-RPC from stdin, dispatches to the
//! MCP server, and writes responses to stdout. All non-protocol
//! output (logging, debug) goes to stderr, never stdout.
//!
//! stdio is **dual-era**, and the era is decided PER MESSAGE, not per
//! process. Under MCP 2026-07-28 an open connection is explicitly not a
//! conversation or a session: a stdio process may serve a modern client
//! that never sends `initialize` (it probes `server/discover` first and
//! carries its protocol version in every `params._meta`), a legacy client
//! that does, or both. The single `McpServer` held for the process
//! lifetime therefore holds legacy lifecycle state only; modern requests
//! never consult it.

use std::sync::Arc;

use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::mcp::McpError;
use crate::mcp::protocol::*;
use crate::mcp::server::McpServer;

/// Handle one line of stdin, returning the line to write to stdout.
///
/// Returns `Ok(None)` for blank input and for notifications, which produce
/// no response.
pub(crate) async fn process_line(
    server: &Arc<tokio::sync::Mutex<McpServer>>,
    line: &str,
) -> Result<Option<String>, McpError> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }

    let message: JsonRpcMessage = match serde_json::from_str(line) {
        Ok(msg) => msg,
        Err(e) => {
            // We have no id to respond with, but JSON-RPC still wants a
            // response -- so say so explicitly with a null id rather than
            // fabricating `0` and colliding with a real request.
            let err_resp =
                JsonRpcResponse::error(JsonRpcId::Null, PARSE_ERROR, format!("Parse error: {e}"));
            return Ok(Some(serde_json::to_string(&err_resp)?));
        }
    };

    let mut server = server.lock().await;
    match server.handle_message(message).await {
        Some(response) => Ok(Some(serde_json::to_string(&response)?)),
        None => Ok(None),
    }
}

/// Run the MCP server over stdin/stdout.
///
/// Reads one JSON-RPC message per line from stdin, dispatches to
/// `McpServer::handle_message`, and writes the response (if any)
/// as a single line to stdout.
///
/// Returns when stdin is closed (EOF) or an I/O error occurs.
pub async fn run_stdio(server: Arc<tokio::sync::Mutex<McpServer>>) -> Result<(), McpError> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    tracing::info!("MCP stdio transport started");

    while let Some(line) = lines.next_line().await? {
        if let Some(json) = process_line(&server, &line).await? {
            stdout.write_all(json.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    tracing::info!("MCP stdio transport closed (stdin EOF)");
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::server::tests::stub_handler;

    fn server() -> Arc<tokio::sync::Mutex<McpServer>> {
        Arc::new(tokio::sync::Mutex::new(McpServer::new(stub_handler())))
    }

    async fn line(server: &Arc<tokio::sync::Mutex<McpServer>>, raw: &str) -> serde_json::Value {
        let out = process_line(server, raw).await.unwrap().unwrap();
        serde_json::from_str(&out).unwrap()
    }

    #[tokio::test]
    async fn parse_error_responds_with_a_null_id() {
        let srv = server();
        let resp = line(&srv, "{not json").await;
        assert_eq!(resp["error"]["code"], PARSE_ERROR);
        assert!(resp["id"].is_null(), "expected null id, got {}", resp["id"]);
    }

    /// A modern client probes `server/discover` first, then works without
    /// ever sending `initialize` -- a stdio process is not a session.
    #[tokio::test]
    async fn modern_client_discovers_then_lists_tools_without_initialize() {
        let srv = server();

        let discover = line(
            &srv,
            r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#,
        )
        .await;
        assert_eq!(
            discover["result"]["supportedVersions"],
            serde_json::json!([PROTOCOL_VERSION])
        );

        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                    META_CLIENT_CAPABILITIES: {},
                }
            }
        })
        .to_string();
        let tools = line(&srv, &raw).await;
        assert_eq!(tools["result"]["tools"][0]["name"], "store_memory");
        assert_eq!(tools["result"]["resultType"], "complete");
        assert!(!srv.lock().await.is_initialized());
    }

    #[tokio::test]
    async fn legacy_client_initializes_then_lists_tools() {
        let srv = server();

        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "LegacyClient", "version": "1.0.0" }
            }
        })
        .to_string();
        let init = line(&srv, &raw).await;
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_ne!(init["result"]["protocolVersion"], PROTOCOL_VERSION);

        // The initialized notification produces no output.
        assert!(
            process_line(
                &srv,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .await
            .unwrap()
            .is_none()
        );

        let tools = line(&srv, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
        assert_eq!(tools["result"]["tools"][0]["name"], "store_memory");
    }

    #[tokio::test]
    async fn blank_lines_are_ignored() {
        let srv = server();
        assert!(process_line(&srv, "   ").await.unwrap().is_none());
    }
}
