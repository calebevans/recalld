//! Serialization for Recalld memory records.
//!
//! Provides serde-based camelCase JSON format for the HTTP API wire protocol.
//! Uses `serde_json` with skip-if-none and integer-millis timestamps.

mod json;

pub use json::{
    ApiError, ApiResponse, CreateMemoryRequest, MemoryResponse, NamespaceRequest,
    NamespaceResponse, PaginatedResponse, PaginationParams, SearchHit, SearchRequest,
    SearchResponse, UpdateMemoryRequest,
};
