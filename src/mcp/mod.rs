//! MCP (Model Context Protocol) server for Recalld.
//!
//! Exposes Recalld memory operations as MCP tools and resources,
//! allowing AI agents (Claude Code, Cursor, etc.) to use Recalld
//! as their memory system via the standard MCP protocol.

pub mod protocol;
pub mod server;
pub mod transport;

pub mod bridge;
pub mod bridge_adapters;
pub mod resources;
pub mod tools;

pub use protocol::*;
pub use server::{McpHandler, McpServer};
pub use transport::run_stdio;

use thiserror::Error;

/// MCP subsystem errors.
#[derive(Debug, Error)]
pub enum McpError {
    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialization or deserialization error occurred.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The server has not been initialized yet.
    #[error("Server not initialized")]
    NotInitialized,

    /// The requested tool was not found.
    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    /// The requested resource was not found.
    #[error("Resource not found: {0}")]
    ResourceNotFound(String),

    /// An error in the bridge layer.
    #[error("Bridge error: {0}")]
    Bridge(String),
}
