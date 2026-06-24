//! Error types for the search pipeline.

use crate::model::MemoryId;
use thiserror::Error;

/// Errors that can occur during search operations.
#[derive(Debug, Error)]
pub enum SearchError {
    /// The specified namespace does not exist.
    #[error("namespace not found: {0}")]
    NamespaceNotFound(String),

    /// Embedding generation failed.
    #[error("embedding provider error: {0}")]
    EmbeddingFailed(String),

    /// Vector index operation failed.
    #[error("vector index error: {0}")]
    VectorIndexError(String),

    /// Metadata storage operation failed.
    #[error("metadata store error: {0}")]
    MetadataError(String),

    /// The requested memory was not found.
    #[error("memory not found: {0:?}")]
    MemoryNotFound(MemoryId),

    /// A pipeline stage exceeded its time budget.
    #[error("query timeout: {stage} exceeded {budget_ms}ms (actual: {actual_ms}ms)")]
    StageTimeout {
        stage: &'static str,
        budget_ms: u64,
        actual_ms: u64,
    },

    /// No search criteria were provided.
    #[error("empty query: at least one of text, tags, or memory_id must be provided")]
    EmptyQuery,

    /// An unexpected internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience alias for search operations.
pub type Result<T> = std::result::Result<T, SearchError>;
