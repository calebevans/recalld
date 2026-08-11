//! Caller-specified graph edges: validation and application.
//!
//! Named for contrast with [`autolink`](super::autolink): the edges here are
//! the ones a client explicitly asked for (`supersedes`, `parentId`), not the
//! ones the system derives from similarity, shared entities, or time.
//!
//! # Why validation is separate from application
//!
//! Both the MCP and HTTP store paths commit the memory record to storage
//! (meta.db + fulltext.dat + vectors.dat + FTS + cache) *before* they touch
//! the graph. Failing after that write would require unwinding all of it.
//! So the flow is split in two:
//!
//! 1. [`check_link_target`] runs **before** anything is written. A missing,
//!    cross-namespace, or self-referential target fails the whole store, and
//!    because nothing has been written yet there is no rollback to get wrong.
//! 2. [`apply_supersedes_edge`] runs **after** the record is committed and
//!    never returns an error — a duplicate edge or a persistence failure must
//!    not report a store that plainly succeeded as a failure. The
//!    [`SupersedesOutcome`] it returns is the truthful report instead.

use std::sync::Arc;

use crate::cache::CacheManager;
use crate::graph::SharedGraph;
use crate::model::{EdgeType, MemoryId, NamespaceId, SupersedesOutcome, SupersedesStatus};
use crate::storage::engine::RedbStorageEngine;
use crate::storage::{PersistedEdge, StorageEngine as StorageEngineTrait};

// ═══════════════════════════════════════════════════════════════════════
// LinkError
// ═══════════════════════════════════════════════════════════════════════

/// A caller-specified link target that cannot be honoured.
///
/// Every variant is a *pre-write* failure: it is raised before the new
/// memory exists, so the correct response is to reject the store outright.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// No memory with this ID exists in storage.
    #[error("{field} target {target} does not exist")]
    TargetNotFound {
        /// The request field naming the target (`supersedes`, `parentId`).
        field: &'static str,
        /// The target ID the caller supplied.
        target: MemoryId,
    },

    /// The target exists but lives in a different namespace.
    #[error(
        "{field} target {target} belongs to namespace {actual} but the new memory is being \
         stored in namespace {expected}; {field} links must stay within a single namespace"
    )]
    NamespaceMismatch {
        /// The request field naming the target.
        field: &'static str,
        /// The target ID the caller supplied.
        target: MemoryId,
        /// The namespace the target actually lives in.
        actual: NamespaceId,
        /// The namespace the new memory is being stored in.
        expected: NamespaceId,
    },

    /// The target is the memory being stored — a degenerate 1-cycle.
    #[error("{field} target {target} is the memory being stored; a memory cannot link to itself")]
    SelfReference {
        /// The request field naming the target.
        field: &'static str,
        /// The target ID the caller supplied.
        target: MemoryId,
    },

    /// The target has a storage record but no graph node. The edge cannot
    /// be created, and the inconsistency is ours, not the caller's.
    #[error(
        "{field} target {target} exists in storage but is missing from the relationship graph \
         (internal inconsistency)"
    )]
    TargetMissingFromGraph {
        /// The request field naming the target.
        field: &'static str,
        /// The target ID the caller supplied.
        target: MemoryId,
    },

    /// Looking the target up failed (storage error, poisoned lock, panic).
    #[error("failed to look up {field} target {target}: {message}")]
    Lookup {
        /// The request field naming the target.
        field: &'static str,
        /// The target ID the caller supplied.
        target: MemoryId,
        /// The underlying failure.
        message: String,
    },
}

impl LinkError {
    /// The request field this error is about, for `field`-scoped error
    /// responses (HTTP 422 bodies carry it; MCP folds it into the message).
    pub fn field(&self) -> &'static str {
        match self {
            LinkError::TargetNotFound { field, .. }
            | LinkError::NamespaceMismatch { field, .. }
            | LinkError::SelfReference { field, .. }
            | LinkError::TargetMissingFromGraph { field, .. }
            | LinkError::Lookup { field, .. } => field,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// validate_link_target (pure)
// ═══════════════════════════════════════════════════════════════════════

/// Decide whether a caller-specified link target is usable.
///
/// Pure: all I/O is done by the caller and handed in. [`check_link_target`]
/// is the async wrapper that performs those lookups.
///
/// # Arguments
///
/// * `field` — the request field naming the target, used in messages.
/// * `new_memory_id` — the ID of the memory being stored, or `None` when the
///   caller has not minted one yet (the HTTP create path). `None` skips the
///   self-reference check, which is vacuous when the ID does not exist.
/// * `target_namespace` — the namespace of the target's storage record, or
///   `None` when no record was found.
/// * `target_in_graph` — whether the target has a node in the graph.
///
/// # Deliberately allowed
///
/// A tombstoned target passes. Tombstoned memories keep their graph node by
/// design and recall never surfaces them, so rejecting them would break the
/// legitimate "correct a memory I just deleted" flow.
pub fn validate_link_target(
    field: &'static str,
    new_memory_id: Option<MemoryId>,
    target: MemoryId,
    target_namespace: Option<NamespaceId>,
    target_in_graph: bool,
    expected_namespace: NamespaceId,
) -> Result<(), LinkError> {
    if new_memory_id == Some(target) {
        return Err(LinkError::SelfReference { field, target });
    }

    let Some(actual) = target_namespace else {
        return Err(LinkError::TargetNotFound { field, target });
    };

    if actual != expected_namespace {
        return Err(LinkError::NamespaceMismatch {
            field,
            target,
            actual,
            expected: expected_namespace,
        });
    }

    if !target_in_graph {
        return Err(LinkError::TargetMissingFromGraph { field, target });
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// check_link_target (async I/O wrapper)
// ═══════════════════════════════════════════════════════════════════════

/// Look up a caller-specified link target and validate it.
///
/// Call this **before** writing the new memory: a failure here should reject
/// the store, and doing so pre-write means there is nothing to unwind.
///
/// The lookup reads the RAM cache first and falls back to a blocking
/// `get_record` on a spawned thread, mirroring the rest of the store path.
pub async fn check_link_target(
    field: &'static str,
    new_memory_id: Option<MemoryId>,
    target: MemoryId,
    expected_namespace: NamespaceId,
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) -> Result<(), LinkError> {
    // Cheapest check first: it needs no I/O at all.
    if new_memory_id == Some(target) {
        return Err(LinkError::SelfReference { field, target });
    }

    let target_namespace = match cache.get(target).await {
        Some(record) => Some(record.namespace_id),
        None => {
            let storage = storage.clone();
            let record = tokio::task::spawn_blocking(move || {
                let storage_r = storage
                    .read()
                    .map_err(|e| format!("storage lock poisoned: {e}"))?;
                storage_r.get_record(target).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| LinkError::Lookup {
                field,
                target,
                message: format!("blocking task join error: {e}"),
            })?
            .map_err(|message| LinkError::Lookup {
                field,
                target,
                message,
            })?;
            record.map(|r| NamespaceId::new(r.namespace_id))
        }
    };

    let target_in_graph = graph.read().await.get_node(&target).is_some();

    validate_link_target(
        field,
        new_memory_id,
        target,
        target_namespace,
        target_in_graph,
        expected_namespace,
    )
}

// ═══════════════════════════════════════════════════════════════════════
// Applying the edge
// ═══════════════════════════════════════════════════════════════════════

/// What happened when a caller-specified edge was inserted and persisted.
#[derive(Debug)]
enum EdgeApplication {
    /// Inserted into the graph and written to `edges.db`.
    Created,
    /// An edge of this type already joined the two memories.
    AlreadyPresent,
    /// The graph refused the edge (carries the reason).
    Rejected(String),
    /// Live in this process's graph but not written to disk.
    NotDurable(String),
}

/// Insert one caller-specified edge into the graph and persist it.
///
/// Shared by every explicit link field so `supersedes` and `parentId`
/// cannot drift apart in durability, locking, or `edge_count` behaviour —
/// the inconsistency between them was itself part of the reported bug.
async fn apply_explicit_edge(
    source: MemoryId,
    target: MemoryId,
    edge_type: EdgeType,
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) -> EdgeApplication {
    // Insert into the in-memory graph, then release the write lock before
    // touching storage. Holding the graph lock across a storage acquisition
    // would invert the lock order used everywhere else.
    let insert_result = {
        let mut graph_w = graph.write().await;
        graph_w.add_edge(source, target, edge_type, 1.0, false)
    };

    match insert_result {
        Ok(_) => {}
        Err(crate::graph::GraphError::EdgeExists(_, _)) => {
            return EdgeApplication::AlreadyPresent;
        }
        Err(e) => return EdgeApplication::Rejected(e.to_string()),
    }

    let persisted = PersistedEdge {
        source,
        target,
        edge_type,
        weight: 1.0,
        auto_created: false,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };

    let persist_result = {
        let storage = storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| format!("storage lock poisoned: {e}"))?;
            storage_r
                .batch_add_edges(&[persisted])
                .map_err(|e| e.to_string())
        })
        .await
    };

    let persist_error = match persist_result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(e) => Some(format!("blocking task join error: {e}")),
    };

    if let Some(message) = persist_error {
        return EdgeApplication::NotDurable(message);
    }

    // `edge_count` counts OUTGOING edges, so only the source's count moves.
    // The update is additive and runs before autolink, which re-reads the
    // cache, so the two stay in agreement.
    bump_edge_count(source, storage, cache).await;

    EdgeApplication::Created
}

/// Create and persist the `Supersedes` edge for a memory that has already
/// been written.
///
/// Never returns an error — by the time this runs the record is committed,
/// so reporting a failure would lie about a store that succeeded. The
/// returned [`SupersedesOutcome`] is the report instead; see
/// [`SupersedesStatus`] for what each variant means.
///
/// Assumes [`check_link_target`] already ran. If the target vanished in
/// between, the outcome is [`SupersedesStatus::Failed`] and an `error!` is
/// logged, because that is a real (if unreachable) internal inconsistency.
pub async fn apply_supersedes_edge(
    new_memory_id: MemoryId,
    target: MemoryId,
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) -> SupersedesOutcome {
    match apply_explicit_edge(
        new_memory_id,
        target,
        EdgeType::Supersedes,
        graph,
        storage,
        cache,
    )
    .await
    {
        EdgeApplication::Created => SupersedesOutcome::new(target, SupersedesStatus::Applied),

        EdgeApplication::AlreadyPresent => SupersedesOutcome::with_detail(
            target,
            SupersedesStatus::AlreadyApplied,
            "a supersedes edge already connected these two memories; \
             the requested end state was already in place",
        ),

        EdgeApplication::Rejected(message) => {
            tracing::error!(
                memory_id = %new_memory_id,
                superseded = %target,
                error = %message,
                "supersedes edge could not be added to the graph after pre-validation passed"
            );
            SupersedesOutcome::with_detail(
                target,
                SupersedesStatus::Failed,
                format!("the supersedes edge could not be created: {message}"),
            )
        }

        EdgeApplication::NotDurable(message) => {
            tracing::error!(
                memory_id = %new_memory_id,
                superseded = %target,
                error = %message,
                "supersedes edge is live in the graph but could not be persisted to edges.db; \
                 it will be lost on restart"
            );
            SupersedesOutcome::with_detail(
                target,
                SupersedesStatus::AppliedNotDurable,
                format!(
                    "the supersedes edge is active for this process but was not written to \
                     disk and will be lost on restart: {message}"
                ),
            )
        }
    }
}

/// Create and persist the `ParentChild` edge from `parent` to `child`.
///
/// Direction follows [`EdgeType::ParentChild`]: source is the parent.
/// Like the supersedes edge this runs after the record is committed and so
/// cannot fail the store; problems are logged. Assumes
/// [`check_link_target`] already ran with field `"parentId"`.
pub async fn apply_parent_edge(
    parent: MemoryId,
    child: MemoryId,
    graph: &SharedGraph,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) {
    match apply_explicit_edge(parent, child, EdgeType::ParentChild, graph, storage, cache).await {
        EdgeApplication::Created | EdgeApplication::AlreadyPresent => {}
        EdgeApplication::Rejected(message) => tracing::error!(
            memory_id = %child,
            parent = %parent,
            error = %message,
            "parent edge could not be added to the graph after pre-validation passed"
        ),
        EdgeApplication::NotDurable(message) => tracing::error!(
            memory_id = %child,
            parent = %parent,
            error = %message,
            "parent edge is live in the graph but could not be persisted to edges.db; \
             it will be lost on restart"
        ),
    }
}

/// Additively increment a memory's cached outgoing-edge count by one,
/// in both storage and the RAM cache. Mirrors `autolink::persist_edges`:
/// a failure here is a stale counter, not a lost edge, so it only warns.
async fn bump_edge_count(
    memory_id: MemoryId,
    storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: &Arc<CacheManager>,
) {
    let new_count = cache
        .get(memory_id)
        .await
        .map(|r| r.edge_count)
        .unwrap_or(0)
        .saturating_add(1);

    {
        let storage = storage.clone();
        match tokio::task::spawn_blocking(move || {
            let storage_r = storage
                .read()
                .map_err(|e| format!("storage lock poisoned: {e}"))?;
            storage_r
                .update_edge_count(memory_id, new_count)
                .map_err(|e| e.to_string())
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                memory_id = %memory_id,
                error = %e,
                "Failed to update edge count in storage; cache is correct but on-disk record may be stale"
            ),
            Err(e) => tracing::warn!(
                memory_id = %memory_id,
                error = %e,
                "Edge count update task panicked; cache is correct but on-disk record may be stale"
            ),
        }
    }

    cache.update_edge_count(memory_id, new_count).await;
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// The namespace the new memory is being stored in.
    fn ns() -> NamespaceId {
        NamespaceId::new(1)
    }

    // ── Live-storage fixtures ────────────────────────────────────────

    /// Embedding width used by the fixture namespace. Small on purpose:
    /// these tests are about edge bookkeeping, not vectors.
    const DIM: usize = 4;

    struct Fixture {
        graph: SharedGraph,
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut engine = RedbStorageEngine::open(dir.path()).expect("open storage");
            let mut config = crate::model::NamespaceConfig::default_namespace(0);
            config.embedding_dim = DIM as u32;
            if engine
                .get_namespace_by_name(&config.name)
                .expect("namespace lookup")
                .is_none()
            {
                engine.create_namespace(&config).expect("create namespace");
            }

            Self {
                graph: Arc::new(tokio::sync::RwLock::new(
                    crate::graph::RelationshipGraph::new(),
                )),
                storage: Arc::new(std::sync::RwLock::new(engine)),
                cache: Arc::new(crate::cache::CacheManager::new(
                    crate::cache::CacheConfig {
                        embedding_dim: DIM,
                        ..Default::default()
                    },
                    None,
                )),
                _dir: dir,
            }
        }

        /// Writes a memory to storage and gives it a graph node, which is
        /// the state an explicit edge is applied against.
        async fn insert_memory(&self) -> MemoryId {
            let id = MemoryId::new();
            let mut record = crate::model::DiskRecord {
                version: crate::model::DiskRecord::CURRENT_VERSION,
                id: *id.as_bytes(),
                namespace_id: ns().get(),
                created_at: 0,
                last_accessed_at: 0,
                phase: crate::model::DecayPhase::Full,
                strength: 1.0,
                decay_strength: 1.0,
                stability: 1.0,
                difficulty: 5.0,
                is_permastore: 0,
                vector_slot: 0,
                edge_count: 0,
                summary: "fixture".into(),
                tags: Vec::new(),
                access_history: Vec::new(),
                text_offset: 0,
                text_length: 0,
            };

            {
                let mut storage_w = self.storage.write().expect("storage lock");
                storage_w
                    .insert_memory(id, ns(), &mut record, &[0.0; DIM], None)
                    .expect("insert memory");
            }

            let slot = record.vector_slot;
            self.graph
                .write()
                .await
                .add_node(id, ns(), crate::model::DecayPhase::Full, 1.0, slot)
                .expect("add node");
            id
        }

        fn edge_count(&self, id: MemoryId) -> u16 {
            self.storage
                .read()
                .expect("storage lock")
                .get_record(id)
                .expect("get record")
                .expect("record exists")
                .edge_count
        }

        fn persisted_edges(&self) -> Vec<PersistedEdge> {
            self.storage
                .read()
                .expect("storage lock")
                .load_all_edges()
                .expect("load edges")
        }
    }

    /// Regression: a `parentId` edge must move `edge_count`, whichever door
    /// created it.
    ///
    /// The HTTP handlers used to insert this edge themselves and skip the
    /// counter, so the same memory ranked differently depending on whether
    /// it arrived over HTTP or MCP. Both doors now call this helper, and
    /// there is no longer a raw edge-insertion method on the HTTP graph
    /// trait to drift back to.
    #[tokio::test]
    async fn parent_edge_is_persisted_and_counted_on_the_parent() {
        let fx = Fixture::new();
        let parent = fx.insert_memory().await;
        let child = fx.insert_memory().await;

        apply_parent_edge(parent, child, &fx.graph, &fx.storage, &fx.cache).await;

        // `edge_count` counts OUTGOING edges, so only the parent's moves.
        assert_eq!(fx.edge_count(parent), 1, "parent edge must be counted");
        assert_eq!(fx.edge_count(child), 0, "the child owns no outgoing edge");

        let edges = fx.persisted_edges();
        assert_eq!(edges.len(), 1, "the edge must survive a restart");
        assert_eq!(edges[0].source, parent);
        assert_eq!(edges[0].target, child);
        assert_eq!(edges[0].edge_type, EdgeType::ParentChild);
        assert!(!edges[0].auto_created);
    }

    /// The two explicit link fields must behave identically, which is the
    /// reason they share [`apply_explicit_edge`] at all.
    #[tokio::test]
    async fn supersedes_and_parent_edges_count_the_same_way() {
        let fx = Fixture::new();
        let parent = fx.insert_memory().await;
        let child = fx.insert_memory().await;
        let superseded = fx.insert_memory().await;

        apply_parent_edge(parent, child, &fx.graph, &fx.storage, &fx.cache).await;
        let outcome =
            apply_supersedes_edge(child, superseded, &fx.graph, &fx.storage, &fx.cache).await;

        assert_eq!(outcome.status, SupersedesStatus::Applied);
        assert_eq!(fx.edge_count(parent), 1);
        assert_eq!(fx.edge_count(child), 1, "the supersedes source is counted");
        assert_eq!(fx.edge_count(superseded), 0);
    }

    /// Re-applying the same parent edge must not inflate the count: the
    /// requested end state was already in place.
    #[tokio::test]
    async fn repeated_parent_edge_does_not_double_count() {
        let fx = Fixture::new();
        let parent = fx.insert_memory().await;
        let child = fx.insert_memory().await;

        apply_parent_edge(parent, child, &fx.graph, &fx.storage, &fx.cache).await;
        apply_parent_edge(parent, child, &fx.graph, &fx.storage, &fx.cache).await;

        assert_eq!(fx.edge_count(parent), 1);
        assert_eq!(fx.persisted_edges().len(), 1);
    }

    /// Any other namespace.
    fn other_ns() -> NamespaceId {
        NamespaceId::new(2)
    }

    /// A missing target must error rather than silently no-op — the whole
    /// point of the fix.
    #[test]
    fn missing_target_errors() {
        let target = MemoryId::new();
        let err = validate_link_target(
            "supersedes",
            Some(MemoryId::new()),
            target,
            None,
            false,
            ns(),
        )
        .unwrap_err();

        assert!(matches!(err, LinkError::TargetNotFound { .. }));
        assert!(err.to_string().contains("does not exist"));
    }

    /// A target in another namespace is rejected: honouring it would create
    /// exactly the cross-namespace edge that recall must not follow.
    #[test]
    fn cross_namespace_target_errors() {
        let target = MemoryId::new();
        let err = validate_link_target(
            "supersedes",
            Some(MemoryId::new()),
            target,
            Some(other_ns()),
            true,
            ns(),
        )
        .unwrap_err();

        match err {
            LinkError::NamespaceMismatch {
                actual, expected, ..
            } => {
                assert_eq!(actual, other_ns());
                assert_eq!(expected, ns());
            }
            other => panic!("expected NamespaceMismatch, got {other:?}"),
        }
    }

    /// The happy path: record present, node present, same namespace.
    #[test]
    fn valid_target_is_accepted() {
        let result = validate_link_target(
            "supersedes",
            Some(MemoryId::new()),
            MemoryId::new(),
            Some(ns()),
            true,
            ns(),
        );

        assert!(result.is_ok());
    }

    /// A record with no graph node cannot receive an edge, and that is an
    /// internal inconsistency rather than caller error.
    #[test]
    fn record_without_graph_node_errors() {
        let err = validate_link_target(
            "supersedes",
            Some(MemoryId::new()),
            MemoryId::new(),
            Some(ns()),
            false,
            ns(),
        )
        .unwrap_err();

        assert!(matches!(err, LinkError::TargetMissingFromGraph { .. }));
    }

    /// Self-reference is a degenerate 1-cycle and is rejected — but only
    /// when the caller has an ID to compare against.
    #[test]
    fn self_reference_errors_only_when_id_is_known() {
        let id = MemoryId::new();

        let err =
            validate_link_target("supersedes", Some(id), id, Some(ns()), true, ns()).unwrap_err();
        assert!(matches!(err, LinkError::SelfReference { .. }));

        // The HTTP create path has no ID yet; the check is vacuous there.
        assert!(validate_link_target("supersedes", None, id, Some(ns()), true, ns()).is_ok());
    }

    /// The same validator serves both explicit link fields, so the field
    /// name has to survive into both the message and `field()`.
    #[test]
    fn field_name_propagates_to_message_and_accessor() {
        for field in ["supersedes", "parentId"] {
            let target = MemoryId::new();
            let err = validate_link_target(field, Some(MemoryId::new()), target, None, false, ns())
                .unwrap_err();

            assert_eq!(err.field(), field);
            assert!(err.to_string().starts_with(field));

            let mismatch = validate_link_target(
                field,
                Some(MemoryId::new()),
                target,
                Some(other_ns()),
                true,
                ns(),
            )
            .unwrap_err();

            assert_eq!(mismatch.field(), field);
            assert!(mismatch.to_string().contains(field));
        }
    }
}
