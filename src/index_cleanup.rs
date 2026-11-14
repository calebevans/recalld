//! Best-effort cleanup for legacy namespace purge. Canonical forget uses the
//! durable effects/completion path; this remaining purge helper cannot promise
//! atomicity or derived readiness after failures.

use crate::cache::CacheManager;
use crate::graph::SharedGraph;
use crate::model::{DiskRecord, MemoryId, NamespaceId};
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
    /// Graph nodes removed.
    pub graph_nodes_removed: usize,
}

/// Remove a batch of memories from the cache and the four indexes.
///
/// Takes **one** lock per subsystem for the whole slice, not one per
/// record.
///
/// `records` must be the memories as they were **before** any tombstone
/// strip. `tombstone` erases the tag list, and the tag list is where the
/// entity names live — pass the stripped record and the entity index
/// keeps pointing at the deleted memory forever.
pub(crate) async fn purge_from_indexes(
    handles: &IndexHandles<'_>,
    records: &[(MemoryId, DiskRecord)],
    fts: FtsAction,
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

    // 4. Entity index. Driven off the pre-strip tags.
    {
        let mut entity_index = handles.entity_index.write().await;
        for (id, record) in records {
            let metadata = crate::model::parse_structured_tags(&record.tags);
            if !metadata.entities.is_empty() {
                entity_index.remove(*id, &metadata.entities);
            }
        }
    }

    // 5. Graph.
    {
        let mut graph_w = handles.graph.write().await;
        for id in &ids {
            if graph_w.contains(id) {
                graph_w.remove_memory(*id);
                counts.graph_nodes_removed += 1;
            }
        }
    }

    counts
}
