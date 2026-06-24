//! Graph rebuild from persistent storage.
//!
//! Reconstructs the in-memory relationship graph from edges.db records
//! at startup, after meta.db has been loaded.

use crate::graph::structure::RelationshipGraph;
use crate::storage::PersistedEdge;

/// Rebuild the in-memory graph from edges.db records.
///
/// Called at startup AFTER meta.db has been loaded (so id_index is
/// populated with all MemoryId -> NodeKey mappings).
///
/// # Sequence
/// 1. meta.db load populates nodes + id_index (CS-10 / CS-07)
/// 2. This function iterates edges.db and inserts edges
/// 3. For each edge: resolve source/target MemoryId to NodeKey
///    via id_index, create GraphEdge, push EdgeKey into
///    source.outgoing and target.incoming
///
/// # Error handling
/// Edges referencing unknown MemoryIds are logged and skipped.
/// This can happen if meta.db was compacted but edges.db was not
/// (a consistency violation that should be rare but survivable).
///
/// # Returns
/// Count of successfully loaded edges.
pub fn rebuild_from_storage(
    graph: &mut RelationshipGraph,
    edge_iter: impl Iterator<Item = PersistedEdge>,
) -> usize {
    let mut loaded: usize = 0;
    let mut skipped: usize = 0;

    for persisted in edge_iter {
        let source_key = match graph.id_index.get(&persisted.source) {
            Some(&k) => k,
            None => {
                skipped += 1;
                tracing::debug!(
                    source = %persisted.source,
                    "rebuild_from_storage: edge source not found in id_index"
                );
                continue;
            }
        };
        let target_key = match graph.id_index.get(&persisted.target) {
            Some(&k) => k,
            None => {
                skipped += 1;
                tracing::debug!(
                    target = %persisted.target,
                    "rebuild_from_storage: edge target not found in id_index"
                );
                continue;
            }
        };

        let edge = crate::graph::structure::GraphEdge {
            source: source_key,
            target: target_key,
            edge_type: persisted.edge_type,
            weight: persisted.weight,
            auto_created: persisted.auto_created,
            created_at: persisted.created_at as i64,
        };

        let ek = graph.edges.insert(edge);

        if let Some(source_node) = graph.nodes.get_mut(source_key) {
            source_node.outgoing.push(ek);
        }
        if let Some(target_node) = graph.nodes.get_mut(target_key) {
            target_node.incoming.push(ek);
        }

        loaded += 1;
    }

    if skipped > 0 {
        tracing::warn!(
            loaded,
            skipped,
            "rebuild_from_storage: skipped edges with missing endpoints"
        );
    }

    loaded
}
