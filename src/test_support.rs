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
            let mut storage_w = self.storage.write().expect("storage lock");
            storage_w
                .insert_memory(id, spec.namespace, &mut record, &[0.0; DIM], None)
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
        crate::mcp::bridge_adapters::McpNamespaceAdapter::new(self.storage.clone(), chrono_tz::UTC)
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
