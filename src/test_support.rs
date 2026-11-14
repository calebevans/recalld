//! Shared test fixtures for tests that need live storage.
//!
//! A unit test that only needs a struct builds one inline. This module is for
//! the other kind: tests whose subject is what the *system* does — an edge
//! counter that has to survive a redb write, a namespace statistic that has to
//! agree with what `list_memories` would return. Those need a real
//! [`RedbStorageEngine`] over a real temp directory, and standing one up is
//! ~50 lines that would otherwise be copied per module and drift.
//!
//! Compiled only under `cfg(test)`.

// A fixture accessor is dead code from the point of view of any single test
// module -- the point of the module is that different callers use different
// subsets. Warning on that would just push each addition to grow a fake
// caller.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use crate::graph::SharedGraph;
use crate::model::{DecayPhase, MemoryId, NamespaceId};
use crate::storage::engine::RedbStorageEngine;
use crate::storage::{PersistedEdge, StorageEngine as StorageEngineTrait};

/// Embedding width used by the fixture namespace. Small on purpose:
/// these tests are about bookkeeping, not vectors.
pub(crate) const DIM: usize = 4;

/// The namespace the fixture's memories are stored in by default.
fn ns() -> NamespaceId {
    NamespaceId::new(1)
}

/// What to write for one fixture memory.
///
/// Every field has a sensible default, so a test states only what it is
/// actually about. Use struct-update syntax:
///
/// ```ignore
/// let spec = MemorySpec { tags: &["type/decision"], ..MemorySpec::new(ns) };
/// ```
pub(crate) struct MemorySpec<'a> {
    pub(crate) namespace: NamespaceId,
    pub(crate) summary: &'a str,
    /// Written verbatim to `DiskRecord.tags`, prefixes included — this is
    /// the on-disk form, not the API form.
    pub(crate) tags: &'a [&'a str],
    pub(crate) phase: DecayPhase,
    pub(crate) decay_strength: f32,
    pub(crate) is_permastore: bool,
    pub(crate) edge_count: u16,
    /// The embedding to write. `None` means [`DIM`] zeros, which is what
    /// a test that is not about vectors wants. Set it when the namespace
    /// has a width of its own — `insert_vector` rejects anything that is
    /// not exactly the namespace's width.
    pub(crate) embedding: Option<&'a [f32]>,
}

impl MemorySpec<'_> {
    /// A live, full-phase, full-strength memory with no tags.
    pub(crate) fn new(namespace: NamespaceId) -> Self {
        Self {
            namespace,
            summary: "fixture",
            tags: &[],
            phase: DecayPhase::Full,
            decay_strength: 1.0,
            is_permastore: false,
            edge_count: 0,
            embedding: None,
        }
    }
}

pub(crate) struct Fixture {
    pub(crate) graph: SharedGraph,
    pub(crate) storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    pub(crate) cache: Arc<crate::cache::CacheManager>,
    /// Held on the fixture, not built per adapter call, so a test can
    /// assert on the same index the adapter under test just wrote to.
    pub(crate) vector_index: Arc<tokio::sync::RwLock<crate::search::FlatVectorIndex>>,
    /// See [`vector_index`](Self::vector_index).
    pub(crate) fts_index: Arc<tokio::sync::Mutex<crate::search::FtsIndex>>,
    /// See [`vector_index`](Self::vector_index).
    pub(crate) entity_index: Arc<tokio::sync::RwLock<crate::search::EntityIndex>>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
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

        let fts_index = crate::search::FtsIndex::new(dir.path()).expect("open fts index");

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
            vector_index: Arc::new(tokio::sync::RwLock::new(
                crate::search::FlatVectorIndex::new(DIM),
            )),
            fts_index: Arc::new(tokio::sync::Mutex::new(fts_index)),
            entity_index: Arc::new(tokio::sync::RwLock::new(crate::search::EntityIndex::new())),
            _dir: dir,
        }
    }

    /// The temp directory backing this fixture's storage files. Also the
    /// storage engine's `db_path`, so namespace directories hang
    /// directly off it.
    pub(crate) fn path(&self) -> &Path {
        self._dir.path()
    }

    /// Where a namespace's `vectors.dat` directory lives.
    pub(crate) fn namespace_dir(&self, name: &str) -> std::path::PathBuf {
        self.path().join(name)
    }

    /// Shut this fixture's storage down and hand back the directory,
    /// still alive, for someone else to open.
    ///
    /// This is how a test spells "restart". A [`RedbStorageEngine`]
    /// holds an exclusive `recalld.lock` and a live mmap per namespace
    /// for its entire lifetime, so nothing — not a second engine, not a
    /// whole `Recalld` — can open the same directory until this one is
    /// dropped. Consuming the fixture is what guarantees the drop
    /// happens, and returning the [`tempfile::TempDir`] is what keeps
    /// the files from going with it.
    pub(crate) fn close(self) -> tempfile::TempDir {
        let Self {
            graph,
            storage,
            cache,
            vector_index,
            fts_index,
            entity_index,
            _dir,
        } = self;
        drop((graph, cache, vector_index, fts_index, entity_index));

        // Not a formality: if a test built an adapter and kept it, the
        // engine would outlive this call and the next open would fail
        // with `DatabaseLocked` somewhere far from the cause.
        let engine = Arc::try_unwrap(storage).unwrap_or_else(|_| {
            panic!(
                "close(): something still holds the storage Arc, so the engine's file \
                 locks would outlive this call and the next open would fail"
            )
        });
        drop(engine);

        _dir
    }

    /// Create an extra namespace, so a test can prove a per-namespace
    /// count really is per-namespace.
    pub(crate) fn create_namespace(&self, name: &str) -> NamespaceId {
        self.with_namespace(name, DIM)
    }

    /// [`create_namespace`](Self::create_namespace) with an explicit
    /// embedding width.
    pub(crate) fn with_namespace(&self, name: &str, dim: usize) -> NamespaceId {
        let mut config = crate::model::NamespaceConfig::default_namespace(0);
        config.name = name.to_string();
        config.embedding_dim = dim as u32;
        self.storage
            .write()
            .expect("storage lock")
            .create_namespace(&config)
            .expect("create namespace")
    }

    /// The on-disk record for `id`, or `None` if it was hard-deleted.
    pub(crate) fn record(&self, id: MemoryId) -> Option<crate::model::DiskRecord> {
        self.storage
            .read()
            .expect("storage lock")
            .get_record(id)
            .expect("get record")
    }

    /// Writes a memory to storage and gives it a graph node, which is
    /// the state an explicit edge is applied against.
    pub(crate) async fn insert_memory(&self) -> MemoryId {
        self.insert_memory_with(MemorySpec::new(ns())).await
    }

    /// A live memory in `namespace` carrying `tags`.
    pub(crate) async fn insert_memory_in(&self, namespace: NamespaceId, tags: &[&str]) -> MemoryId {
        self.insert_memory_with(MemorySpec {
            tags,
            ..MemorySpec::new(namespace)
        })
        .await
    }

    /// [`insert_memory`](Self::insert_memory) with every field a test might
    /// need to vary.
    pub(crate) async fn insert_memory_with(&self, spec: MemorySpec<'_>) -> MemoryId {
        let id = MemoryId::new();
        let mut record = crate::model::DiskRecord {
            schema: None,
            version: crate::model::DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: spec.namespace.get(),
            created_at: 0,
            last_accessed_at: 0,
            phase: spec.phase,
            strength: spec.decay_strength,
            decay_strength: spec.decay_strength,
            stability: 1.0,
            difficulty: 5.0,
            is_permastore: u8::from(spec.is_permastore),
            vector_slot: 0,
            edge_count: spec.edge_count,
            summary: spec.summary.into(),
            tags: spec
                .tags
                .iter()
                .map(|t| crate::model::Tag::new(*t).expect("fixture tag is valid"))
                .collect(),
            access_history: Vec::new(),
            text_offset: 0,
            text_length: 0,
        };

        {
            let zeros = [0.0; DIM];
            let embedding = spec.embedding.unwrap_or(&zeros);
            let mut storage_w = self.storage.write().expect("storage lock");
            storage_w
                .insert_memory(id, spec.namespace, &mut record, embedding, None)
                .expect("insert memory");
        }

        let slot = record.vector_slot;
        self.graph
            .write()
            .await
            .add_node(id, spec.namespace, spec.phase, spec.decay_strength, slot)
            .expect("add node");
        id
    }

    /// Delete a memory the way the product does.
    ///
    /// Calls [`RedbStorageEngine::tombstone_memory`], which is what
    /// `McpStorageAdapter::delete_memory` calls — going straight to
    /// `MetadataStore::tombstone` would skip whatever the engine layer does
    /// and leave the fixture in a state the running system never reaches.
    pub(crate) fn tombstone(&self, id: MemoryId) {
        self.storage
            .read()
            .expect("storage lock")
            .tombstone_memory(id)
            .expect("tombstone memory");
    }

    /// The MCP namespace adapter over this fixture's storage.
    pub(crate) fn namespace_adapter(&self) -> crate::mcp::bridge_adapters::McpNamespaceAdapter {
        crate::mcp::bridge_adapters::McpNamespaceAdapter::new(
            self.storage.clone(),
            self.cache.clone(),
            self.vector_index.clone(),
            self.fts_index.clone(),
            self.entity_index.clone(),
            self.graph.clone(),
            chrono_tz::UTC,
        )
    }

    /// The MCP storage adapter over this fixture's storage.
    ///
    /// `McpStorageAdapter::new` takes nine subsystems. They are built
    /// here once — empty and real, not mocked — and the index handles
    /// come off the fixture, so a test can assert against the very index
    /// the adapter wrote to.
    pub(crate) fn storage_adapter(&self) -> crate::mcp::bridge_adapters::McpStorageAdapter {
        crate::mcp::bridge_adapters::McpStorageAdapter::new(
            self.storage.clone(),
            self.cache.clone(),
            Arc::new(crate::embedding::PassthroughProvider::new(DIM)),
            self.vector_index.clone(),
            self.fts_index.clone(),
            self.entity_index.clone(),
            self.graph.clone(),
            Arc::new(crate::config::RecalldConfig::default()),
            chrono_tz::UTC,
        )
    }

    pub(crate) fn edge_count(&self, id: MemoryId) -> u16 {
        self.storage
            .read()
            .expect("storage lock")
            .get_record(id)
            .expect("get record")
            .expect("record exists")
            .edge_count
    }

    pub(crate) fn persisted_edges(&self) -> Vec<PersistedEdge> {
        self.storage
            .read()
            .expect("storage lock")
            .load_all_edges()
            .expect("load edges")
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Stub MCP bridge
// ═══════════════════════════════════════════════════════════════════════

/// A bridge whose every operation panics, except the ones a test asks
/// for.
///
/// The subject is the tool handler's own behaviour — the argument
/// checking and the error rendering it does BEFORE and AFTER the bridge
/// call. `unreachable!()` on everything else is the assertion: a handler
/// that was supposed to reject its arguments and instead ran the query
/// fails loudly rather than silently returning an empty result that
/// looks like a pass.
#[derive(Default)]
pub(crate) struct StubBridge {
    /// What `delete_namespace` returns instead of doing anything.
    pub(crate) delete_namespace_error: Option<crate::mcp::bridge::BridgeError>,
    /// What `delete_memories` returns instead of doing anything.
    /// Deliberately settable to a WRONG length, which is the contract
    /// violation the handler has to notice.
    pub(crate) delete_memories_result: Option<Vec<bool>>,
}

impl StubBridge {
    /// Wrap this stub in an [`McpBridge`](crate::mcp::bridge::McpBridge)
    /// with `default` as the session namespace.
    pub(crate) fn into_bridge(self) -> crate::mcp::bridge::McpBridge {
        let shared = Arc::new(self);
        crate::mcp::bridge::McpBridge {
            search: shared.clone(),
            storage: shared.clone(),
            namespaces: shared.clone(),
            health: shared,
            default_namespace: crate::model::constants::DEFAULT_NAMESPACE_NAME.to_string(),
            timezone: chrono_tz::UTC,
        }
    }
}

#[async_trait::async_trait]
impl crate::mcp::bridge::SearchPipeline for StubBridge {
    async fn search(
        &self,
        _: crate::mcp::bridge::SearchInput,
    ) -> Result<crate::mcp::bridge::SearchResponse, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before searching")
    }
    async fn find_similar(
        &self,
        _: MemoryId,
        _: usize,
        _: Option<f32>,
        _: bool,
    ) -> Result<Vec<crate::mcp::bridge::SearchHit>, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before searching")
    }
    async fn scan_duplicates(
        &self,
        _: &str,
        _: f32,
        _: usize,
    ) -> Result<Vec<crate::mcp::bridge::DuplicateCluster>, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before scanning")
    }
}

#[async_trait::async_trait]
impl crate::mcp::bridge::StorageEngine for StubBridge {
    async fn store_memory(
        &self,
        _: crate::mcp::bridge::StoreInput,
    ) -> Result<crate::mcp::bridge::StoredMemory, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before storing")
    }
    async fn get_memory(
        &self,
        _: MemoryId,
    ) -> Result<Option<crate::mcp::bridge::MemoryRecord>, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before reading")
    }
    async fn delete_memory(&self, _: MemoryId) -> Result<bool, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before deleting")
    }
    async fn delete_memories(
        &self,
        _: &[MemoryId],
    ) -> Result<Vec<bool>, crate::mcp::bridge::BridgeError> {
        match &self.delete_memories_result {
            Some(flags) => Ok(flags.clone()),
            None => {
                unreachable!("the handler was expected to reject its arguments before deleting")
            }
        }
    }
    async fn reinforce_memory(
        &self,
        _: MemoryId,
        _: u8,
    ) -> Result<crate::mcp::bridge::ReinforceResult, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before reinforcing")
    }
    async fn list_memories(
        &self,
        _: crate::mcp::bridge::ListMemoriesInput,
    ) -> Result<crate::mcp::bridge::ListMemoriesResponse, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before listing")
    }
    async fn list_tags(
        &self,
        _: crate::mcp::bridge::ListTagsInput,
    ) -> Result<crate::mcp::bridge::ListTagsResponse, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before listing")
    }
}

#[async_trait::async_trait]
impl crate::mcp::bridge::NamespaceRegistry for StubBridge {
    async fn list_namespaces(
        &self,
    ) -> Result<Vec<crate::mcp::bridge::NamespaceInfo>, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before listing")
    }
    async fn create_namespace(
        &self,
        _: crate::mcp::bridge::CreateNamespaceInput,
    ) -> Result<crate::mcp::bridge::NamespaceInfo, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before creating")
    }
    async fn namespace_stats(
        &self,
        _: &str,
    ) -> Result<crate::mcp::bridge::NamespaceStats, crate::mcp::bridge::BridgeError> {
        unreachable!("the handler was expected to reject its arguments before reading stats")
    }
    async fn delete_namespace(
        &self,
        _: crate::mcp::bridge::DeleteNamespaceInput,
    ) -> Result<crate::mcp::bridge::DeleteNamespaceResult, crate::mcp::bridge::BridgeError> {
        match &self.delete_namespace_error {
            Some(e) => Err(clone_bridge_error(e)),
            None => {
                unreachable!("the handler was expected to reject its arguments before deleting")
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::mcp::bridge::HealthChecker for StubBridge {
    async fn check_health(&self) -> crate::mcp::bridge::HealthStatus {
        unreachable!("no test in this module checks health")
    }
}

/// `BridgeError` is not `Clone`; this is enough of one for a stub.
fn clone_bridge_error(e: &crate::mcp::bridge::BridgeError) -> crate::mcp::bridge::BridgeError {
    use crate::mcp::bridge::BridgeError as E;
    match e {
        E::NotFound(m) => E::NotFound(m.clone()),
        E::InvalidInput(m) => E::InvalidInput(m.clone()),
        E::Storage(m) => E::Storage(m.clone()),
        E::Unavailable(m) => E::Unavailable(m.clone()),
        E::Search(m) => E::Search(m.clone()),
        E::Internal(m) => E::Internal(m.clone()),
        E::TooLarge(m) => E::TooLarge(m.clone()),
        E::PartiallyApplied(m) => E::PartiallyApplied(m.clone()),
    }
}
