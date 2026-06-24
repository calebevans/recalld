//! Central error type for all API handlers.
//!
//! `AppError` implements `IntoResponse` so handlers can return
//! `Result<impl IntoResponse, AppError>`. Each variant maps to an HTTP
//! status code and a machine-readable error code serialized as JSON via
//! [`ApiError`](crate::serialization::ApiError).

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::serialization::ApiError;

// ═══════════════════════════════════════════════════════════════════════
// AppError
// ═══════════════════════════════════════════════════════════════════════

/// Central error type for all API handlers. Implements `IntoResponse`
/// so handlers can return `Result<impl IntoResponse, AppError>`.
///
/// Each variant maps to a specific HTTP status code and machine-readable
/// error code. The [`ApiError`] JSON body provides the error code, a
/// human-readable message, and an optional field name for validation errors.
#[derive(Debug)]
pub enum AppError {
    /// 404 -- Memory, namespace, or resource not found.
    NotFound {
        /// The kind of resource (e.g. "memory", "namespace").
        resource: &'static str,
        /// The identifier that was looked up.
        id: String,
    },

    /// 400 -- Request validation failed.
    BadRequest {
        /// Human-readable description of the validation failure.
        message: String,
        /// The request field that caused the error, if applicable.
        field: Option<String>,
    },

    /// 409 -- Resource already exists (e.g. duplicate namespace name).
    Conflict {
        /// Human-readable description.
        message: String,
    },

    /// 422 -- Semantically invalid request (e.g. dimension mismatch,
    /// mutually exclusive fields both provided).
    UnprocessableEntity {
        /// Human-readable description.
        message: String,
        /// The request field that caused the error, if applicable.
        field: Option<String>,
    },

    /// 503 -- A required subsystem is unavailable (e.g. embedding
    /// provider down, storage I/O error).
    ServiceUnavailable {
        /// Human-readable description.
        message: String,
    },

    /// 408 -- Request timeout (from tower_http::timeout, but also
    /// catchable for embedding provider timeouts).
    Timeout,

    /// 500 -- Unexpected internal error. Logged with full context;
    /// the response body contains a generic message.
    Internal {
        /// The underlying error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

// ═══════════════════════════════════════════════════════════════════════
// IntoResponse
// ═══════════════════════════════════════════════════════════════════════

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_code, message, field) = match &self {
            AppError::NotFound { resource, id } => (
                StatusCode::NOT_FOUND,
                "NOT_FOUND".to_string(),
                format!("{resource} '{id}' not found"),
                None,
            ),
            AppError::BadRequest { message, field } => (
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST".to_string(),
                message.clone(),
                field.clone(),
            ),
            AppError::Conflict { message } => (
                StatusCode::CONFLICT,
                "CONFLICT".to_string(),
                message.clone(),
                None,
            ),
            AppError::UnprocessableEntity { message, field } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "UNPROCESSABLE_ENTITY".to_string(),
                message.clone(),
                field.clone(),
            ),
            AppError::ServiceUnavailable { message } => (
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE".to_string(),
                message.clone(),
                None,
            ),
            AppError::Timeout => (
                StatusCode::REQUEST_TIMEOUT,
                "TIMEOUT".to_string(),
                "request timed out".to_string(),
                None,
            ),
            AppError::Internal { source } => {
                // Log the full error chain for debugging. Never
                // expose internal details to the client.
                tracing::error!(error = %source, "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR".to_string(),
                    "an internal error occurred".to_string(),
                    None,
                )
            }
        };

        let body = ApiError {
            error: error_code,
            message,
            field,
        };

        (status, Json(body)).into_response()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// From implementations for real subsystem errors
// ═══════════════════════════════════════════════════════════════════════

// Re-export real error types from their owning modules so downstream
// code that was using `super::errors::StorageError` etc. continues to
// compile.
pub use crate::embedding::EmbeddingError;
pub use crate::graph::GraphError;
pub use crate::search::SearchError;
pub use crate::storage::StorageError;

impl From<StorageError> for AppError {
    fn from(err: StorageError) -> Self {
        match &err {
            StorageError::NotFound(uuid) => AppError::NotFound {
                resource: "memory",
                id: uuid.to_string(),
            },
            StorageError::DuplicateId(uuid) => AppError::Conflict {
                message: format!("memory {} already exists", uuid),
            },
            StorageError::DuplicateName(name) => AppError::Conflict {
                message: format!("namespace '{}' already exists", name),
            },
            StorageError::NamespaceNotFound(id) => AppError::NotFound {
                resource: "namespace",
                id: id.to_string(),
            },
            StorageError::DatabaseLocked => AppError::ServiceUnavailable {
                message: "database is locked by another process".to_string(),
            },
            _ => AppError::Internal {
                source: Box::new(err),
            },
        }
    }
}

impl From<EmbeddingError> for AppError {
    fn from(err: EmbeddingError) -> Self {
        match &err {
            EmbeddingError::DimensionMismatch { expected, got } => AppError::UnprocessableEntity {
                message: format!("embedding dimension mismatch: expected {expected}, got {got}"),
                field: Some("embedding".to_string()),
            },
            EmbeddingError::Unavailable(_)
            | EmbeddingError::RateLimited { .. }
            | EmbeddingError::Network(_) => AppError::ServiceUnavailable {
                message: format!("embedding provider: {err}"),
            },
            EmbeddingError::TextTooLong { tokens, limit } => AppError::BadRequest {
                message: format!("text too long: {tokens} tokens exceeds limit of {limit}"),
                field: Some("text".to_string()),
            },
            _ => AppError::Internal {
                source: Box::new(err),
            },
        }
    }
}

impl From<SearchError> for AppError {
    fn from(err: SearchError) -> Self {
        match &err {
            SearchError::NamespaceNotFound(name) => AppError::NotFound {
                resource: "namespace",
                id: name.clone(),
            },
            SearchError::EmbeddingFailed(msg) => AppError::ServiceUnavailable {
                message: format!("embedding failed: {msg}"),
            },
            SearchError::EmptyQuery => AppError::BadRequest {
                message: "query text is required".to_string(),
                field: Some("query".to_string()),
            },
            SearchError::MemoryNotFound(id) => AppError::NotFound {
                resource: "memory",
                id: format!("{id}"),
            },
            SearchError::StageTimeout { .. } => AppError::Timeout,
            _ => AppError::Internal {
                source: Box::new(err),
            },
        }
    }
}

impl From<GraphError> for AppError {
    fn from(err: GraphError) -> Self {
        match &err {
            GraphError::MemoryNotFound(id) => AppError::NotFound {
                resource: "memory",
                id: format!("{id}"),
            },
            GraphError::DuplicateNode(id) => AppError::Conflict {
                message: format!("graph node for memory {} already exists", id),
            },
            _ => AppError::Internal {
                source: Box::new(err),
            },
        }
    }
}
