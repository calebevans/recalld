//! HTTP API server for Recalld.
//!
//! Provides a JSON REST API over axum + tokio for memory CRUD,
//! search, namespace management, and operational endpoints.
//!
//! # Quick start
//!
//! ```ignore
//! use recalld::api::{serve, AppState, ApiConfig};
//!
//! let state = AppState::new(search, storage, cache, graph, decay, namespaces, metrics);
//! serve(state, ApiConfig::default()).await?;
//! ```

pub mod adapters;
mod errors;
mod handlers;
mod middleware;
mod models;
mod routes;
mod state;

pub use errors::AppError;
pub use models::*;
pub use routes::router;
pub use state::AppState;

// Re-export API-layer DI traits so downstream can implement them.
pub use state::{
    FsrsEngine, MetricsCollector, NamespaceRegistry, RecordCache,
    RelationshipGraph, SearchPipeline, StorageEngine,
};

// Re-export API-layer supporting types.
pub use state::{
    NamespaceListInfo, NamespaceStats, QueryInput, ReinforceResult,
    ResolvedSearchResult, SearchFilter, SearchQuery,
};

// Re-export real error types from their owning modules.
pub use crate::storage::StorageError;
pub use crate::embedding::EmbeddingError;
pub use crate::search::SearchError;
pub use crate::graph::GraphError;

use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

// ═══════════════════════════════════════════════════════════════════════
// ApiConfig
// ═══════════════════════════════════════════════════════════════════════

/// Server configuration, loaded from config file or environment.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Bind address. Default: `"127.0.0.1"`.
    pub bind_address: String,

    /// Listen port. Default: `7878`.
    pub port: u16,

    /// Request timeout in seconds. Default: `30`.
    /// Applied via `tower_http::timeout::TimeoutLayer`.
    pub request_timeout_secs: u64,

    /// Maximum request body size in bytes. Default: 4 MB.
    /// Applied via `axum::extract::DefaultBodyLimit`.
    pub max_body_size: usize,

    /// Enable CORS permissive mode. Default: `false`.
    /// When `true`, allows any origin (development only).
    pub cors_permissive: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind_address: "127.0.0.1".to_string(),
            port: 7878,
            request_timeout_secs: 30,
            max_body_size: 4 * 1024 * 1024, // 4 MB
            cors_permissive: false,
        }
    }
}

impl ApiConfig {
    /// Construct an `ApiConfig` from the existing
    /// [`ServerConfig`](crate::config::ServerConfig).
    ///
    /// Maps `request_timeout_ms` to seconds and `max_body_bytes` to
    /// `max_body_size`.
    pub fn from_server_config(sc: &crate::config::ServerConfig) -> Self {
        Self {
            bind_address: sc.bind_address.clone(),
            port: sc.port,
            request_timeout_secs: sc.request_timeout_ms / 1000,
            max_body_size: sc.max_body_bytes,
            cors_permissive: false,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Server Startup
// ═══════════════════════════════════════════════════════════════════════

/// Start the API server. Blocks until shutdown signal (Ctrl-C).
///
/// # Arguments
///
/// * `state` -- Shared application state with references to all subsystems.
/// * `config` -- Server bind/port/timeout configuration.
///
/// # Errors
///
/// Returns an error if the TCP listener cannot bind or the server
/// encounters an unrecoverable I/O error.
pub async fn serve(
    state: AppState,
    config: ApiConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = router(state, &config);

    let addr: SocketAddr =
        format!("{}:{}", config.bind_address, config.port).parse()?;

    let listener = TcpListener::bind(addr).await?;
    info!("Recalld API listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("API server shut down gracefully");
    Ok(())
}

/// Listens for Ctrl-C (SIGINT) to trigger graceful shutdown.
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl-C handler");
    info!("shutdown signal received");
}
