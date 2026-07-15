//! Core data model types for Recalld.
//!
//! This module re-exports all public types so consumers can write:
//! ```ignore
//! use recalld::model::{Memory, MemoryId, Tag, DecayPhase};
//! ```
//!
//! ## Required Cargo.toml entries
//!
//! ```toml
//! [dependencies]
//! uuid        = { version = "1.16", features = ["v7", "serde"] }
//! serde       = { version = "1.0", features = ["derive"] }
//! serde_json  = "1.0"
//! chrono      = { version = "0.4", default-features = false, features = ["clock", "serde"] }
//! thiserror   = "2.0"
//! ```

pub mod constants;
pub mod decay;
pub mod edge;
pub mod error;
pub mod id;
pub mod memory;
pub mod namespace;
pub mod record;
pub mod tag;

// Re-export primary types at module level for convenience.
pub use self::decay::DecayPhase;
pub use self::edge::EdgeType;
pub use self::error::{DecodeError, TagError, ValidationError};
pub use self::id::{MemoryId, NamespaceId};
pub use self::memory::{AccessEvent, AccessKind, Memory};
pub use self::namespace::{NamespaceConfig, PhaseThresholds};
pub use self::record::{CachedRecord, DiskRecord};
pub use self::tag::{StructuredMetadata, Tag, entity_overlap, parse_structured_tags};
