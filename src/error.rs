//! Unified error type for Recalld.
//!
//! Subsystem errors are defined in their own modules (e.g., `StorageError`
//! in `storage::error`, `GraphError` in `graph::structure`). This module
//! provides `RecalldError`, the top-level enum that wraps all subsystem
//! errors and adds system-level variants (config, shutdown, init).
//!
//! **Convention**: Subsystem code returns its own error type. The system
//! layer (`system.rs`, `api/handlers.rs`) converts to `RecalldError`
//! via the `From` impls below. API handlers then map `RecalldError`
//! to HTTP status codes.

use std::path::PathBuf;
use thiserror::Error;

use crate::storage::error::StorageError;
use crate::embedding::EmbeddingError;
use crate::search::error::SearchError;
use crate::decay::sweep::SweepError;
use crate::config::ConfigError;

/// Top-level error for Recalld operations.
///
/// Each variant wraps a subsystem error. The `#[from]` attribute on
/// thiserror generates `impl From<SubsystemError> for RecalldError`,
/// enabling `?` propagation from subsystem calls.
#[derive(Debug, Error)]
pub enum RecalldError {
    // ── Subsystem wrappers ───────────────────────────────────────

    /// Storage layer error.
    #[error("storage: {0}")]
    Storage(#[from] StorageError),

    /// Embedding provider error.
    #[error("embedding: {0}")]
    Embedding(#[from] EmbeddingError),

    /// Search pipeline error.
    #[error("search: {0}")]
    Search(#[from] SearchError),

    /// Decay sweep error.
    #[error("decay sweep: {0}")]
    Sweep(#[from] SweepError),

    /// Configuration error.
    #[error("config: {0}")]
    Config(#[from] ConfigError),

    // ── System-level errors ──────────────────────────────────────

    /// Initialization failed at a named step.
    #[error("initialization failed at step '{step}': {message}")]
    Init {
        /// The startup step that failed.
        step: &'static str,
        /// Description of the failure.
        message: String,
        /// The underlying error, if any.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Shutdown encountered an error.
    #[error("shutdown error: {message}")]
    Shutdown {
        /// Description of the failure.
        message: String,
        /// The underlying error, if any.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// API server error.
    #[error("API server error: {0}")]
    Server(String),

    /// I/O error on a specific path.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The file or directory path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// An operation exceeded its time budget.
    #[error("operation timed out after {duration_ms}ms: {operation}")]
    Timeout {
        /// Description of the operation that timed out.
        operation: String,
        /// How long the timeout waited.
        duration_ms: u64,
    },

    /// System is not ready to serve requests.
    #[error("system not ready: {reason}")]
    NotReady {
        /// Why the system is not ready.
        reason: String,
    },
}

/// Convenience alias used at the system level.
pub type Result<T> = std::result::Result<T, RecalldError>;

impl RecalldError {
    /// Map this error to an HTTP status code for the API response.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Config(_) => 400,
            Self::NotReady { .. } => 503,
            Self::Timeout { .. } => 503,
            _ => 500,
        }
    }

    /// Determine whether the caller should retry the operation.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout { .. } | Self::NotReady { .. })
    }
}
