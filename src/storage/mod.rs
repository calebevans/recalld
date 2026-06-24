//! Storage layer for Recalld.
//!
//! Provides persistent storage for memory records, embeddings, text,
//! and graph edges. The public API is defined by the `StorageEngine`
//! trait; the concrete implementation is `RedbStorageEngine`.
//!
//! # Submodules
//!
//! - `vectors` — Memory-mapped vector storage (per-namespace .dat files)
//! - `metadata` — redb-backed metadata B-tree with secondary indexes
//! - `indexes` — Phase bitmap indexes (roaring)
//! - `text` — Append-only text log with CRC32 integrity
//! - `edges` — redb-backed graph edge persistence
//! - `error` — Unified `StorageError` type
//! - `fsync` — Filesystem sync helpers for crash safety

pub mod edges;
pub mod engine;
pub mod error;
pub mod fsync;
pub mod indexes;
pub mod metadata;
pub mod text;
pub mod vectors;

// ── Re-exports ──────────────────────────────────────────────────────

pub use edges::{cleanup_orphaned_edges, EdgeStore, PersistedEdge};
pub use engine::{RedbStorageEngine, StorageEngine};
pub use error::StorageError;
pub use indexes::PhaseIndex;
pub use metadata::MetadataStore;
pub use text::{
    recover_text_compaction, CompactionResult, TextRef, TextStore,
};
pub use vectors::{VectorManager, VectorStore};
