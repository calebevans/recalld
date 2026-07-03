//! Vector similarity search and query pipeline for Recalld.
//!
//! This module provides:
//! - SIMD-accelerated dot product and L2 normalization (`simd`)
//! - `VectorIndex` trait and `FlatVectorIndex` brute-force implementation (`index`)
//! - `SearchQuery`, `QueryMode`, `SearchFilter` query types (`query`)
//! - `QueryEngine` orchestrating the 9-step search pipeline (`pipeline`)
//! - `SearchResult`, `SearchResponse`, `MemoryResponse` response types (`response`)
//! - `SearchError` error type (`error`)

// CS-14: SIMD vector search
mod index;
mod simd;

// CS-26: FTS5 full-text search (replaces custom BM25)
mod fts;

// CS-29: Entity index for entity-based graph edges
mod entity_index;
pub mod entity_normalize;

// CS-16: Search pipeline
pub mod adapters;
pub mod error;
mod pipeline;
mod query;
mod response;

// --- CS-14 re-exports ---
pub use index::{FilterEntry, FlatVectorIndex, TagInterner};
pub use simd::{dot_product_simd, is_normalized, normalize_l2};

// --- CS-26 re-exports ---
pub use fts::FtsIndex;

// --- CS-29 re-exports ---
pub use entity_index::EntityIndex;
pub use entity_normalize::canonicalize_entity;

// --- CS-16 re-exports ---
pub use error::SearchError;
pub use pipeline::{
    AccessRecorder, EmbeddingProviderRegistry, EntityIndexReader, EntityRecallResult,
    FtsIndexRegistry, FtsResult, GraphReader, MetadataStore, NamespaceResolver, QueryEngine,
    RecordCache, RifProcessor, RifSuppression, ScoredResult, VectorIndexRegistry,
};
pub use query::SearchFilter as PipelineSearchFilter;
pub use query::{QueryMode, SearchQuery};
pub use response::{
    MemoryResponse, SearchResponse, SearchResult as PipelineSearchResult, StageTimings,
};

use std::path::Path;

use crate::model::{MemoryId, NamespaceId};

// ---------------------------------------------------------------------------
// VectorSearchResult (CS-14)
// ---------------------------------------------------------------------------

/// A single result from the vector index search.
///
/// Returned by [`VectorIndex::search`]. The caller uses `id` to load the
/// full memory record from the record cache or disk.
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    /// The memory record this result refers to.
    pub id: MemoryId,
    /// Similarity score. For dot product on L2-normalized vectors, this
    /// is in `[-1.0, 1.0]`.
    pub score: f32,
    /// Decay phase at search time (1 = full, 2 = summary, 3 = ghost).
    pub decay_phase: u8,
}

// ---------------------------------------------------------------------------
// SearchFilter (CS-14, vector-level pre-filter)
// ---------------------------------------------------------------------------

/// Pre-filter criteria applied during vector search.
///
/// All specified fields are ANDed together. An empty filter matches
/// everything. Tag inclusion uses OR semantics; tag exclusion uses AND.
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    /// Only include memories in this namespace.
    pub namespace_id: Option<NamespaceId>,
    /// Memory must have at least one of these tags (OR semantics).
    pub include_tags: Vec<String>,
    /// Memory must not have any of these tags.
    pub exclude_tags: Vec<String>,
    /// Only include memories in these decay phases.
    pub decay_phases: Option<Vec<u8>>,
    /// Minimum similarity score threshold (post-filter).
    pub min_score: Option<f32>,
}

// ---------------------------------------------------------------------------
// VectorMetadata
// ---------------------------------------------------------------------------

/// Metadata stored alongside each vector for pre-filtering during search.
#[derive(Debug, Clone)]
pub struct VectorMetadata {
    /// Namespace this memory belongs to.
    pub namespace_id: NamespaceId,
    /// Current decay phase (1 = full, 2 = summary, 3 = ghost).
    pub decay_phase: u8,
    /// Tags associated with this memory.
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// VectorError
// ---------------------------------------------------------------------------

/// Errors that can occur during vector operations.
#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    /// Dimension mismatch between query and index.
    #[error("dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The index file is corrupt.
    #[error("corrupt index file: {0}")]
    CorruptIndex(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),
}

// ---------------------------------------------------------------------------
// VectorIndex trait
// ---------------------------------------------------------------------------

/// Abstract interface for vector similarity search.
///
/// Implementations must be thread-safe for concurrent reads. Write
/// operations require exclusive access managed by the caller.
pub trait VectorIndex: Send + Sync {
    /// Insert a vector for the given memory (must be L2-normalized).
    fn add(
        &mut self,
        id: MemoryId,
        vector: &[f32],
        metadata: VectorMetadata,
    ) -> Result<(), VectorError>;

    /// Remove a vector by memory ID. Returns `Ok(true)` if removed.
    fn remove(&mut self, id: MemoryId) -> Result<bool, VectorError>;

    /// Find the top-k most similar vectors to the query vector.
    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<VectorSearchResult>, VectorError>;

    /// Number of vectors currently stored.
    fn len(&self) -> usize;

    /// Whether the index is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The fixed dimensionality of vectors in this index.
    fn dimensions(&self) -> usize;

    /// Persist the index to disk.
    fn save(&self, path: &Path) -> Result<(), VectorError>;

    /// Whether the index should be rebuilt or switched.
    fn needs_rebuild(&self) -> bool;

    /// Return the raw vector for a given ID, if present.
    fn get_vector(&self, id: MemoryId) -> Option<Vec<f32>>;

    /// Update metadata for an existing vector without replacing vector data.
    fn update_metadata(
        &mut self,
        id: MemoryId,
        metadata: VectorMetadata,
    ) -> Result<bool, VectorError>;
}
