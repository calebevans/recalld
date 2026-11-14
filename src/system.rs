//! Root system object for Recalld.
//!
//! The [`Recalld`] struct owns or holds `Arc` references to every
//! subsystem. Constructed by [`Recalld::new()`], which executes
//! the ordered startup sequence. Torn down by [`Recalld::shutdown()`].

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, RwLock, watch};
use tokio::task::JoinHandle;

use crate::cache::manager::{CacheConfig as CacheMgrConfig, CacheManager};
use crate::config::RecalldConfig;
use crate::decay::config::DecayConfig;
use crate::decay::sweep::{DecaySweepRunner, SweepConfig};
use crate::embedding::{self, EmbeddingProvider};
use crate::error::{RecalldError, Result};
use crate::graph::activation::ActivationConfig;
use crate::graph::{self, RelationshipGraph, SharedGraph};
use crate::model::{DecayPhase, DiskRecord, MemoryId, NamespaceId};
use crate::rif::{RifConfig, RifEngine};
use crate::search::{
    EntityIndex, FlatVectorIndex, FtsIndex, QueryEngine, VectorError, VectorIndex as _,
    VectorMetadata,
};
use crate::storage::engine::RedbStorageEngine;
// Import the StorageEngine trait so its methods are in scope.
use crate::storage::StorageEngine as _;

// ═══════════════════════════════════════════════════════════════════════
// Startup vector-index load
// ═══════════════════════════════════════════════════════════════════════

/// What the startup vector load did.
///
/// Counts rather than per-record logging: a store with a changed
/// embedding width would otherwise emit one warning per memory, and the
/// one number an operator needs — how many — would be the one thing not
/// on the line.
#[derive(Debug, Default, PartialEq, Eq)]
struct VectorLoadStats {
    /// Vectors added to the index.
    loaded: usize,
    /// Records skipped for being tombstones. Tombstones have had their
    /// content stripped and are deliberately absent from every search
    /// index; loading one would resurrect it as a vector hit.
    tombstoned: usize,
    /// Records whose `vector_slot` points past the end of their
    /// namespace's `vectors.dat`.
    missing: usize,
    /// Records whose namespace's vector file could not be read at all —
    /// in practice, a namespace directory that is gone or was never
    /// opened.
    unreadable: usize,
    /// Records whose stored vector is a different width than the index.
    dim_mismatch: usize,
    /// Namespaces that produced at least one [`dim_mismatch`], sorted so
    /// the log line is stable across boots.
    ///
    /// [`dim_mismatch`]: VectorLoadStats::dim_mismatch
    mismatched_namespaces: BTreeSet<u32>,
    /// Namespaces that produced at least one [`unreadable`] record.
    ///
    /// [`unreadable`]: VectorLoadStats::unreadable
    unreadable_namespaces: BTreeSet<u32>,
}

impl VectorLoadStats {
    /// Every record that did not make it into the index, for the
    /// one-line summary.
    fn skipped(&self) -> usize {
        self.tombstoned + self.missing + self.unreadable + self.dim_mismatch
    }
}

/// Fill `index` from the vectors already on disk.
///
/// [`FlatVectorIndex`] is pure RAM. It is created empty, and until this
/// existed nothing but the store path ever put anything in it — so every
/// restart began with an empty index and stayed empty until the process
/// stored its first memory. `find_similar_memories` failed outright
/// ("no embedding found for memory <id>"), and `recall_memories`
/// silently degraded to FTS, entity and graph matching, which returns
/// accurate-looking results that ignore semantic similarity entirely.
/// This load is what makes a restart invisible.
///
/// # Nothing here is fatal
///
/// A missing vector, an unreadable namespace, a width that does not
/// match: all are counted and skipped. The consequence of skipping is
/// that one memory is not reachable by vector search — which is
/// precisely the state it was in before this function existed, so
/// failing startup over it would trade a degraded system for no system.
///
/// `records` is the caller's existing `scan_all()`. This function does
/// not scan; it reads one vector per live record.
fn load_vector_index(
    storage: &RedbStorageEngine,
    records: &[(MemoryId, DiskRecord)],
    index: &mut FlatVectorIndex,
) -> VectorLoadStats {
    let mut stats = VectorLoadStats::default();

    // Size the buffer from the live records, not all of them. A store
    // that is mostly tombstones would otherwise reserve — and actually
    // allocate — several hundred MB it never uses.
    let live = records
        .iter()
        .filter(|(_, r)| r.phase != DecayPhase::Tombstone)
        .count();
    index.reserve(live);

    for (memory_id, record) in records {
        if record.phase == DecayPhase::Tombstone {
            stats.tombstoned += 1;
            continue;
        }

        let namespace_id = NamespaceId::new(record.namespace_id);
        let vector = match storage.get_vector(namespace_id, record.vector_slot) {
            Ok(Some(v)) => v,
            Ok(None) => {
                stats.missing += 1;
                continue;
            }
            Err(_) => {
                stats.unreadable += 1;
                stats.unreadable_namespaces.insert(record.namespace_id);
                continue;
            }
        };

        let metadata = VectorMetadata {
            namespace_id,
            // The phase the record ACTUALLY has. The store path writes a
            // hardcoded `Full` and `VectorIndex::update_metadata` has no
            // callers, so before this every index-level phase filter
            // matched everything and was in effect inert.
            //
            // KNOWN GAP, stated plainly: this makes the filter correct
            // only as of the last restart. A memory that transitions
            // Full -> Summary -> Ghost while the process is running
            // still carries its boot-time phase in the index, because
            // the decay sweep holds no handle on the vector index.
            // Giving it one is a separate change; do not paper over it
            // here.
            decay_phase: record.phase.as_u8(),
            // From the record, not from the store request. `DiskRecord`
            // tags are already normalized `Tag`s, which is the same form
            // the query side derives its filter tags in — so a tag
            // filter behaves the same before and after a restart.
            tags: record.tags.iter().map(|t| t.to_string()).collect(),
        };

        match index.add(*memory_id, &vector, metadata) {
            Ok(()) => stats.loaded += 1,
            Err(VectorError::DimensionMismatch { .. }) => {
                stats.dim_mismatch += 1;
                stats.mismatched_namespaces.insert(record.namespace_id);
            }
            Err(e) => {
                // `add` has no other failure mode today. If it grows
                // one, count it where an operator will see it rather
                // than dropping the record silently.
                stats.unreadable += 1;
                stats.unreadable_namespaces.insert(record.namespace_id);
                tracing::warn!(memory_id = %memory_id, %e, "vector rejected by the index");
            }
        }
    }

    stats
}

/// System readiness state. Checked by the health endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemState {
    /// All subsystems nominal.
    Ready,
    /// Running but some features degraded (e.g., no graph).
    Degraded {
        /// Why the system is degraded.
        reason: &'static str,
    },
    /// Shutting down, not accepting new requests.
    ShuttingDown,
}

/// Root system object. Owns or holds `Arc` references to every
/// subsystem. Constructed by `Recalld::new()`, which executes
/// the ordered startup sequence. Torn down by `Recalld::shutdown()`.
///
/// Only one `Recalld` instance should exist per process. The
/// exclusive file locks on storage files enforce this at the OS level.
pub struct Recalld {
    clock: Arc<dyn crate::time::Clock>,
    // -- Configuration ------------------------------------------------
    config: Arc<RecalldConfig>,

    // -- Storage ------------------------------------------------------
    /// Unified storage engine (meta.db, fulltext.dat, vectors.dat, edges.db).
    /// Wrapped in Arc<std::sync::RwLock> because RedbStorageEngine
    /// has &mut self methods (insert_memory, delete_memory, compact,
    /// sync). Read methods use &self.
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,

    // -- Graph --------------------------------------------------------
    /// In-memory relationship graph (Arc<tokio::sync::RwLock<RelationshipGraph>>).
    graph: SharedGraph,

    // -- Cache --------------------------------------------------------
    /// RAM cache with moka-backed eviction.
    cache: Arc<CacheManager>,

    // -- Embedding ----------------------------------------------------
    /// Pluggable embedding provider (trait object behind Arc).
    embedding: Arc<dyn EmbeddingProvider>,

    // -- RIF ----------------------------------------------------------
    /// Retrieval-induced forgetting engine.
    rif_engine: Arc<RifEngine>,

    // -- Search -------------------------------------------------------
    /// Flat vector index for SIMD similarity search.
    vector_index: Arc<RwLock<FlatVectorIndex>>,

    /// FTS5 full-text search index (single SQLite database for all namespaces).
    fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,

    /// Entity inverted index for entity-based graph edges.
    entity_index: Arc<RwLock<EntityIndex>>,

    /// Query engine orchestrating the 9-step search pipeline.
    query_engine: Arc<QueryEngine>,

    // -- Background tasks ---------------------------------------------
    /// Decay sweep runner (owns its own shutdown channel).
    decay_runner: Option<DecaySweepRunner>,

    /// Decay sweep background task handle.
    decay_handle: Option<JoinHandle<()>>,

    // -- Shutdown coordination ----------------------------------------
    shutdown_tx: watch::Sender<bool>,
    drain_notify: Arc<Notify>,
    inflight_count: Arc<AtomicU32>,
    state: SystemState,
}

impl Recalld {
    /// Construct and initialize a Recalld instance.
    ///
    /// Executes the startup sequence in order. Any failure at any step
    /// returns `RecalldError::Init` with the step name and cause.
    /// Resources acquired before the failure are cleaned up by normal
    /// `Drop` behavior.
    pub async fn new(config: RecalldConfig) -> Result<Self> {
        Self::with_clock(config, Arc::new(crate::time::SystemClock)).await
    }

    /// Construct an isolated instance with a shared lifecycle clock.
    /// Search-pipeline exposure clock injection is a separate A2 handoff.
    pub async fn with_clock(
        config: RecalldConfig,
        clock: Arc<dyn crate::time::Clock>,
    ) -> Result<Self> {
        let config = Arc::new(config);
        let (shutdown_tx, _) = watch::channel(false);
        let drain_notify = Arc::new(Notify::new());
        let inflight_count = Arc::new(AtomicU32::new(0));

        tracing::info!("starting Recalld");

        // -- Step 1: Open storage engine ------------------------------
        let storage = {
            let data_dir = &config.storage.data_dir;
            tracing::info!(data_dir = %data_dir, "opening storage engine");
            let engine = RedbStorageEngine::open(data_dir).map_err(|e| RecalldError::Init {
                step: "open_storage",
                message: format!("failed to open storage at {}: {}", data_dir, e),
                source: Some(Box::new(e)),
            })?;
            tracing::info!("storage engine opened");
            Arc::new(std::sync::RwLock::new(engine))
        };

        let effects_rebuild = storage
            .read()
            .map_err(|e| RecalldError::Init {
                step: "effects_rebuild",
                message: e.to_string(),
                source: None,
            })?
            .meta_store()
            .needs_effects_rebuild();

        // -- Step 2: Scan all records once, reuse for graph / FTS / entity index
        let all_records = {
            let storage_r = storage.read().map_err(|e| RecalldError::Init {
                step: "lock_storage_for_scan",
                message: format!("storage lock poisoned: {}", e),
                source: None,
            })?;
            storage_r.scan_all().map_err(|e| RecalldError::Init {
                step: "scan_all_records",
                message: "failed to scan meta.db".into(),
                source: Some(Box::new(e)),
            })?
        };

        // -- Step 2b: Build relationship graph from scanned records ---
        let graph: SharedGraph = {
            let storage_r = storage.read().map_err(|e| RecalldError::Init {
                step: "lock_storage_for_graph",
                message: format!("storage lock poisoned: {}", e),
                source: None,
            })?;

            let mut rel_graph = RelationshipGraph::with_capacity(all_records.len());

            for (memory_id, record) in &all_records {
                let namespace_id = crate::model::NamespaceId::new(record.namespace_id);
                let phase = record.phase;
                // Silently skip duplicates during startup load.
                let _ = rel_graph.add_node(
                    *memory_id,
                    namespace_id,
                    phase,
                    record.decay_strength,
                    record.vector_slot,
                );
            }

            // Load all edges from edges.db.
            let persisted_edges = storage_r.load_all_edges().map_err(|e| RecalldError::Init {
                step: "load_graph_edges",
                message: "failed to load edges from edges.db".into(),
                source: Some(Box::new(e)),
            })?;

            // After CS-23, graph::PersistedEdge == storage::PersistedEdge
            // (with created_at: u64). Pass them directly -- no field mapping needed.
            let expected_edges = persisted_edges.len();
            let edge_count =
                graph::rebuild_from_storage(&mut rel_graph, persisted_edges.into_iter());

            if effects_rebuild && edge_count != expected_edges {
                return Err(RecalldError::Init {
                    step: "effects_graph_rebuild",
                    message: "graph did not accept all canonical edges".into(),
                    source: None,
                });
            }
            let stats = rel_graph.stats();
            tracing::info!(
                nodes = stats.node_count,
                edges = stats.edge_count,
                loaded_edges = edge_count,
                avg_degree = %format!("{:.1}", stats.avg_degree),
                "relationship graph loaded"
            );

            Arc::new(RwLock::new(rel_graph))
        };

        // -- Step 3: Initialize cache manager -------------------------
        let cache = {
            let cache_cfg = CacheMgrConfig {
                max_capacity_bytes: config.cache.max_capacity_bytes,
                time_to_idle: Some(Duration::from_secs(config.cache.time_to_idle_secs)),
                time_to_live: Some(Duration::from_secs(config.cache.time_to_live_secs)),
                embedding_dim: config.embedding.dimensions,
            };
            // No external eviction listener for now.
            let mgr = CacheManager::new(cache_cfg, None);
            mgr.bind_storage(&storage);
            tracing::info!("cache manager initialized");
            Arc::new(mgr)
        };

        // -- Step 4: Create embedding provider ------------------------
        let embedding: Arc<dyn EmbeddingProvider> = {
            let emb_config = embedding::EmbeddingConfig {
                provider: match config.embedding.provider {
                    crate::config::EmbeddingProvider::OpenAI => embedding::ProviderType::OpenAI,
                    crate::config::EmbeddingProvider::Ollama => embedding::ProviderType::Ollama,
                    crate::config::EmbeddingProvider::Passthrough => {
                        embedding::ProviderType::Passthrough
                    }
                    crate::config::EmbeddingProvider::Bedrock => embedding::ProviderType::Bedrock,
                },
                dimensions: config.embedding.dimensions,
                model: config.embedding.model_name.clone(),
                api_key: std::env::var(&config.embedding.api_key_env).ok(),
                base_url: Some(config.embedding.base_url.clone()),
                batch_size: Some(config.embedding.batch_size),
                timeout_secs: None,
                region: Some(config.embedding.region.clone()),
                cache_embeddings: false,
                cache_max_entries: 1000,
            };
            let provider = embedding::build_provider(
                &emb_config,
                &config.embedding.document_prefix,
                &config.embedding.query_prefix,
            )
            .await
            .map_err(|e| RecalldError::Init {
                step: "create_embedding_provider",
                message: format!("failed to create embedding provider: {}", e,),
                source: None,
            })?;
            tracing::info!(
                provider = ?config.embedding.provider,
                model = %config.embedding.model_name,
                dimensions = config.embedding.dimensions,
                document_prefix = %config.embedding.document_prefix,
                query_prefix = %config.embedding.query_prefix,
                "embedding provider ready"
            );
            Arc::from(provider)
        };

        // -- Step 5: Create RIF engine --------------------------------
        let rif_engine = {
            let rif_cfg = RifConfig {
                enabled: config.rif.enabled,
                gamma: 0.3,
                activation_low: config.rif.activation_threshold_low as f32,
                activation_high: config.rif.activation_threshold_high as f32,
                max_suppression: config.rif.max_suppression as f32,
                max_enhancement: 0.05,
                max_hops: config.rif.propagation_depth,
                stability_floor: 0.5,
                max_neighbors: 100,
                max_reduction_per_query: 0.75,
            };
            let engine = RifEngine::new(rif_cfg);
            tracing::info!(enabled = config.rif.enabled, "RIF engine initialized");
            Arc::new(engine)
        };

        // -- Step 6: Create FlatVectorIndex and load persisted vectors -
        let vector_index = {
            let mut index = FlatVectorIndex::new(config.embedding.dimensions);

            // The index lives in RAM only; without this the process
            // starts semantically blind and stays that way until it
            // stores something. See `load_vector_index`.
            let started = std::time::Instant::now();
            let stats = match storage.read() {
                Ok(storage_r) => load_vector_index(&storage_r, &all_records, &mut index),
                Err(e) => {
                    // Unreachable — step 2 already took this lock and
                    // would have failed first. Degrade rather than
                    // refuse to boot, on principle: an empty vector
                    // index is the old behaviour, not a new outage.
                    tracing::warn!(%e, "storage lock poisoned; vector index left empty");
                    VectorLoadStats::default()
                }
            };
            if effects_rebuild && (stats.dim_mismatch + stats.unreadable + stats.missing > 0) {
                return Err(RecalldError::Init {
                    step: "effects_vector_rebuild",
                    message: "vector rebuild incomplete; effects remain pending".into(),
                    source: None,
                });
            }
            let elapsed = started.elapsed();

            if stats.dim_mismatch > 0 {
                tracing::warn!(
                    skipped = stats.dim_mismatch,
                    index_dimensions = config.embedding.dimensions,
                    namespaces = ?stats.mismatched_namespaces,
                    "stored vectors are a different width than the vector index and were \
                     skipped; those memories are unreachable by vector search. Either \
                     embedding.dimensions changed since they were written, or the namespace \
                     was created with its own embeddingDim"
                );
            }
            if stats.unreadable > 0 {
                tracing::warn!(
                    skipped = stats.unreadable,
                    namespaces = ?stats.unreadable_namespaces,
                    "vectors could not be read and were skipped; those memories are \
                     unreachable by vector search"
                );
            }
            if stats.missing > 0 {
                tracing::warn!(
                    skipped = stats.missing,
                    "records point at vector slots that do not exist and were skipped; \
                     those memories are unreachable by vector search"
                );
            }

            tracing::info!(
                dimensions = config.embedding.dimensions,
                loaded = stats.loaded,
                skipped = stats.skipped(),
                skipped_tombstoned = stats.tombstoned,
                skipped_missing_slot = stats.missing,
                skipped_unreadable = stats.unreadable,
                skipped_dimension_mismatch = stats.dim_mismatch,
                elapsed_ms = elapsed.as_millis() as u64,
                "flat vector index created and loaded from storage"
            );
            Arc::new(RwLock::new(index))
        };

        // -- Step 6b: Open FTS5 index ------------------------------------
        let fts_index = {
            let data_dir = &config.storage.data_dir;
            let data_dir_path = std::path::Path::new(data_dir);
            let fts_path = data_dir_path.join("fts.db");
            let is_new = !fts_path.exists();

            let fts = FtsIndex::new(data_dir_path).map_err(|e| RecalldError::Init {
                step: "open_fts_index",
                message: format!("failed to open FTS5 index: {}", e),
                source: Some(Box::new(e)),
            })?;

            // Migration: if the FTS5 database is newly created (no existing
            // fts.db file), populate it from all records in redb storage.
            // Also handles the case where fts.db exists but is empty
            // (e.g., previous crash during first migration).
            if effects_rebuild {
                fts.clear().map_err(|e| RecalldError::Init {
                    step: "effects_fts_clear",
                    message: e.to_string(),
                    source: Some(Box::new(e)),
                })?;
            }
            if effects_rebuild || is_new || fts.is_empty().unwrap_or(true) {
                if !all_records.is_empty() {
                    let storage_r = storage.read().map_err(|e| RecalldError::Init {
                        step: "fts_migration",
                        message: format!("storage lock poisoned: {}", e),
                        source: None,
                    })?;
                    tracing::info!(
                        records = all_records.len(),
                        "migrating records to FTS5 index"
                    );
                    for (memory_id, record) in &all_records {
                        if record.phase == DecayPhase::Tombstone {
                            continue;
                        }
                        let namespace_id = crate::model::NamespaceId::new(record.namespace_id);
                        let tag_strings: Vec<String> =
                            record.tags.iter().map(|t| t.to_string()).collect();
                        // Retrieve full_text from the text log if available.
                        let full_text = if record.text_length > 0 {
                            let text_ref = crate::storage::TextRef {
                                file_offset: record.text_offset,
                                length: record.text_length,
                            };
                            storage_r
                                .get_text(text_ref)
                                .map_err(|e| RecalldError::Init {
                                    step: "fts_text",
                                    message: e.to_string(),
                                    source: Some(Box::new(e)),
                                })?
                        } else {
                            None
                        };
                        fts.add(
                            namespace_id,
                            *memory_id,
                            &record.summary,
                            full_text.as_deref(),
                            &tag_strings,
                        )
                        .map_err(|e| RecalldError::Init {
                            step: "fts_rebuild",
                            message: e.to_string(),
                            source: Some(Box::new(e)),
                        })?;
                    }
                    drop(storage_r);
                    tracing::info!(indexed = all_records.len(), "FTS5 migration complete");
                }
            }

            // Clean up old BM25 binary files from disk.
            if let Ok(entries) = std::fs::read_dir(data_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("bm25_") && name.ends_with(".bin") {
                            if let Err(e) = std::fs::remove_file(&path) {
                                tracing::warn!(
                                    path = %path.display(),
                                    %e,
                                    "failed to remove old BM25 index file"
                                );
                            } else {
                                tracing::info!(
                                    path = %path.display(),
                                    "removed old BM25 index file"
                                );
                            }
                        }
                    }
                }
            }

            tracing::info!("FTS5 index ready");
            Arc::new(tokio::sync::Mutex::new(fts))
        };

        // -- Step 6c: Build entity index from scanned records ----------
        let entity_index = {
            let mut idx = EntityIndex::with_capacity(all_records.len());
            for (memory_id, record) in &all_records {
                let entities = crate::model::parse_structured_tags(&record.tags).entities;
                if !entities.is_empty() {
                    idx.add(*memory_id, &entities);
                }
            }
            tracing::info!(memories = idx.len(), "entity index built");
            Arc::new(RwLock::new(idx))
        };
        // All startup graph/vector/FTS/entity/cache construction has completed.
        storage
            .read()
            .map_err(|e| RecalldError::Init {
                step: "effects_ack",
                message: e.to_string(),
                source: None,
            })?
            .meta_store()
            .acknowledge_rebuilt_effects()
            .map_err(|e| RecalldError::Init {
                step: "effects_ack",
                message: e.to_string(),
                source: Some(Box::new(e)),
            })?;

        // -- Step 7: Create QueryEngine -------------------------------
        let query_engine = {
            use crate::search::adapters::*;

            let ns_resolver: Arc<dyn crate::search::NamespaceResolver> =
                Arc::new(StorageNamespaceResolver::new(storage.clone()));
            let emb_registry: Arc<dyn crate::search::EmbeddingProviderRegistry> =
                Arc::new(SingleEmbeddingRegistry::new(embedding.clone()));
            let vec_registry: Arc<dyn crate::search::VectorIndexRegistry> =
                Arc::new(SharedVectorIndexRegistry::new(vector_index.clone()));
            let fts_registry: Arc<dyn crate::search::FtsIndexRegistry> =
                Arc::new(SharedFtsIndexRegistry::new(fts_index.clone()));
            let entity_index_reader: Arc<dyn crate::search::EntityIndexReader> =
                Arc::new(SharedEntityIndexReader::new(entity_index.clone()));
            let record_cache: Arc<dyn crate::search::RecordCache> =
                Arc::new(CacheManagerAdapter::new(cache.clone()));
            let meta_store: Arc<dyn crate::search::MetadataStore> =
                Arc::new(StorageMetadataAdapter::new(storage.clone()));
            let graph_reader: Arc<dyn crate::search::GraphReader> =
                Arc::new(SharedGraphReader::new(graph.clone()));
            let rif_proc: Arc<dyn crate::search::RifProcessor> =
                Arc::new(RifProcessorAdapter::new(rif_engine.clone(), graph.clone()));
            let access_rec: Arc<dyn crate::search::AccessRecorder> =
                Arc::new(StorageAccessRecorder::new(storage.clone()));

            let engine = QueryEngine::new(
                ns_resolver,
                emb_registry,
                vec_registry,
                fts_registry,
                entity_index_reader,
                record_cache,
                meta_store,
                graph_reader,
                rif_proc,
                access_rec,
            );
            tracing::info!("query engine initialized");
            Arc::new(engine)
        };

        // -- Step 8: Start decay sweep runner -------------------------
        let (decay_runner, decay_handle) = if config.decay.disable_sweep {
            tracing::info!("decay sweep disabled by config");
            (None, None)
        } else {
            let sweep_cfg = SweepConfig {
                interval: Duration::from_secs_f64(config.decay.sweep_interval_hours * 3600.0),
                sweep_on_startup: true,
                ..SweepConfig::default()
            };
            let decay_cfg = Arc::new(DecayConfig::default());
            let activation_cfg = ActivationConfig::default();

            match DecaySweepRunner::new(
                sweep_cfg,
                decay_cfg,
                activation_cfg,
                storage.clone(),
                graph.clone(),
                cache.clone(),
                config.decay.decay_rate_multiplier,
            ) {
                Ok(runner) => {
                    let runner =
                        runner.with_completion(crate::derived_completion::DerivedCompletion {
                            storage: storage.clone(),
                            cache: cache.clone(),
                            graph: graph.clone(),
                            vectors: vector_index.clone(),
                            fts: fts_index.clone(),
                            entities: entity_index.clone(),
                        });
                    let runner = runner.with_clock(clock.clone());
                    let handle = runner.start();
                    tracing::info!("decay sweep runner started");
                    (Some(runner), Some(handle))
                }
                Err(e) => {
                    tracing::warn!(
                        %e,
                        "decay sweep runner failed to start, continuing without sweeps"
                    );
                    (None, None)
                }
            }
        };

        // -- Step 9: Ensure "default" namespace exists ----------------
        {
            let storage_r = storage.read().map_err(|e| RecalldError::Init {
                step: "check_default_namespace",
                message: format!("storage lock poisoned: {}", e),
                source: None,
            })?;
            let has_default = storage_r
                .get_namespace_by_name(crate::model::constants::DEFAULT_NAMESPACE_NAME)
                .map_err(|e| RecalldError::Init {
                    step: "check_default_namespace",
                    message: format!("failed to check default namespace: {}", e),
                    source: Some(Box::new(e)),
                })?
                .is_some();
            drop(storage_r);

            if !has_default {
                let ns_config = crate::model::NamespaceConfig {
                    schema: None,
                    id: crate::model::NamespaceId::UNSET,
                    name: crate::model::constants::DEFAULT_NAMESPACE_NAME.to_string(),
                    embedding_dim: config.embedding.dimensions as u32,
                    initial_stability: 3.7145,
                    default_difficulty: 5.0,
                    phase_thresholds: crate::model::namespace::PhaseThresholds::default(),
                    permastore_threshold: 1500.0,
                    created_at: clock.now_millis(),
                    desired_retention: 0.9,
                    decay_rate_multiplier: None,
                };
                let mut storage_w = storage.write().map_err(|e| RecalldError::Init {
                    step: "create_default_namespace",
                    message: format!("storage lock poisoned: {}", e),
                    source: None,
                })?;
                let ns_id =
                    storage_w
                        .create_namespace(&ns_config)
                        .map_err(|e| RecalldError::Init {
                            step: "create_default_namespace",
                            message: format!("failed to create default namespace: {}", e,),
                            source: Some(Box::new(e)),
                        })?;
                tracing::info!(
                    id = ns_id.get(),
                    dimensions = config.embedding.dimensions,
                    "created default namespace"
                );
            }
        }

        tracing::info!("Recalld initialized");

        Ok(Self {
            clock,
            config,
            storage,
            graph,
            cache,
            embedding,
            rif_engine,
            vector_index,
            fts_index,
            entity_index,
            query_engine,
            decay_runner,
            decay_handle,
            shutdown_tx,
            drain_notify,
            inflight_count,
            state: SystemState::Ready,
        })
    }

    /// Shared clock for creation, feedback and sweeps in this instance.
    pub fn clock(&self) -> &Arc<dyn crate::time::Clock> {
        &self.clock
    }

    // -- Accessor methods ---------------------------------------------

    /// Read-only access to the configuration.
    pub fn config(&self) -> &RecalldConfig {
        &self.config
    }

    /// Access the storage engine (behind std::sync::RwLock).
    pub fn storage(&self) -> &Arc<std::sync::RwLock<RedbStorageEngine>> {
        &self.storage
    }

    /// Access the in-memory relationship graph.
    pub fn graph(&self) -> &SharedGraph {
        &self.graph
    }

    /// Access the RAM cache manager.
    pub fn cache(&self) -> &Arc<CacheManager> {
        &self.cache
    }

    /// Access the embedding provider.
    pub fn embedding(&self) -> &Arc<dyn EmbeddingProvider> {
        &self.embedding
    }

    /// Access the RIF engine.
    pub fn rif_engine(&self) -> &Arc<RifEngine> {
        &self.rif_engine
    }

    /// Access the flat vector index.
    pub fn vector_index(&self) -> &Arc<RwLock<FlatVectorIndex>> {
        &self.vector_index
    }

    /// Retry publication of committed effects without restarting. Errors leave
    /// query admission closed; canonical application failures require reopen.
    pub async fn complete_pending_effects(
        &self,
    ) -> std::result::Result<(), crate::storage::StorageError> {
        crate::derived_completion::DerivedCompletion {
            storage: self.storage.clone(),
            cache: self.cache.clone(),
            graph: self.graph.clone(),
            vectors: self.vector_index.clone(),
            fts: self.fts_index.clone(),
            entities: self.entity_index.clone(),
        }
        .complete()
        .await
    }

    /// Access the FTS5 full-text search index.
    pub fn fts_index(&self) -> &Arc<tokio::sync::Mutex<FtsIndex>> {
        &self.fts_index
    }

    /// Access the entity inverted index.
    pub fn entity_index(&self) -> &Arc<RwLock<EntityIndex>> {
        &self.entity_index
    }

    /// Access the query engine.
    pub fn query_engine(&self) -> &Arc<QueryEngine> {
        &self.query_engine
    }

    /// In-flight request counter for shutdown coordination.
    pub fn inflight_count(&self) -> &Arc<AtomicU32> {
        &self.inflight_count
    }

    /// Drain notification for shutdown coordination.
    pub fn drain_notify(&self) -> &Arc<Notify> {
        &self.drain_notify
    }

    /// Subscribe to the shutdown signal (for API middleware).
    pub fn shutdown_rx(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Current system readiness state.
    pub fn state(&self) -> SystemState {
        self.state
    }

    // -- Serve --------------------------------------------------------

    /// Install OS signal handlers and run the API server.
    /// Returns when shutdown is complete.
    pub async fn serve(self, bind_addr: SocketAddr) -> Result<()> {
        tracing::info!(%bind_addr, "API server listening");

        // Wait for shutdown signal.
        Self::shutdown_signal(self.shutdown_tx.clone()).await;

        // Server has stopped. Run shutdown sequence.
        self.shutdown().await
    }

    /// Wait for SIGTERM or SIGINT, then broadcast shutdown.
    async fn shutdown_signal(shutdown_tx: watch::Sender<bool>) {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        let terminate = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {
                tracing::info!("received SIGINT, initiating shutdown");
            }
            _ = terminate => {
                tracing::info!("received SIGTERM, initiating shutdown");
            }
        }

        let _ = shutdown_tx.send(true);
    }

    // -- Shutdown -----------------------------------------------------

    /// Execute the graceful shutdown sequence.
    ///
    /// Steps (ordered, with timeouts):
    /// 1. Signal all background tasks to stop
    /// 2. Drain in-flight API requests (5-second timeout)
    /// 3. Stop decay sweep runner and join its task
    /// 4. Flush and sync all storage files
    /// 5. Persist phase index to disk
    /// 6. Drop subsystems in reverse init order (handled by Rust's struct drop)
    async fn shutdown(self) -> Result<()> {
        tracing::info!("shutdown sequence started");
        let shutdown_start = std::time::Instant::now();

        // Step 1: Signal background tasks.
        let _ = self.shutdown_tx.send(true);

        // Step 2: Drain in-flight requests.
        let drain_timeout = Duration::from_secs(5);
        if self.inflight_count.load(Ordering::Acquire) > 0 {
            tracing::info!(
                inflight = self.inflight_count.load(Ordering::Relaxed),
                timeout_secs = drain_timeout.as_secs(),
                "waiting for in-flight requests to drain"
            );
            let drain_result =
                tokio::time::timeout(drain_timeout, self.drain_notify.notified()).await;
            match drain_result {
                Ok(()) => tracing::info!("all in-flight requests drained"),
                Err(_) => {
                    let remaining = self.inflight_count.load(Ordering::Relaxed);
                    tracing::warn!(
                        remaining,
                        "drain timeout exceeded, proceeding with shutdown"
                    );
                }
            }
        }

        // Step 3: Stop decay sweep runner.
        if let Some(ref runner) = self.decay_runner {
            runner.shutdown();
        }
        if let Some(handle) = self.decay_handle {
            let task_timeout = Duration::from_secs(10);
            match tokio::time::timeout(task_timeout, handle).await {
                Ok(Ok(())) => {
                    tracing::debug!("decay sweep task stopped")
                }
                Ok(Err(e)) => {
                    tracing::warn!(%e, "decay sweep task panicked")
                }
                Err(_) => {
                    tracing::warn!("decay sweep task did not stop within timeout")
                }
            }
        }

        // Step 4: Flush and sync storage.
        {
            let mut storage_w = self.storage.write().map_err(|e| RecalldError::Shutdown {
                message: format!("storage lock poisoned: {}", e),
                source: None,
            })?;
            match storage_w.sync() {
                Ok(()) => tracing::info!("storage synced"),
                Err(e) => tracing::error!(
                    %e,
                    "storage sync failed during shutdown"
                ),
            }
        }

        // Step 5: Persist phase index.
        {
            let storage_r = self.storage.read().map_err(|e| RecalldError::Shutdown {
                message: format!("storage lock poisoned: {}", e),
                source: None,
            })?;
            if let Err(e) = storage_r.persist_phase_index() {
                tracing::error!(
                    %e,
                    "failed to persist phase index during shutdown"
                );
            }
        }

        // Step 6: Drop subsystems in reverse init order.
        // Arc-wrapped subsystems are dropped when Self is dropped.
        // Explicit ordering is handled by Rust's struct drop order
        // (declaration order). All Arc clones held by background
        // tasks have already been released (tasks joined above).

        let elapsed = shutdown_start.elapsed();
        tracing::info!(elapsed_ms = elapsed.as_millis(), "shutdown complete");

        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchFilter;
    use crate::test_support::{DIM, Fixture, MemorySpec};

    fn ns() -> NamespaceId {
        NamespaceId::new(1)
    }

    /// Run the startup load against a fixture's storage, into a fresh
    /// index of `dim` — which is what step 6 does, minus the logging.
    fn load(fx: &Fixture, dim: usize) -> (FlatVectorIndex, VectorLoadStats) {
        let storage_r = fx.storage.read().expect("storage lock");
        let records = storage_r.scan_all().expect("scan_all");
        let mut index = FlatVectorIndex::new(dim);
        let stats = load_vector_index(&storage_r, &records, &mut index);
        (index, stats)
    }

    /// A config pointed at `dir` that will not talk to the network or
    /// start a sweep.
    fn offline_config(dir: &std::path::Path) -> RecalldConfig {
        let mut config = RecalldConfig::default();
        config.storage.data_dir = dir.to_str().expect("utf-8 temp dir").to_string();
        config.embedding.provider = crate::config::EmbeddingProvider::Passthrough;
        config.embedding.dimensions = DIM;
        config.decay.disable_sweep = true;
        config
    }

    #[tokio::test]
    async fn effects_block_cached_queries_and_reopen_rebuilds_deleted_fts_rows() {
        let fx = Fixture::new();
        let live = fx.insert_memory().await;
        let dead = fx.insert_memory().await;
        let dir = fx.close();
        let system = Recalld::new(offline_config(dir.path())).await.unwrap();
        let record = system
            .storage
            .read()
            .unwrap()
            .get_record(dead)
            .unwrap()
            .unwrap();
        let ns = crate::model::NamespaceId::new(record.namespace_id);
        system
            .cache
            .insert(dead, crate::model::CachedRecord::from(&record))
            .await;
        system.storage.write().unwrap().delete_memory(dead).unwrap();
        assert!(
            system.query_engine.get_memory(dead).await.is_err(),
            "cached deleted record must not bypass admission"
        );
        assert!(system.query_engine.similar(dead, 5).await.is_err());
        let query =
            serde_json::from_value(serde_json::json!({"text":"fixture", "namespace":"default"}))
                .unwrap();
        assert!(system.query_engine.search(query).await.is_err());
        assert!(
            system
                .storage
                .read()
                .unwrap()
                .meta_store()
                .needs_effects_rebuild()
        );
        {
            use crate::api::adapters::*;
            let state = crate::api::AppState::new(
                Arc::new(SearchPipelineAdapter::new(
                    system.query_engine.clone(),
                    system.embedding.clone(),
                    system.vector_index.clone(),
                    system.fts_index.clone(),
                    system.entity_index.clone(),
                )),
                Arc::new(StorageEngineAdapter::new(
                    system.storage.clone(),
                    system.cache.clone(),
                    system.vector_index.clone(),
                    system.fts_index.clone(),
                    system.entity_index.clone(),
                    system.graph.clone(),
                )),
                Arc::new(RecordCacheAdapter::new(system.cache.clone())),
                Arc::new(RelationshipGraphAdapter::new(
                    system.graph.clone(),
                    system.vector_index.clone(),
                    system.entity_index.clone(),
                    system.storage.clone(),
                    system.cache.clone(),
                    system.config.clone(),
                )),
                Arc::new(FsrsEngineAdapter::new(
                    system.storage.clone(),
                    system.cache.clone(),
                    system.graph.clone(),
                    system.config.clone(),
                    false,
                )),
                Arc::new(NamespaceRegistryAdapter::new(system.storage.clone())),
                Arc::new(NoopMetricsCollector),
            );
            use tower::ServiceExt;
            let response = crate::api::router(state, &crate::api::ApiConfig::default())
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/memories/{dead}"))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "HTTP cache must not return deleted content or a false 404"
            );
        }
        drop(system);
        let system = Recalld::new(offline_config(dir.path())).await.unwrap();
        assert!(
            !system
                .storage
                .read()
                .unwrap()
                .meta_store()
                .needs_effects_rebuild()
        );
        assert!(
            system
                .query_engine
                .get_memory(dead)
                .await
                .unwrap()
                .is_none()
        );
        assert!(system.vector_index.read().await.get_vector(dead).is_none());
        assert!(!system.graph.read().await.contains(&dead));
        let hits = system
            .fts_index
            .lock()
            .await
            .search(ns, "fixture", 10)
            .unwrap();
        assert!(hits.iter().all(|hit| hit.0 != dead));
        assert!(hits.iter().any(|hit| hit.0 == live));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_mcp_cached_consumers_block_then_recover_online() {
        use crate::mcp::bridge::{SearchPipeline, StorageEngine as McpStorage};
        use crate::mcp::bridge_adapters::{McpSearchAdapter, McpStorageAdapter};
        let fx = Fixture::new();
        let live = fx.insert_memory().await;
        let dead = fx.insert_memory().await;
        let dir = fx.close();
        let system = Recalld::new(offline_config(dir.path())).await.unwrap();
        let search = McpSearchAdapter::new(
            system.query_engine.clone(),
            system.embedding.clone(),
            system.storage.clone(),
            system.graph.clone(),
            chrono_tz::UTC,
        );
        let adapter = McpStorageAdapter::new(
            system.storage.clone(),
            system.cache.clone(),
            system.embedding.clone(),
            system.vector_index.clone(),
            system.fts_index.clone(),
            system.entity_index.clone(),
            system.graph.clone(),
            system.config.clone(),
            chrono_tz::UTC,
        );
        let stale = crate::model::CachedRecord::from(
            &system
                .storage
                .read()
                .unwrap()
                .get_record(dead)
                .unwrap()
                .unwrap(),
        );
        system.cache.insert(dead, stale.clone()).await;
        system.storage.write().unwrap().delete_memory(dead).unwrap();
        assert!(search.find_similar(live, 5, None, true).await.is_err());
        assert!(adapter.get_memory(dead).await.is_err());
        assert!(adapter.get_memory(live).await.is_err());
        system.complete_pending_effects().await.unwrap();
        system.cache.insert(dead, stale).await;
        assert!(
            system
                .query_engine
                .get_memory(dead)
                .await
                .unwrap()
                .is_none()
        );
        assert!(adapter.get_memory(dead).await.unwrap().is_none());
        assert!(adapter.get_memory(live).await.unwrap().is_some());
        assert!(search.find_similar(live, 5, None, true).await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_completion_crash_reopen_preserves_receipt_and_deleted_content() {
        const CHILD: &str = "RECALLD_ONLINE_COMPLETION_CHILD";
        if let Ok(path) = std::env::var(CHILD) {
            let system = Recalld::new(offline_config(std::path::Path::new(&path)))
                .await
                .unwrap();
            let dead = system.storage.read().unwrap().scan_all().unwrap()[0].0;
            system.storage.write().unwrap().delete_memory(dead).unwrap();
            system.complete_pending_effects().await.unwrap();
            panic!("child did not exit at crash point");
        }
        let fx = Fixture::new();
        let dead = fx.insert_memory().await;
        let dir = fx.close();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("system::tests::online_completion_crash_reopen_preserves_receipt_and_deleted_content")
            .arg("--exact").arg("--nocapture")
            .env(CHILD, dir.path()).env("RECALLD_TEST_EFFECT_CRASH", "before_online_derived_ack")
            .output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(91),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt = {
            let storage = RedbStorageEngine::open(dir.path()).unwrap();
            let pending = storage.meta_store().pending_effects().unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].canonical_applied);
            pending[0].receipt.clone()
        };
        let system = Recalld::new(offline_config(dir.path())).await.unwrap();
        assert!(
            system
                .storage
                .read()
                .unwrap()
                .meta_store()
                .pending_effects()
                .unwrap()
                .is_empty()
        );
        assert!(
            system
                .query_engine
                .get_memory(dead)
                .await
                .unwrap()
                .is_none()
        );
        assert!(system.fts_index.lock().await.is_empty().unwrap());
        assert!(system.vector_index.read().await.get_vector(dead).is_none());
        system.complete_pending_effects().await.unwrap();
        assert_eq!(
            system
                .storage
                .read()
                .unwrap()
                .meta_store()
                .control_state()
                .unwrap()
                .committed_generation,
            receipt.committed_generation
        );
    }

    // ── the regression ───────────────────────────────────────────────

    /// THE bug. `FlatVectorIndex` is RAM-only and was only ever filled
    /// by the store path, so every restart came up with an empty index:
    /// `find_similar_memories` failed with "no embedding found for
    /// memory <id>" and `recall_memories` quietly fell back to FTS and
    /// graph matching until something was stored in that process.
    ///
    /// This drives the real `Recalld::new`, not the helper, because the
    /// helper is not what was broken — the wiring was. Delete the load
    /// from step 6 and this fails with `0 == 3`.
    #[tokio::test]
    async fn a_restart_reloads_the_vector_index_from_disk() {
        let dir = {
            let fx = Fixture::new();
            for _ in 0..3 {
                fx.insert_memory().await;
            }
            fx.close()
        };

        let system = Recalld::new(offline_config(dir.path()))
            .await
            .expect("system starts over the existing store");

        assert_eq!(
            system.vector_index().read().await.len(),
            3,
            "the three memories on disk should be searchable by vector \
             immediately, before this process stores anything"
        );
    }

    /// The same property one layer down, where the counts are visible.
    #[tokio::test]
    async fn the_load_reads_vectors_that_outlived_the_engine_that_wrote_them() {
        let dir = {
            let fx = Fixture::new();
            fx.insert_memory().await;
            fx.insert_memory().await;
            fx.close()
        };

        let engine = RedbStorageEngine::open(dir.path()).expect("reopen engine");
        let records = engine.scan_all().expect("scan_all");
        let mut index = FlatVectorIndex::new(DIM);
        let stats = load_vector_index(&engine, &records, &mut index);

        assert_eq!(stats.loaded, 2);
        assert_eq!(stats.skipped(), 0);
        assert_eq!(index.len(), 2);
    }

    // ── what gets skipped ────────────────────────────────────────────

    /// A tombstone has had its content stripped and is deliberately
    /// absent from every search index. Loading its vector would
    /// resurrect it as a similarity hit.
    #[tokio::test]
    async fn a_tombstoned_memory_is_not_loaded() {
        let fx = Fixture::new();
        let live = fx.insert_memory().await;
        let dead = fx.insert_memory().await;
        fx.tombstone(dead);

        let (index, stats) = load(&fx, DIM);

        assert_eq!(stats.loaded, 1);
        assert_eq!(stats.tombstoned, 1);
        assert!(index.get_vector(live).is_some());
        assert!(
            index.get_vector(dead).is_none(),
            "a tombstone must not come back as a vector hit"
        );
    }

    /// A record pointing past the end of its namespace's `vectors.dat` —
    /// what a truncated or replaced vector file leaves behind. The
    /// memory is simply not vector-searchable, which is what it was
    /// before this load existed; startup carries on.
    #[tokio::test]
    async fn a_record_whose_vector_is_gone_is_skipped_and_startup_continues() {
        let fx = Fixture::new();
        let good = fx.insert_memory().await;
        fx.insert_memory().await;

        let storage_r = fx.storage.read().expect("storage lock");
        let mut records = storage_r.scan_all().expect("scan_all");
        // Point one record at a slot that was never allocated.
        let orphan = records
            .iter_mut()
            .find(|(id, _)| *id != good)
            .expect("two records");
        orphan.1.vector_slot = 9_999;

        let mut index = FlatVectorIndex::new(DIM);
        let stats = load_vector_index(&storage_r, &records, &mut index);

        assert_eq!(stats.missing, 1);
        assert_eq!(stats.loaded, 1);
        assert!(
            index.get_vector(good).is_some(),
            "one bad record must not cost the others their vectors"
        );
    }

    /// A record in a namespace that no longer has a vector store at all.
    #[tokio::test]
    async fn a_record_in_an_unreadable_namespace_is_skipped() {
        let fx = Fixture::new();
        fx.insert_memory().await;

        let storage_r = fx.storage.read().expect("storage lock");
        let mut records = storage_r.scan_all().expect("scan_all");
        records[0].1.namespace_id = 4_242;

        let mut index = FlatVectorIndex::new(DIM);
        let stats = load_vector_index(&storage_r, &records, &mut index);

        assert_eq!(stats.unreadable, 1);
        assert_eq!(stats.loaded, 0);
        assert!(stats.unreadable_namespaces.contains(&4_242));
        assert_eq!(index.len(), 0);
    }

    // ── the real decay phase ─────────────────────────────────────────

    /// The store path hardcodes `Full`, so every index-level phase
    /// filter was inert. Loading the record's true phase is what makes
    /// the filter mean something.
    #[tokio::test]
    async fn the_loaded_entry_carries_the_records_real_phase() {
        let fx = Fixture::new();
        let ghost = fx
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Ghost,
                ..MemorySpec::new(ns())
            })
            .await;
        let summary = fx
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Summary,
                ..MemorySpec::new(ns())
            })
            .await;
        let full = fx.insert_memory().await;

        let (index, stats) = load(&fx, DIM);
        assert_eq!(stats.loaded, 3);

        // Asserted through the filter, because the filter is the point.
        let only = |phase: DecayPhase| {
            let filter = SearchFilter {
                decay_phases: Some(vec![phase.as_u8()]),
                ..SearchFilter::default()
            };
            let hits = index
                .search(&[1.0, 0.0, 0.0, 0.0], 10, &filter)
                .expect("search");
            hits.into_iter().map(|h| h.id).collect::<Vec<_>>()
        };

        assert_eq!(only(DecayPhase::Ghost), vec![ghost]);
        assert_eq!(only(DecayPhase::Summary), vec![summary]);
        assert_eq!(only(DecayPhase::Full), vec![full]);
    }

    // ── per-namespace widths ─────────────────────────────────────────

    /// The index has one width; namespaces each have their own. A stored
    /// vector of a different width cannot go in, and that is a warning,
    /// not a failed boot.
    #[tokio::test]
    async fn vectors_of_the_wrong_width_are_counted_not_fatal() {
        let fx = Fixture::new();
        let narrow = fx.insert_memory().await;

        let wide_ns = fx.with_namespace("wide", 8);
        fx.insert_memory_with(MemorySpec {
            embedding: Some(&[0.0; 8]),
            ..MemorySpec::new(wide_ns)
        })
        .await;

        let (index, stats) = load(&fx, DIM);

        assert_eq!(stats.loaded, 1, "the matching-width memory still loads");
        assert_eq!(stats.dim_mismatch, 1);
        assert_eq!(
            stats.mismatched_namespaces,
            [wide_ns.get()].into_iter().collect(),
            "the warning names the namespace an operator has to go look at"
        );
        assert!(index.get_vector(narrow).is_some());
    }

    /// The other route to the same state, and the likelier one: the
    /// operator changed `embedding.dimensions` between runs. Every
    /// stored vector is now the wrong width.
    #[tokio::test]
    async fn a_changed_embedding_width_skips_everything_without_failing() {
        let fx = Fixture::new();
        fx.insert_memory().await;
        fx.insert_memory().await;

        let (index, stats) = load(&fx, DIM * 2);

        assert_eq!(stats.loaded, 0);
        assert_eq!(stats.dim_mismatch, 2);
        assert_eq!(index.len(), 0);
    }

    // ── end to end ───────────────────────────────────────────────────

    /// The property the bug report was actually about: after a restart,
    /// asking for a memory's neighbours works. `find_similar` fails at
    /// `vector_indexes.get_vector(id)` — the same lookup this asserts —
    /// so an index that has the vector is the whole of the fix.
    #[tokio::test]
    async fn a_stored_memory_still_has_its_embedding_after_a_restart() {
        let (dir, id) = {
            let fx = Fixture::new();
            let id = fx
                .insert_memory_with(MemorySpec {
                    embedding: Some(&[1.0, 0.0, 0.0, 0.0]),
                    ..MemorySpec::new(ns())
                })
                .await;
            (fx.close(), id)
        };

        let system = Recalld::new(offline_config(dir.path()))
            .await
            .expect("system starts over the existing store");

        let index = system.vector_index().read().await;
        assert_eq!(
            index.get_vector(id),
            Some(vec![1.0, 0.0, 0.0, 0.0]),
            "find_similar_memories resolves the source embedding through \
             exactly this lookup; None is the 'no embedding found for \
             memory <id>' error the user saw"
        );
        let hits = index
            .search(&[1.0, 0.0, 0.0, 0.0], 5, &SearchFilter::default())
            .expect("search");
        assert_eq!(hits.first().map(|h| h.id), Some(id));
    }
}
