//! Recalld — an AI memory system with biologically-inspired decay,
//! retrieval-induced forgetting, and graph-based association.
//!
//! # Architecture
//!
//! Recalld is structured as a layered system:
//!
//! 1. **Model** — Core data types shared across all modules
//! 2. **Serialization** — Binary (disk) and JSON (API) format conversion
//! 3. **Storage** — Four-file persistence engine (vectors, metadata, text, edges)
//! 4. **Graph** — In-memory relationship graph with typed edges
//! 5. **Decay** — FSRS-based memory strength calculation and sweep runner
//! 6. **RIF** — Retrieval-induced forgetting engine
//! 7. **Cache** — RAM cache with neighborhood prefetching and pressure response
//! 8. **Embedding** — Pluggable embedding providers (OpenAI, Ollama, passthrough)
//! 9. **Search** — SIMD vector search, query pipeline
//! 10. **API** — axum HTTP server for LLM tool-call integration
//! 11. **CLI** — Command-line client for the API server
//! 12. **Config** — Layered configuration (TOML + env + CLI flags)
//! 13. **System** — Root struct, startup, shutdown, error types

// ── Module declarations ──────────────────────────────────────────────

pub mod api;
pub mod backup;
#[cfg(feature = "bench")]
pub mod bench;
pub mod cache;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod decay;
pub mod embedding;
pub mod error;
pub mod graph;
pub mod health;
pub mod mcp;
pub mod model;
pub mod rif;
pub mod search;
pub mod serialization;
pub mod storage;
pub mod system;
pub mod time;

// ── Top-level re-exports ─────────────────────────────────────────────
//
// Consumers write `use recalld::{Recalld, RecalldConfig, RecalldError}`
// for the system-level API, and `use recalld::model::*` for data types.

pub use config::RecalldConfig;
pub use error::RecalldError;
pub use system::Recalld;

// Re-export core types at the crate root for ergonomics.
pub use model::{DecayPhase, EdgeType, Memory, MemoryId, NamespaceConfig, NamespaceId};
