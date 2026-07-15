//! Shared application state passed to every handler via axum's `State`
//! extractor.
//!
//! `AppState` is the single point of composition for all subsystems.
//! Handlers never construct subsystem references themselves -- they
//! receive them through `AppState`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::embedding::EmbeddingError;
use crate::graph::GraphError;
use crate::model::decay::DecayPhase;
use crate::model::id::{MemoryId, NamespaceId};
use crate::model::memory::AccessKind;
use crate::model::namespace::NamespaceConfig;
use crate::model::record::{CachedRecord, DiskRecord};
use crate::search::SearchError;
use crate::storage::StorageError;

// ═══════════════════════════════════════════════════════════════════════
// API-layer dependency-injection traits
// ═══════════════════════════════════════════════════════════════════════
//
// These traits define the API server's contracts against its subsystem
// dependencies. They are intentionally narrower than the full subsystem
// interfaces to keep the API layer decoupled and testable. Adapter
// implementations connecting these traits to real subsystems live in
// `api/adapters.rs`.

/// Search pipeline: embedding generation, vector search, and graph
/// expansion.
#[async_trait::async_trait]
pub trait SearchPipeline: Send + Sync {
    /// Generate an embedding for the given text within a namespace.
    async fn embed_text(
        &self,
        text: &str,
        namespace_id: NamespaceId,
    ) -> Result<Vec<f32>, EmbeddingError>;

    /// Execute a search query, returning scored results.
    async fn search(&self, query: SearchQuery) -> Result<Vec<ResolvedSearchResult>, SearchError>;

    /// Index a memory's embedding in the vector index.
    async fn index_memory(&self, id: MemoryId, embedding: &[f32], namespace_id: NamespaceId);

    /// Remove a memory from the vector index.
    async fn remove_from_index(&self, id: MemoryId);

    /// Retrieve the embedding vector for a memory, if indexed.
    fn get_embedding(&self, id: MemoryId) -> Option<Vec<f32>>;

    /// Retrieve embedding vectors for multiple memories in a single
    /// lock acquisition. Returns a map from `MemoryId` to its embedding
    /// vector. IDs not found in the index are omitted from the result.
    fn get_embeddings_batch(&self, ids: &[MemoryId]) -> HashMap<MemoryId, Vec<f32>>;

    /// Return the total number of indexed vectors.
    fn indexed_count(&self) -> usize;

    /// Check whether the embedding provider is healthy.
    async fn embedding_provider_healthy(&self) -> bool;

    /// Add a memory to the FTS5 full-text search index.
    async fn fts_add(
        &self,
        namespace_id: NamespaceId,
        memory_id: MemoryId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
    );

    /// Remove a memory from the FTS5 full-text search index.
    async fn fts_remove(&self, memory_id: MemoryId);

    /// Add a memory to the entity index for entity-based linking.
    async fn entity_index_add(&self, memory_id: MemoryId, entities: &[String]);

    /// Remove a memory from the entity index.
    async fn entity_index_remove(&self, memory_id: MemoryId, entities: &[String]);
}

/// Persistent storage engine (meta.db, fulltext.dat, vectors.dat, edges.db).
#[async_trait::async_trait]
pub trait StorageEngine: Send + Sync {
    /// Create a new memory record and persist it.
    async fn create_memory(
        &self,
        namespace_id: NamespaceId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
        embedding: &[f32],
        initial_stability: Option<f32>,
        created_at: Option<i64>,
    ) -> Result<CachedRecord, StorageError>;

    /// Delete a memory by ID. Returns `true` if it existed.
    async fn delete_memory(&self, id: MemoryId) -> Result<bool, StorageError>;

    /// Retrieve a single memory's DiskRecord by ID.
    ///
    /// Returns `None` if no record exists for this ID.
    async fn get_record(&self, id: MemoryId) -> Option<DiskRecord>;

    /// Retrieve namespace statistics.
    async fn namespace_stats(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<NamespaceStats, StorageError>;

    /// List memories with filters applied.
    ///
    /// Returns `CachedRecord` instances matching all filter criteria.
    /// Results are UNSORTED -- the caller must sort them.
    async fn list_memories(
        &self,
        filter: &crate::api::models::ListFilter,
    ) -> Result<Vec<CachedRecord>, StorageError>;

    /// List memories with server-side filtering and pagination.
    ///
    /// Returns `(matching_records, total_count)` where the records
    /// are the paginated slice and the count is total before pagination.
    async fn list_memories_filtered(
        &self,
        namespace_id: NamespaceId,
        require_tags: &[crate::model::tag::Tag],
        time_range_start: Option<i64>,
        time_range_end: Option<i64>,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<(MemoryId, DiskRecord)>, u64), StorageError>;

    /// Health probe: returns `true` if the storage engine can read/write.
    async fn ping(&self) -> bool;

    /// Load the full_text for a memory from fulltext.dat.
    ///
    /// Returns `Ok(None)` if the memory has no full_text or doesn't exist.
    async fn get_full_text(&self, id: MemoryId) -> Result<Option<String>, StorageError>;

    /// Iterate all records in creation order.
    async fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError>;

    /// Return all (MemoryId, DiskRecord) pairs in a given phase.
    async fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError>;

    /// List all distinct tags with their memory counts.
    async fn list_tags(&self) -> Result<Vec<(String, u64)>, StorageError>;

    /// Return the database directory path for file-size computation.
    fn storage_path(&self) -> Result<PathBuf, StorageError>;
}

/// RAM record cache (moka-backed).
#[async_trait::async_trait]
pub trait RecordCache: Send + Sync {
    /// Look up a record in the cache, or load from storage on miss.
    ///
    /// Returns an `Arc<CachedRecord>` to avoid deep-cloning on every
    /// cache hit. Callers can dereference or clone only when mutation
    /// is needed.
    async fn get_or_load(
        &self,
        id: MemoryId,
        storage: &dyn StorageEngine,
    ) -> Option<Arc<CachedRecord>>;

    /// Insert a record into the cache.
    async fn insert(&self, record: &CachedRecord);

    /// Remove a record from the cache.
    async fn remove(&self, id: MemoryId);

    /// Return the number of entries currently in the cache.
    fn entry_count(&self) -> u64;

    /// Return the cache hit rate as a fraction in [0.0, 1.0].
    fn hit_rate(&self) -> f64;
}

/// Relationship graph for edge traversal.
#[async_trait::async_trait]
pub trait RelationshipGraph: Send + Sync {
    /// Add a directed edge between two memories.
    async fn add_edge(
        &self,
        from: MemoryId,
        to: MemoryId,
        edge_type: &str,
    ) -> Result<(), GraphError>;

    /// Tombstone a memory's graph node (set phase to Tombstone, strength to 0).
    /// Unlike full removal, this preserves the node and edges for graph traversal.
    async fn tombstone_node(&self, id: MemoryId) -> Result<(), GraphError>;

    /// Add a node to the relationship graph for a newly created memory.
    async fn add_node(
        &self,
        id: MemoryId,
        namespace_id: NamespaceId,
        phase: crate::model::decay::DecayPhase,
        strength: f32,
        vector_slot: u32,
    ) -> Result<(), GraphError>;

    /// Perform autolink, entity-link, and temporal-link for a newly created memory.
    /// This is a composite operation that discovers and creates edges to similar,
    /// entity-related, and temporally-adjacent memories.
    async fn perform_post_creation_links(
        &self,
        memory_id: MemoryId,
        namespace_id: NamespaceId,
        embedding: &[f32],
        tags: &[String],
        entities: &[String],
        created_at: i64,
    );
}

/// FSRS decay engine for reinforcement and strength recalculation.
#[async_trait::async_trait]
pub trait FsrsEngine: Send + Sync {
    /// Record an access event for decay tracking.
    async fn record_access(&self, id: MemoryId, kind: AccessKind);

    /// Apply manual reinforcement with the given FSRS quality rating.
    async fn reinforce(
        &self,
        id: MemoryId,
        quality: u8,
    ) -> Result<ReinforceResult, Box<dyn std::error::Error + Send + Sync>>;

    /// Whether the decay sweep thread is still alive.
    fn sweep_thread_alive(&self) -> bool;

    /// Return the time of the last completed decay sweep, if any.
    fn last_sweep_time(&self) -> Option<std::time::Instant>;

    /// Trigger an immediate decay sweep, optionally at a simulated time.
    async fn trigger_sweep(
        &self,
        as_of_millis: Option<i64>,
    ) -> Result<crate::decay::SweepResult, Box<dyn std::error::Error + Send + Sync>>;
}

/// Namespace registry: name <-> id mapping, config lookup.
#[async_trait::async_trait]
pub trait NamespaceRegistry: Send + Sync {
    /// Resolve a namespace by name, returning its config if found.
    fn resolve(&self, name: &str) -> Option<NamespaceConfig>;

    /// Look up a namespace by its integer ID.
    fn get_by_id(&self, id: u32) -> Option<NamespaceConfig>;

    /// Return the human-readable name for a namespace ID.
    fn name_for(&self, id: NamespaceId) -> Option<String>;

    /// List all registered namespaces.
    async fn list_all(&self) -> Vec<NamespaceListInfo>;

    /// Create a new namespace.
    async fn create(
        &self,
        name: &str,
        embedding_dim: Option<u32>,
        initial_stability: f32,
        desired_retention: f32,
        decay_rate_multiplier: Option<f32>,
    ) -> Result<NamespaceConfig, Box<dyn std::error::Error + Send + Sync>>;
}

/// Prometheus-compatible metrics collector.
///
/// No real metrics module exists yet; this trait is a forward-looking
/// contract. A no-op implementation can be used until metrics are added.
#[async_trait::async_trait]
pub trait MetricsCollector: Send + Sync {
    /// Render all metrics in Prometheus exposition format.
    async fn render_prometheus(&self) -> String;
}

// ═══════════════════════════════════════════════════════════════════════
// API-layer data transfer objects
// ═══════════════════════════════════════════════════════════════════════
//
// These types are the API server's own DTOs, deliberately different from
// the internal types (e.g. `crate::search::SearchQuery` has different
// fields). They are not stubs of types that exist elsewhere.

/// Internal search query representation for the API layer.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// The query input (text or pre-computed vector).
    pub query: QueryInput,
    /// Target namespace for the search.
    pub namespace_id: NamespaceId,
    /// Target namespace name (passed to pipeline for namespace-scoped search).
    pub namespace_name: String,
    /// Maximum number of results to return.
    pub k: usize,
    /// Tag inclusion filter (AND semantics).
    pub include_tags: Vec<String>,
    /// Tag exclusion filter.
    pub exclude_tags: Vec<String>,
    /// Optional filter to specific decay phases.
    pub decay_phases: Option<Vec<u8>>,
    /// Minimum score threshold.
    pub min_score: Option<f32>,
    /// Graph expansion depth (0 = direct matches only).
    pub graph_depth: usize,
    /// Whether to apply retrieval-induced forgetting.
    pub apply_rif: bool,
    /// Optional inclusive lower bound for temporal filtering (millis since epoch).
    pub time_range_start: Option<i64>,
    /// Optional inclusive upper bound for temporal filtering (millis since epoch).
    pub time_range_end: Option<i64>,
    /// Named entities for entity overlap scoring.
    pub entities: Vec<String>,
}

/// Search query input: either natural-language text or a pre-computed
/// embedding vector.
#[derive(Debug, Clone)]
pub enum QueryInput {
    /// Natural-language text to be embedded by the server.
    Text(String),
    /// Pre-computed embedding vector.
    Vector(Vec<f32>),
}

/// A single resolved search result with its similarity score.
#[derive(Debug, Clone)]
pub struct ResolvedSearchResult {
    /// The cached memory record.
    pub memory: CachedRecord,
    /// Cosine similarity score.
    pub score: f32,
}

/// Result of a manual reinforcement operation.
#[derive(Debug, Clone)]
pub struct ReinforceResult {
    /// Updated FSRS strength (retrievability).
    pub strength: f32,
    /// Updated FSRS stability in days.
    pub stability: f32,
    /// New decay phase after reinforcement.
    pub phase: crate::model::decay::DecayPhase,
    /// Whether the memory is now in permastore.
    pub is_permastore: bool,
}

/// Statistics for a single namespace, returned by storage.
#[derive(Debug, Clone)]
pub struct NamespaceStats {
    /// Total memory count.
    pub memory_count: u64,
    /// Memories in Full phase (phase 1).
    pub phase_1_count: u64,
    /// Memories in Summary phase (phase 2).
    pub phase_2_count: u64,
    /// Memories in Ghost phase (phase 3).
    pub phase_3_count: u64,
    /// Permastore memory count.
    pub permastore_count: u64,
    /// Average decay strength across all memories.
    pub avg_strength: f32,
    /// Total edge count for memories in this namespace.
    pub edge_count: u64,
}

/// Summary info for a namespace listing.
#[derive(Debug, Clone)]
pub struct NamespaceListInfo {
    /// Namespace integer ID.
    pub id: u32,
    /// Human-readable name.
    pub name: String,
    /// Embedding dimensionality.
    pub embedding_dim: u32,
    /// Total memory count.
    pub memory_count: u64,
    /// Creation timestamp (millis since epoch).
    pub created_at: i64,
}

// ═══════════════════════════════════════════════════════════════════════
// AppState
// ═══════════════════════════════════════════════════════════════════════

/// Shared state passed to every handler via axum's `State` extractor.
///
/// All fields are `Arc`-wrapped trait objects so cloning `AppState` is
/// cheap (pointer copies). This is the single point of composition for
/// all subsystems. Handlers never construct subsystem references
/// themselves -- they receive them through `AppState`.
#[derive(Clone)]
pub struct AppState {
    /// The search pipeline: embedding, vector search, graph expansion.
    pub search: Arc<dyn SearchPipeline>,

    /// Persistent storage engine (meta.db, fulltext.dat, vectors.dat, edges.db).
    pub storage: Arc<dyn StorageEngine>,

    /// RAM record cache (moka-backed).
    pub cache: Arc<dyn RecordCache>,

    /// Relationship graph for edge traversal.
    pub graph: Arc<dyn RelationshipGraph>,

    /// FSRS decay engine for reinforcement and strength recalculation.
    pub decay: Arc<dyn FsrsEngine>,

    /// Namespace registry: name <-> id mapping, config lookup.
    pub namespaces: Arc<dyn NamespaceRegistry>,

    /// Prometheus-compatible metrics collector.
    pub metrics: Arc<dyn MetricsCollector>,

    /// Server start time for uptime reporting in health checks.
    pub started_at: std::time::Instant,
}

impl AppState {
    /// Construct a new `AppState` from pre-initialized subsystems.
    ///
    /// All subsystems must be fully initialized before the API server
    /// starts -- the server does not handle lazy initialization.
    pub fn new(
        search: Arc<dyn SearchPipeline>,
        storage: Arc<dyn StorageEngine>,
        cache: Arc<dyn RecordCache>,
        graph: Arc<dyn RelationshipGraph>,
        decay: Arc<dyn FsrsEngine>,
        namespaces: Arc<dyn NamespaceRegistry>,
        metrics: Arc<dyn MetricsCollector>,
    ) -> Self {
        Self {
            search,
            storage,
            cache,
            graph,
            decay,
            namespaces,
            metrics,
            started_at: std::time::Instant::now(),
        }
    }
}
