//! Stdio transport for MCP JSON-RPC communication.
//!
//! Reads newline-delimited JSON-RPC from stdin, dispatches to the
//! MCP server, and writes responses to stdout. All non-protocol
//! output (logging, debug) goes to stderr, never stdout.

use std::sync::Arc;

use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::mcp::McpError;
use crate::mcp::protocol::*;
use crate::mcp::server::McpServer;

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
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let message: JsonRpcMessage = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(e) => {
                // Parse error -- we don't have an ID to respond with,
                // but JSON-RPC says we should still respond.
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
                let json = serde_json::to_string(&err_resp)?;
                stdout.write_all(json.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                continue;
            }
        };

        let mut server = server.lock().await;
        if let Some(response) = server.handle_message(message).await {
            let json = serde_json::to_string(&response)?;
            stdout.write_all(json.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
        // Notifications (id == None) produce no response.
    }

    tracing::info!("MCP stdio transport closed (stdin EOF)");
    Ok(())
}
