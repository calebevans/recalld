//! Removing memories from the indexes that storage cannot reach.
//!
//! Deleting a memory is two jobs. Storage owns one of them — the
//! meta.db record, the vector slot, the edge rows — and returns.
//! The other job is the four indexes that live outside storage: the
//! FTS5 database, the in-memory `FlatVectorIndex`, the entity index and
//! the relationship graph. Nothing in `storage/` holds a handle to any
//! of them, so every delete path has to do that half itself.
//!
//! There are three such paths — the MCP tool, the HTTP endpoint, and
//! now namespace deletion — and until this module existed they were
//! three hand-written copies of the same six steps. Two of them agreed;
//! the HTTP one had quietly stopped after the cache invalidation, so an
//! HTTP-deleted memory kept a live vector-index entry (burning a top-k
//! slot in every search and looking like a valid autolink target), kept
//! its FTS row (which survives restart forever), and kept a graph node
//! reporting full strength, propping up its neighbours' decay
//! resistance.
//!
//! One implementation is the fix for that; a comment telling the next
//! author to keep three copies in step is not.
//!
//! # Best-effort by design
//!
//! Every step here runs after the storage-side removal has already
//! committed. There is nothing to roll back to, so a failure is warned
//! about and the remaining steps still run: a wedged FTS connection must
//! not be the reason the vector index keeps serving a deleted memory.

use crate::cache::CacheManager;
use crate::graph::SharedGraph;
use crate::model::{DecayPhase, DiskRecord, MemoryId, NamespaceId};
use crate::search::{EntityIndex, FlatVectorIndex, FtsIndex};

/// Borrowed handles to the four out-of-storage indexes, plus the cache.
pub(crate) struct IndexHandles<'a> {
    /// Record cache — entries for deleted memories must not be served.
    pub cache: &'a CacheManager,
    /// In-memory vector index used by semantic search.
    pub vector_index: &'a tokio::sync::RwLock<FlatVectorIndex>,
    /// SQLite FTS5 index used by keyword search.
    pub fts_index: &'a tokio::sync::Mutex<FtsIndex>,
    /// Entity -> memory inverted index.
    pub entity_index: &'a tokio::sync::RwLock<EntityIndex>,
    /// Relationship graph.
    pub graph: &'a SharedGraph,
}

/// What to do with each memory's graph node.
pub(crate) enum GraphAction {
    /// Keep the node and its edges, set its phase to Tombstone. For
    /// `forget_memory`: the record survives, so relationship chains
    /// stay traversable.
    MarkTombstoned,
    /// Remove the node and every edge incident to it, with no bridging.
    /// For namespace deletion: the record is gone, so a node pointing at
    /// it would be a dangling reference.
    RemoveNode,
}

/// How to reach the FTS rows.
pub(crate) enum FtsAction {
    /// One `remove` per supplied record.
    PerRecord,
    /// One `DELETE ... WHERE namespace_id = ?`. Reaches rows whose
    /// meta.db record is already gone, which per-record removal cannot.
    WholeNamespace(NamespaceId),
}

/// What was actually removed, for reporting back to the caller.
#[derive(Debug, Default)]
pub(crate) struct IndexPurgeCounts {
    /// FTS documents deleted.
    pub fts_removed: usize,
    /// Vector-index entries deleted.
    pub vector_entries_removed: usize,
    /// Graph nodes removed. Always 0 for [`GraphAction::MarkTombstoned`].
    pub graph_nodes_removed: usize,
}

/// Remove a batch of memories from the cache and the four indexes.
///
/// Takes **one** lock per subsystem for the whole slice, not one per
/// record.
///
/// Records supply canonical namespace IDs; entity cleanup uses reverse
/// postings and works even after labels have been stripped by tombstoning.
pub(crate) async fn purge_from_indexes(
    handles: &IndexHandles<'_>,
    records: &[(MemoryId, DiskRecord)],
    fts: FtsAction,
    graph: GraphAction,
) -> IndexPurgeCounts {
    let mut counts = IndexPurgeCounts::default();

    if records.is_empty() && matches!(fts, FtsAction::PerRecord) {
        return counts;
    }

    let ids: Vec<MemoryId> = records.iter().map(|(id, _)| *id).collect();

    // 1. Cache.
    handles.cache.batch_invalidate(&ids).await;

    // 2. FTS5.
    {
        let fts_index = handles.fts_index.lock().await;
        match fts {
            FtsAction::PerRecord => {
                for id in &ids {
                    match fts_index.remove(*id) {
                        Ok(true) => counts.fts_removed += 1,
                        Ok(false) => {}
                        Err(e) => tracing::warn!(
                            memory_id = %id,
                            %e,
                            "FTS5 removal failed (non-fatal)"
                        ),
                    }
                }
            }
            FtsAction::WholeNamespace(namespace_id) => {
                match fts_index.remove_namespace(namespace_id) {
                    Ok(removed) => counts.fts_removed = removed,
                    Err(e) => tracing::warn!(
                        namespace_id = namespace_id.get(),
                        %e,
                        "FTS5 namespace removal failed (non-fatal)"
                    ),
                }
            }
        }
    }

    // 3. Vector index.
    {
        use crate::search::VectorIndex;
        let mut vector_index = handles.vector_index.write().await;
        for id in &ids {
            match vector_index.remove(*id) {
                Ok(true) => counts.vector_entries_removed += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    memory_id = %id,
                    %e,
                    "vector index removal failed (non-fatal)"
                ),
            }
        }
    }

    // 4. Entity index: labels may already be stripped or lossy in legacy tags.
    {
        let mut entity_index = handles.entity_index.write().await;
        match fts {
            FtsAction::WholeNamespace(namespace_id) => entity_index.remove_namespace(namespace_id),
            FtsAction::PerRecord => {
                for (id, record) in records {
                    entity_index.remove(NamespaceId::new(record.namespace_id), *id);
                }
            }
        }
    }

    // 5. Graph.
    {
        let mut graph_w = handles.graph.write().await;
        for id in &ids {
            match graph {
                GraphAction::MarkTombstoned => {
                    // Err means the node is not in the graph, which is
                    // not a problem: there is nothing left to mark.
                    let _ = graph_w.update_node_state(*id, DecayPhase::Tombstone, 0.0);
                }
                GraphAction::RemoveNode => {
                    if graph_w.contains(id) {
                        graph_w.remove_memory(*id);
                        counts.graph_nodes_removed += 1;
                    }
                }
            }
        }
    }

    counts
}
