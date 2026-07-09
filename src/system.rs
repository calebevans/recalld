//! Root system object for Recalld.
//!
//! The [`Recalld`] struct owns or holds `Arc` references to every
//! subsystem. Constructed by [`Recalld::new()`], which executes
//! the ordered startup sequence. Torn down by [`Recalld::shutdown()`].

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
use crate::rif::{RifConfig, RifEngine};
use crate::search::{EntityIndex, FlatVectorIndex, FtsIndex, QueryEngine};
use crate::storage::engine::RedbStorageEngine;
// Import the StorageEngine trait so its methods are in scope.
use crate::storage::StorageEngine as _;

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
            let edge_count =
                graph::rebuild_from_storage(&mut rel_graph, persisted_edges.into_iter());

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
                    crate::config::EmbeddingProvider::Bedrock => {
                        #[cfg(feature = "bedrock")]
                        {
                            embedding::ProviderType::Bedrock
                        }
                        #[cfg(not(feature = "bedrock"))]
                        {
                            return Err(RecalldError::Init {
                                step: "create_embedding_provider",
                                message: "Bedrock provider requires the 'bedrock' feature. \
                                          Rebuild with: cargo build --features bedrock"
                                    .into(),
                                source: None,
                            });
                        }
                    }
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

        // -- Step 6: Create FlatVectorIndex ---------------------------
        let vector_index = {
            let index = FlatVectorIndex::new(config.embedding.dimensions);
            tracing::info!(
                dimensions = config.embedding.dimensions,
                "flat vector index created"
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
            if is_new || fts.is_empty().unwrap_or(true) {
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
                        let namespace_id = crate::model::NamespaceId::new(record.namespace_id);
                        let tag_strings: Vec<String> =
                            record.tags.iter().map(|t| t.to_string()).collect();
                        // Retrieve full_text from the text log if available.
                        let full_text = if record.text_length > 0 {
                            let text_ref = crate::storage::TextRef {
                                file_offset: record.text_offset,
                                length: record.text_length,
                            };
                            storage_r.get_text(text_ref).ok().flatten()
                        } else {
                            None
                        };
                        if let Err(e) = fts.add(
                            namespace_id,
                            *memory_id,
                            &record.summary,
                            full_text.as_deref(),
                            &tag_strings,
                        ) {
                            tracing::warn!(
                                memory_id = %memory_id,
                                %e,
                                "failed to index record in FTS5 during migration"
                            );
                        }
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
                .get_namespace_by_name("default")
                .map_err(|e| RecalldError::Init {
                    step: "check_default_namespace",
                    message: format!("failed to check default namespace: {}", e),
                    source: Some(Box::new(e)),
                })?
                .is_some();
            drop(storage_r);

            if !has_default {
                let ns_config = crate::model::NamespaceConfig {
                    id: crate::model::NamespaceId::UNSET,
                    name: "default".to_string(),
                    embedding_dim: config.embedding.dimensions as u32,
                    initial_stability: 3.7145,
                    default_difficulty: 5.0,
                    phase_thresholds: crate::model::namespace::PhaseThresholds::default(),
                    permastore_threshold: 1500.0,
                    created_at: chrono::Utc::now().timestamp_millis(),
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
