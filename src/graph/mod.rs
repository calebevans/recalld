//! Relationship graph for Recalld memory associations.
//!
//! Provides the in-memory graph data structure (`RelationshipGraph`),
//! BFS traversal, ACT-R spreading activation, embedding-based auto-linking,
//! and ghost memory bridging. All types are re-exported at the module level
//! for convenience:
//!
//! ```ignore
//! use recalld::graph::{RelationshipGraph, SharedGraph, GraphNode, GraphEdge};
//! ```

pub mod activation;
pub mod autolink;
mod structure;

// Re-export primary types from CS-10 (structure.rs)
pub use structure::{
    EdgeKey, GraphEdge, GraphError, GraphNode, GraphStats, NodeKey, RelationshipGraph,
    RemovalResult, TraversalDirection, TraversalFilter, TraversalResult,
};

// Re-export CS-11 activation types and functions
pub use activation::{
    ActivationConfig, MAX_CONNECTION_BONUS, SpreadingActivationConfig, connection_bonus,
    effective_retrievability, graph_stats, rebuild_from_storage, recompute_centrality,
    rif_edge_factor, spreading_activation, spreading_edge_factor,
};
// Re-export PersistedEdge from storage (the canonical definition)
pub use crate::storage::PersistedEdge;

// Re-export CS-11 auto-link types and functions
pub use autolink::{
    AutoLinkCandidate, AutoLinkError, AutoLinkThresholds, DEFAULT_MAX_LINKS, THRESHOLD_HARD_FLOOR,
    auto_link, perform_autolink, perform_entity_link, perform_temporal_link,
};

use std::sync::Arc;

use tokio::sync::RwLock;

/// System-level concurrent handle to the graph.
///
/// # Concurrency contract
///
/// `tokio::sync::RwLock` is async-aware and can be held across `.await`
/// points without blocking the executor thread. CS-04 calls async methods
/// on `SharedGraph` using `.read().await` and `.write().await`.
///
/// ```rust,ignore
/// let edges = {
///     let g = shared_graph.read().await;
///     g.edges_for(&src).iter().cloned().collect::<Vec<_>>()
/// }; // lock dropped
/// persist_edges(&edges).await?;
///
/// // Also valid -- lock held across await (safe with tokio::sync::RwLock)
/// let mut g = shared_graph.write().await;
/// g.add_edge(src, tgt, etype, weight, false)?;
/// ```
///
/// **Performance note**: tokio's RwLock has slightly higher per-operation
/// overhead than `std::sync::RwLock` for purely synchronous operations,
/// but the async compatibility is required by CS-04's decay sweep and
/// other async consumers.
pub type SharedGraph = Arc<RwLock<RelationshipGraph>>;
