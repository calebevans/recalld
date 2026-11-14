//! Decay sweep runner -- background task that iterates all memories,
//! computes effective retrievability, executes phase transitions, and
//! deletes expired memories.
//!
//! The sweep is the enforcement arm of the FSRS decay engine. The math
//! lives in [`super::fsrs`]; this module applies it at scale.
//!
//! # Phase Ordering
//!
//! Phases are processed in reverse order (Ghost -> Summary -> Full) so
//! that deletions happen before transitions INTO the deleted phase,
//! preventing a memory from being ghosted and immediately deleted in
//! the same sweep.
//!
//! # Concurrency
//!
//! The sweep runs in a background tokio task with cooperative yielding.
//! It does NOT hold a global lock -- individual record operations are
//! serialized by the storage write lock. Planned snapshots are checked again
//! under that lock before applying any mutation. Feedback and phase/content loss
//! use one metadata transaction; hard deletion/edge effects remain a separate boundary.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};
use tokio::time;
use tracing::{debug, error, info, instrument, warn};

use super::config::DecayConfig;
use super::fsrs::FsrsEngine;
use crate::cache::CacheManager;
use crate::graph::SharedGraph;
use crate::graph::activation::{ActivationConfig, connection_bonus};
use crate::model::{DecayPhase, MemoryId};
use crate::storage::{RedbStorageEngine, StorageEngine as _};

// ── Decay Metadata ──────────────────────────────────────────────────

/// Subset of memory metadata needed by the sweep runner.
///
/// Avoids loading the full memory record (which includes text, tags,
/// embedding) when the sweep only needs decay-related fields.
#[derive(Debug, Clone)]
pub struct DecayMetadata {
    pub phase: DecayPhase,
    pub created_at: i64,
    /// FSRS stability S in days.
    pub stability: f32,
    /// Milliseconds since Unix epoch.
    pub last_accessed_at: i64,
    /// Whether the memory is exempt from decay sweeps.
    pub is_permastore: bool,
    /// Namespace policy snapshot; concrete namespace values override global defaults.
    pub namespace: crate::model::NamespaceConfig,
    /// Whether the record has a non-empty summary.
    /// Used by Full -> Summary transitions to skip the summary check
    /// without re-reading the record from storage.
    pub has_summary: bool,
}

impl DecayMetadata {
    /// Compare lifecycle inputs, not materialized strengths written by this sweep.
    fn matches(&self, record: &crate::model::DiskRecord) -> bool {
        record.phase == self.phase
            && record.created_at == self.created_at
            && record.last_accessed_at == self.last_accessed_at
            && record.stability.to_bits() == self.stability.to_bits()
            && (record.is_permastore != 0) == self.is_permastore
            && !record.summary.is_empty() == self.has_summary
    }
}

// ── SweepConfig ─────────────────────────────────────────────────────

/// Configuration for the decay sweep runner.
#[derive(Debug, Clone)]
pub struct SweepConfig {
    /// Interval between sweep runs. Default: 6 hours.
    /// Valid range: 1 minute to 24 hours.
    pub interval: Duration,

    /// Whether to run a sweep immediately on startup.
    /// Default: true.
    pub sweep_on_startup: bool,

    /// Number of records to process before yielding to the
    /// tokio runtime via `tokio::task::yield_now()`.
    /// Prevents starving query tasks during large sweeps.
    /// Default: 1000.
    pub yield_every_n: usize,

    /// Batch size for storage writes. Phase transitions and
    /// metadata updates are collected into batches of this size
    /// before flushing to storage. Default: 256.
    pub write_batch_size: usize,

    /// Dead-space ratio in fulltext.dat that triggers compaction.
    /// When the fraction of dead (reclaimed) space exceeds this
    /// threshold, fulltext.dat is compacted at the end of the sweep.
    /// Default: 0.30 (30%).
    pub text_compaction_threshold: f64,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(6 * 3600), // 6 hours
            sweep_on_startup: true,
            yield_every_n: 1000,
            write_batch_size: 256,
            text_compaction_threshold: 0.30,
        }
    }
}

impl SweepConfig {
    /// Validates configuration. Returns `Err` if values are out of range.
    pub fn validate(&self) -> Result<(), SweepConfigError> {
        if self.interval < Duration::from_secs(60) {
            return Err(SweepConfigError::IntervalTooShort(self.interval));
        }
        if self.interval > Duration::from_secs(24 * 3600) {
            return Err(SweepConfigError::IntervalTooLong(self.interval));
        }
        if self.yield_every_n == 0 {
            return Err(SweepConfigError::YieldEveryZero);
        }
        if self.write_batch_size == 0 {
            return Err(SweepConfigError::BatchSizeZero);
        }
        if !(0.0..=1.0).contains(&self.text_compaction_threshold) {
            return Err(SweepConfigError::InvalidCompactionThreshold(
                self.text_compaction_threshold,
            ));
        }
        Ok(())
    }
}

/// Errors from [`SweepConfig::validate`].
#[derive(Debug, thiserror::Error)]
pub enum SweepConfigError {
    /// Sweep interval is below the minimum of 60 seconds.
    #[error("sweep interval {0:?} is below minimum of 60 seconds")]
    IntervalTooShort(Duration),
    /// Sweep interval exceeds the maximum of 24 hours.
    #[error("sweep interval {0:?} exceeds maximum of 24 hours")]
    IntervalTooLong(Duration),
    /// yield_every_n must be greater than zero.
    #[error("yield_every_n must be > 0")]
    YieldEveryZero,
    /// write_batch_size must be greater than zero.
    #[error("write_batch_size must be > 0")]
    BatchSizeZero,
    /// text_compaction_threshold must be in [0.0, 1.0].
    #[error("text_compaction_threshold {0} must be in [0.0, 1.0]")]
    InvalidCompactionThreshold(f64),
}

// ── SweepResult ─────────────────────────────────────────────────────

/// Statistics from a single sweep run.
///
/// Returned by `run_sweep()` and logged at INFO level.
#[derive(Debug, Clone, Default)]
pub struct SweepResult {
    /// Total memories scanned across all phases.
    pub memories_scanned: u64,
    /// Memories skipped because they are permastore.
    pub permastore_skipped: u64,
    /// Memories skipped because decay is disabled (multiplier == 0.0).
    pub decay_disabled_skipped: u64,
    /// Phase 1 (Full) -> Phase 2 (Summary) transitions.
    pub full_to_summary: u64,
    /// Phase 2 (Summary) -> Phase 3 (Ghost) transitions.
    pub summary_to_ghost: u64,
    /// Phase 3 (Ghost) -> Deleted (R <= 0.05).
    pub deletions: u64,
    /// Memories where connection bonus prevented a phase transition
    /// that raw R alone would have triggered.
    pub saved_by_connection_bonus: u64,
    /// Whether text compaction succeeded, including an empty generation selection.
    pub compaction_triggered: bool,
    /// Wall-clock duration of the entire sweep.
    pub duration: Duration,
    /// All phase transitions emitted during this sweep.
    pub transitions: Vec<PhaseTransition>,
    /// Errors encountered during the sweep. The sweep continues
    /// past individual record failures; errors are collected here.
    pub errors: Vec<SweepRecordError>,
}

/// An error encountered while processing a single record during sweep.
///
/// Non-fatal -- the sweep skips the record and continues.
#[derive(Debug, Clone)]
pub struct SweepRecordError {
    /// The memory that failed to process.
    pub memory_id: MemoryId,
    /// The phase the memory was in when the error occurred.
    pub phase: DecayPhase,
    /// Human-readable error description.
    pub error: String,
}

/// A phase transition event emitted by the sweep.
///
/// Collected in [`SweepResult::transitions`] for metrics and event logging.
#[derive(Debug, Clone)]
pub enum PhaseTransition {
    /// Full -> Summary: full_text deleted, summary retained.
    Summarized {
        /// The memory that transitioned.
        id: MemoryId,
        /// The phase before the transition.
        old_phase: DecayPhase,
        /// The phase after the transition.
        new_phase: DecayPhase,
        /// Effective retrievability at the time of transition.
        retrievability: f32,
    },
    /// Summary -> Ghost: summary deleted, embedding + edges retained.
    Ghosted {
        /// The memory that transitioned.
        id: MemoryId,
        /// The phase before the transition.
        old_phase: DecayPhase,
        /// The phase after the transition.
        new_phase: DecayPhase,
        /// Effective retrievability at the time of transition.
        retrievability: f32,
    },
    /// Ghost -> Deleted: complete removal.
    Deleted {
        /// The memory that was deleted.
        id: MemoryId,
        /// The phase before deletion (always Ghost).
        old_phase: DecayPhase,
        /// Effective retrievability at the time of deletion.
        retrievability: f32,
    },
}

// ── SweepError ──────────────────────────────────────────────────────

/// Errors that can occur during a decay sweep.
#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    /// An I/O or storage-level error.
    #[error("storage error: {0}")]
    Storage(String),

    /// A graph operation error.
    #[error("graph error: {0}")]
    Graph(String),

    /// A cache operation error.
    #[error("cache error: {0}")]
    Cache(String),

    /// Memory not found during a transition.
    #[error("memory {0} not found during transition")]
    MemoryNotFound(MemoryId),

    /// Phase index is unavailable for a given phase.
    #[error("phase index unavailable for phase {0:?}")]
    PhaseIndexUnavailable(DecayPhase),
}

// ── PendingTransition ───────────────────────────────────────────────

/// A pending phase transition collected during the scan, to be applied
/// in batch to minimize storage round-trips.
///
/// Carries the decay metadata snapshot from scan time so that transition
/// methods can reject stale plans under the final storage write lock.
#[derive(Debug, Clone)]
struct PendingTransition {
    memory_id: MemoryId,
    from_phase: DecayPhase,
    evaluation: SweepEvaluation,
    expected: DecayMetadata,
}

/// Frozen clock/policy inputs travel with every evaluated record, including
/// strength-only updates. Graph-derived strength is recomputed at publication.
#[derive(Debug, Clone)]
struct SweepEvaluation {
    projection: super::lifecycle::LifecycleProjection,
    policy: super::lifecycle::EffectiveLifecyclePolicy,
    activation: ActivationConfig,
}

impl SweepEvaluation {
    fn threshold(&self, phase: DecayPhase) -> Option<f32> {
        match phase {
            DecayPhase::Full => Some(self.policy.config.phase_1_threshold),
            DecayPhase::Summary => Some(self.policy.config.phase_2_threshold),
            DecayPhase::Ghost => Some(self.policy.config.phase_3_threshold),
            DecayPhase::Tombstone => None,
        }
    }

    /// Caller holds graph -> storage through the following canonical write.
    /// No clock sampling or graph lock acquisition happens inside this method.
    fn project_locked(
        &self,
        id: MemoryId,
        expected: &DecayMetadata,
        storage: &RedbStorageEngine,
        graph: &crate::graph::RelationshipGraph,
    ) -> Result<Option<super::lifecycle::LifecycleProjection>, SweepError> {
        let namespace = storage
            .get_namespace(expected.namespace.id)
            .map_err(|e| SweepError::Storage(e.to_string()))?;
        if namespace
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| SweepError::Storage(e.to_string()))?
            != Some(
                serde_json::to_vec(&expected.namespace)
                    .map_err(|e| SweepError::Storage(e.to_string()))?,
            )
            || expected.is_permastore
            || self.policy.decay_rate_multiplier == 0.0
        {
            return Ok(None);
        }
        let bonus = connection_bonus(id, graph, &self.activation).clamp(0.0, MAX_CONNECTION_BONUS);
        super::lifecycle::project(
            expected.stability,
            expected.last_accessed_at,
            &self.policy,
            self.projection.evaluated_at,
            bonus,
        )
        .map(Some)
        .map_err(|e| SweepError::Storage(e.to_string()))
    }
}

// ── Maximum connection bonus constant ───────────────────────────────

/// Maximum connection bonus. Caps the bonus at 15 percentage points
/// to prevent highly-connected memories from becoming immune to decay.
const MAX_CONNECTION_BONUS: f32 = 0.15;

// ── DecaySweepRunner ────────────────────────────────────────────────

/// The decay sweep runner. Owns the background task that periodically
/// scans all memories and applies phase transitions.
///
/// # Lifecycle
///
/// 1. Construct via `DecaySweepRunner::new()`
/// 2. Call `start()` to spawn the background tokio task
/// 3. The task runs indefinitely until `shutdown()` is called
/// 4. `trigger()` forces an immediate sweep outside the regular interval
///
/// The runner holds `Arc` references to shared subsystems. It does NOT
/// own those subsystems -- it borrows them via Arc for the background task.
pub struct DecaySweepRunner {
    completion: Option<crate::derived_completion::DerivedCompletion>,
    clock: Arc<dyn crate::time::Clock>,
    config: SweepConfig,
    decay_config: Arc<DecayConfig>,
    activation_config: ActivationConfig,
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    graph: SharedGraph,
    cache: Arc<CacheManager>,

    /// Global decay rate multiplier from the top-level config.
    /// Namespace-specific multipliers override this.
    global_decay_multiplier: f64,

    /// Sends shutdown signal to the background task.
    shutdown_tx: watch::Sender<bool>,

    /// Notifies the background task to run an immediate sweep.
    trigger_notify: Arc<Notify>,
}

impl DecaySweepRunner {
    /// Create a new sweep runner. Does NOT start the background task --
    /// call `start()` separately.
    ///
    /// `global_decay_multiplier` is the top-level decay rate multiplier
    /// from the application config. Namespace-specific multipliers
    /// override this value.
    pub fn new(
        config: SweepConfig,
        decay_config: Arc<DecayConfig>,
        activation_config: ActivationConfig,
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: SharedGraph,
        cache: Arc<CacheManager>,
        global_decay_multiplier: f64,
    ) -> Result<Self, SweepConfigError> {
        config.validate()?;
        let (shutdown_tx, _) = watch::channel(false);
        Ok(Self {
            clock: Arc::new(crate::time::SystemClock),
            config,
            decay_config,
            activation_config,
            storage,
            graph,
            cache,
            global_decay_multiplier,
            shutdown_tx,
            trigger_notify: Arc::new(Notify::new()),
            completion: None,
        })
    }

    pub(crate) fn with_completion(
        mut self,
        completion: crate::derived_completion::DerivedCompletion,
    ) -> Self {
        self.completion = Some(completion);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn crate::time::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Spawn the background sweep task on the tokio runtime.
    ///
    /// Returns a `JoinHandle` that resolves when the task exits
    /// (after shutdown is signaled).
    ///
    /// The task:
    /// 1. Optionally runs an immediate sweep (if `sweep_on_startup`)
    /// 2. Enters a loop: sleep for `interval`, then sweep
    /// 3. Exits when `shutdown()` is called
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let completion = self.completion.clone();
        let clock = self.clock.clone();
        let config = self.config.clone();
        let decay_config = Arc::clone(&self.decay_config);
        let activation_config = self.activation_config.clone();
        let storage = Arc::clone(&self.storage);
        let graph = Arc::clone(&self.graph);
        let cache = Arc::clone(&self.cache);
        let global_decay_multiplier = self.global_decay_multiplier;
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let trigger_notify = Arc::clone(&self.trigger_notify);

        tokio::spawn(async move {
            // Optional startup sweep
            if config.sweep_on_startup {
                info!("running startup decay sweep");
                let result = Self::execute_sweep_at(
                    &config,
                    &decay_config,
                    &activation_config,
                    &storage,
                    &graph,
                    &cache,
                    global_decay_multiplier,
                    Some(clock.now_millis()),
                )
                .await;
                if let Some(completion) = &completion {
                    if let Err(error) = completion.complete().await {
                        tracing::error!(%error, "sweep effects remain pending; next sweep retries completion");
                    }
                }
                Self::log_result(&result);
            }

            let mut interval = time::interval(config.interval);
            // The first tick fires immediately -- consume it since we
            // may have already done the startup sweep.
            interval.tick().await;

            loop {
                tokio::select! {
                    // Regular interval tick
                    _ = interval.tick() => {
                        info!("starting scheduled decay sweep");
                    }
                    // Manual trigger
                    _ = trigger_notify.notified() => {
                        info!("starting manually triggered decay sweep");
                        // Reset the interval so the next scheduled sweep
                        // is a full interval from now.
                        interval.reset();
                    }
                    // Shutdown signal
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            info!("decay sweep runner shutting down");
                            return;
                        }
                        continue;
                    }
                }

                // Check shutdown before starting a potentially long sweep
                if *shutdown_rx.borrow() {
                    return;
                }

                let result = Self::execute_sweep_at(
                    &config,
                    &decay_config,
                    &activation_config,
                    &storage,
                    &graph,
                    &cache,
                    global_decay_multiplier,
                    Some(clock.now_millis()),
                )
                .await;
                if let Some(completion) = &completion {
                    if let Err(error) = completion.complete().await {
                        tracing::error!(%error, "sweep effects remain pending; next sweep retries completion");
                    }
                }
                Self::log_result(&result);
            }
        })
    }

    /// Signal the background task to shut down gracefully.
    ///
    /// The current sweep (if running) will complete before the task exits.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Trigger an immediate sweep outside the regular interval.
    ///
    /// Non-blocking -- the sweep runs asynchronously in the background task.
    /// If a sweep is already in progress, the trigger is consumed and a
    /// new sweep starts immediately after the current one finishes.
    pub fn trigger(&self) {
        self.trigger_notify.notify_one();
    }

    /// Log the result of a sweep run at INFO level.
    fn log_result(result: &SweepResult) {
        info!(
            scanned = result.memories_scanned,
            permastore_skipped = result.permastore_skipped,
            decay_disabled_skipped = result.decay_disabled_skipped,
            full_to_summary = result.full_to_summary,
            summary_to_ghost = result.summary_to_ghost,
            deletions = result.deletions,
            saved_by_bonus = result.saved_by_connection_bonus,
            compaction = result.compaction_triggered,
            errors = result.errors.len(),
            duration_ms = result.duration.as_millis() as u64,
            "decay sweep complete"
        );
        for err in &result.errors {
            warn!(
                memory_id = %err.memory_id,
                phase = ?err.phase,
                error = %err.error,
                "sweep record error"
            );
        }
    }
}

// ── Sweep Algorithm ─────────────────────────────────────────────────

impl DecaySweepRunner {
    /// Execute a single sweep. This is the complete algorithm.
    ///
    /// # Phase ordering
    ///
    /// Phases are processed in REVERSE order (3 -> 2 -> 1) so that:
    /// 1. Deletions happen before transitions INTO the phase being deleted,
    ///    preventing a memory from being ghosted and immediately deleted
    ///    in the same sweep.
    /// 2. The cheapest operations (phase 3 records are smallest) run first.
    #[instrument(skip_all)]
    pub(crate) async fn execute_sweep_at(
        config: &SweepConfig,
        decay_config: &DecayConfig,
        activation_config: &ActivationConfig,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: &SharedGraph,
        cache: &Arc<CacheManager>,
        global_decay_multiplier: f64,
        as_of_millis: Option<i64>,
    ) -> SweepResult {
        let start = Instant::now();
        let now_millis = as_of_millis
            .unwrap_or_else(|| crate::time::Clock::now_millis(&crate::time::SystemClock));
        let engine = FsrsEngine::new(decay_config);
        let mut result = SweepResult::default();

        // -- Phase 3: Ghost -> Deletable --------------------------
        Self::sweep_phase(
            DecayPhase::Ghost,
            config,
            &engine,
            activation_config,
            storage,
            graph,
            cache,
            now_millis,
            global_decay_multiplier,
            &mut result,
        )
        .await;

        // -- Phase 2: Summary -> Ghost ----------------------------
        Self::sweep_phase(
            DecayPhase::Summary,
            config,
            &engine,
            activation_config,
            storage,
            graph,
            cache,
            now_millis,
            global_decay_multiplier,
            &mut result,
        )
        .await;

        // -- Phase 1: Full -> Summary -----------------------------
        Self::sweep_phase(
            DecayPhase::Full,
            config,
            &engine,
            activation_config,
            storage,
            graph,
            cache,
            now_millis,
            global_decay_multiplier,
            &mut result,
        )
        .await;

        // -- Text.log compaction ----------------------------------
        // Only attempt compaction when this sweep actually created dead
        // space in fulltext.dat. Dead space is created by:
        //   - Full -> Summary transitions (full_text pointers zeroed)
        //   - Ghost -> Deleted transitions (all text removed)
        // If neither occurred, there is no new fragmentation to compact
        // and we skip the expensive write-lock + I/O entirely.
        if result.deletions > 0 || result.full_to_summary > 0 {
            let storage_clone = Arc::clone(storage);
            let compaction_result = tokio::task::spawn_blocking(move || {
                let mut storage_w = storage_clone.write().unwrap_or_else(|e| {
                    error!("storage lock poisoned during compaction check: {e}");
                    e.into_inner()
                });
                storage_w.compact_text_log()
            })
            .await;
            match compaction_result {
                Ok(Ok(_)) => result.compaction_triggered = true,
                Ok(Err(e)) => {
                    error!(error = %e, "fulltext.dat compaction failed");
                }
                Err(e) => {
                    error!(error = %e, "compaction task panicked");
                }
            }
        }

        // -- Persist updated phase bitmaps ------------------------
        {
            let storage_clone = Arc::clone(storage);
            let persist_result = tokio::task::spawn_blocking(move || {
                let storage_r = storage_clone.read().unwrap_or_else(|e| {
                    error!("storage lock poisoned during phase index persist: {e}");
                    e.into_inner()
                });
                storage_r.persist_phase_index()
            })
            .await;
            match persist_result {
                Ok(Err(e)) => {
                    error!(error = %e, "failed to persist phase bitmap index");
                }
                Err(e) => {
                    error!(error = %e, "phase index persist task panicked");
                }
                _ => {}
            }
        }

        result.duration = start.elapsed();
        result
    }

    /// Process all memories in a single phase.
    ///
    /// Loads memory IDs from the PhaseIndex (RoaringBitmap) for the given
    /// phase, then loads metadata in batches to minimize storage lock
    /// acquisitions. For each memory:
    /// 1. Skip if permastore
    /// 2. Calculate raw retrievability R
    /// 3. Calculate connection bonus -> effective R
    /// 4. Determine if a phase transition is needed
    /// 5. Collect the transition into a write batch
    ///
    /// Write batches are flushed every `config.write_batch_size` transitions.
    async fn sweep_phase(
        phase: DecayPhase,
        config: &SweepConfig,
        engine: &FsrsEngine<'_>,
        activation_config: &ActivationConfig,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: &SharedGraph,
        cache: &Arc<CacheManager>,
        now_millis: i64,
        global_decay_multiplier: f64,
        result: &mut SweepResult,
    ) {
        // Get all memory IDs in this phase from the PhaseIndex.
        let memory_ids = Self::load_phase_ids(phase, storage).await;
        let memory_ids = match memory_ids {
            Some(ids) => ids,
            None => return,
        };

        debug!(phase = ?phase, count = memory_ids.len(), "scanning phase");

        let mut pending_transitions: Vec<PendingTransition> = Vec::new();

        // Process records in batches for metadata loading.
        // Each batch acquires the storage lock once to load all metadata,
        // then processes records individually for graph lookups.
        for chunk in memory_ids.chunks(config.write_batch_size) {
            // Batch-load metadata for this chunk under a single lock.
            let (meta_batch, meta_errors) = {
                let storage_clone = Arc::clone(storage);
                let chunk_owned = chunk.to_vec();
                tokio::task::spawn_blocking(move || {
                    let storage_r = storage_clone.read().unwrap_or_else(|e| {
                        error!("storage lock poisoned: {e}");
                        e.into_inner()
                    });
                    Self::load_metadata_batch(&*storage_r, &chunk_owned)
                })
                .await
                .unwrap_or_else(|e| {
                    error!(error = %e, "metadata batch load task panicked");
                    (HashMap::new(), Vec::new())
                })
            };

            // Record batch-load errors.
            for (id, err_msg) in meta_errors {
                result.errors.push(SweepRecordError {
                    memory_id: id,
                    phase,
                    error: err_msg,
                });
            }

            for (i, memory_id) in chunk.iter().enumerate() {
                // Cooperative yielding: give query tasks a chance to run.
                // Track total count across chunks for yield cadence.
                let global_idx = result.memories_scanned as usize;
                if global_idx > 0 && global_idx % config.yield_every_n == 0 {
                    tokio::task::yield_now().await;
                }

                result.memories_scanned += 1;

                let meta = match meta_batch.get(memory_id) {
                    Some(m) => m.clone(),
                    None => {
                        // Memory was deleted or forgotten between bitmap
                        // load and now, or had a load error (already
                        // recorded above).
                        debug!(id = %memory_id, "memory not found in batch, skipping");
                        continue;
                    }
                };

                // Evaluate this record for a possible phase transition.
                let retrievability = Self::evaluate_record(
                    *memory_id,
                    phase,
                    &meta,
                    engine,
                    activation_config,
                    graph,
                    now_millis,
                    global_decay_multiplier,
                    &mut pending_transitions,
                    result,
                )
                .await;

                // A planned transition commits strength with phase/content.
                // Otherwise materialize strength and sync graph state, unless
                // evaluation skipped this permastore/decay-disabled record.
                let has_transition = pending_transitions
                    .last()
                    .is_some_and(|t| t.memory_id == *memory_id);
                if let Some(evaluation) = retrievability.filter(|_| !has_transition) {
                    Self::update_record_state(
                        *memory_id,
                        phase,
                        &evaluation,
                        &meta,
                        storage,
                        graph,
                        result,
                    )
                    .await;
                }

                // Flush write batch if full.
                if pending_transitions.len() >= config.write_batch_size {
                    Self::flush_transitions(&pending_transitions, storage, graph, cache, result)
                        .await;
                    pending_transitions.clear();
                }

                let _ = i; // suppress unused warning
            }
        }

        // Flush remaining transitions.
        if !pending_transitions.is_empty() {
            Self::flush_transitions(&pending_transitions, storage, graph, cache, result).await;
        }
    }

    /// Load all memory IDs belonging to the given phase.
    ///
    /// Tries the PhaseIndex bitmap first, falls back to a full table scan.
    /// Returns `None` if both lookups fail (the phase is skipped).
    async fn load_phase_ids(
        phase: DecayPhase,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    ) -> Option<Vec<MemoryId>> {
        let storage_clone = Arc::clone(storage);
        tokio::task::spawn_blocking(move || {
            let storage_r = storage_clone.read().unwrap_or_else(|e| {
                error!("storage lock poisoned: {e}");
                e.into_inner()
            });
            match storage_r.ids_in_phase(phase) {
                Ok(ids) => Some(ids),
                Err(e) => {
                    warn!(
                        phase = ?phase,
                        error = %e,
                        "PhaseIndex lookup failed, falling back to full table scan"
                    );
                    match storage_r.scan_phase_records(phase) {
                        Ok(records) => Some(
                            records
                                .into_iter()
                                .map(|(id, _): (MemoryId, _)| id)
                                .collect(),
                        ),
                        Err(e2) => {
                            error!(
                                phase = ?phase,
                                error = %e2,
                                "full table scan also failed, skipping phase"
                            );
                            None
                        }
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|e| {
            error!(error = %e, "load_phase_ids task panicked");
            None
        })
    }

    /// Load decay metadata for a batch of memory IDs in a single pass.
    ///
    /// Pre-loads namespace configs to avoid repeated per-record lookups,
    /// then iterates the IDs calling `get_record` for each. All of this
    /// happens under the caller's already-acquired storage lock, so the
    /// lock is held once per batch rather than once per record.
    ///
    /// Missing and tombstoned records (deleted or forgotten between
    /// bitmap load and now) are silently omitted. Errors are collected
    /// in the returned vector.
    fn load_metadata_batch(
        storage: &RedbStorageEngine,
        ids: &[MemoryId],
    ) -> (HashMap<MemoryId, DecayMetadata>, Vec<(MemoryId, String)>) {
        // Pre-load namespace configs to avoid repeated lookups.
        // There are typically very few namespaces (< 10).
        let namespaces = match storage.list_namespaces() {
            Ok(namespaces) => namespaces,
            Err(e) => {
                return (
                    HashMap::new(),
                    ids.iter()
                        .map(|&id| (id, format!("failed to load namespace policy: {e}")))
                        .collect(),
                );
            }
        };
        let ns_cache: HashMap<_, _> = namespaces.into_iter().map(|ns| (ns.id.get(), ns)).collect();

        let mut results = HashMap::with_capacity(ids.len());
        let mut errors = Vec::new();

        for &id in ids {
            match storage.get_record(id) {
                Ok(Some(record)) => {
                    // Forgotten between bitmap load and now -- skip.
                    // `forget_memory` keeps the row, so unlike a deleted
                    // record this one is still here to load, and the
                    // sweep would evaluate it against the phase it was
                    // in when the snapshot was taken.
                    if record.phase == DecayPhase::Tombstone {
                        continue;
                    }

                    let Some(namespace) = ns_cache.get(&record.namespace_id).cloned() else {
                        errors.push((id, "namespace policy not found".to_string()));
                        continue;
                    };

                    results.insert(
                        id,
                        DecayMetadata {
                            phase: record.phase,
                            created_at: record.created_at,
                            stability: record.stability,
                            last_accessed_at: record.last_accessed_at,
                            is_permastore: record.is_permastore != 0,
                            namespace,
                            has_summary: !record.summary.is_empty(),
                        },
                    );
                }
                Ok(None) => {
                    // Memory deleted between bitmap load and now -- skip.
                }
                Err(e) => {
                    errors.push((id, format!("failed to load metadata: {e}")));
                }
            }
        }

        (results, errors)
    }

    /// Evaluate a single record for a possible phase transition.
    ///
    /// Checks permastore exemption, computes raw and effective
    /// retrievability, and pushes a `PendingTransition` if the
    /// effective R falls below the phase threshold.
    ///
    /// Returns frozen clock/policy inputs and the scan-time projection if evaluated
    /// (not skipped), or `None` if the record was skipped (permastore
    /// or decay disabled).
    #[allow(clippy::too_many_arguments)]
    async fn evaluate_record(
        memory_id: MemoryId,
        phase: DecayPhase,
        meta: &DecayMetadata,
        engine: &FsrsEngine<'_>,
        activation_config: &ActivationConfig,
        graph: &SharedGraph,
        now_millis: i64,
        global_decay_multiplier: f64,
        pending_transitions: &mut Vec<PendingTransition>,
        result: &mut SweepResult,
    ) -> Option<SweepEvaluation> {
        if meta.phase != phase {
            return None;
        }

        // -- Permastore exemption ---------------------------------
        if meta.is_permastore {
            result.permastore_skipped += 1;
            return None;
        }

        let policy = match super::lifecycle::resolve_policy(
            &meta.namespace,
            engine.config,
            global_decay_multiplier,
        ) {
            Ok(policy) => policy,
            Err(e) => {
                result.errors.push(SweepRecordError {
                    memory_id,
                    phase,
                    error: e.to_string(),
                });
                return None;
            }
        };
        if !meta.stability.is_finite() || meta.stability <= 0.0 {
            result.errors.push(SweepRecordError {
                memory_id,
                phase,
                error: "invalid record stability".to_string(),
            });
            return None;
        }
        let engine = FsrsEngine::new(&policy.config);
        let multiplier = policy.decay_rate_multiplier;

        // -- Decay disabled check ---------------------------------
        if multiplier == 0.0 {
            result.decay_disabled_skipped += 1;
            return None;
        }

        let connection_bonus =
            Self::calculate_connection_bonus(memory_id, graph, activation_config).await;
        let projection = match super::lifecycle::project(
            meta.stability,
            meta.last_accessed_at,
            &policy,
            now_millis,
            connection_bonus,
        ) {
            Ok(projection) => projection,
            Err(e) => {
                result.errors.push(SweepRecordError {
                    memory_id,
                    phase,
                    error: e.to_string(),
                });
                return None;
            }
        };
        let raw_r = projection.strength;
        let effective_r = projection.decay_strength;

        // -- Determine transition ---------------------------------
        let threshold = match phase {
            DecayPhase::Ghost => engine.config.phase_3_threshold,
            DecayPhase::Summary => engine.config.phase_2_threshold,
            DecayPhase::Full => engine.config.phase_1_threshold,
            // Tombstone is a terminal phase set by explicit deletion;
            // the decay sweep never processes tombstoned memories.
            DecayPhase::Tombstone => return None,
        };

        let evaluation = SweepEvaluation {
            projection,
            policy,
            activation: activation_config.clone(),
        };

        if effective_r <= threshold && phase == DecayPhase::Full && !meta.has_summary {
            result.errors.push(SweepRecordError {
                memory_id,
                phase,
                error: "missing_summary: retaining full text".to_string(),
            });
        } else if effective_r <= threshold {
            pending_transitions.push(PendingTransition {
                memory_id,
                from_phase: phase,
                evaluation: evaluation.clone(),
                expected: meta.clone(),
            });
        } else if raw_r <= threshold && effective_r > threshold {
            // Raw R crossed the threshold, but connection bonus
            // pulled effective R back above it.
            result.saved_by_connection_bonus += 1;
        }

        Some(evaluation)
    }

    /// Reproject strength from the current locked graph using the original
    /// sampled clock and policy, then update metadata and graph together.
    async fn update_record_state(
        memory_id: MemoryId,
        phase: DecayPhase,
        evaluation: &SweepEvaluation,
        meta: &DecayMetadata,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: &SharedGraph,
        result: &mut SweepResult,
    ) {
        let storage = Arc::clone(storage);
        let expected = meta.clone();
        let evaluation = evaluation.clone();
        // Always graph -> storage: never await a graph lock holding a storage lock.
        let mut graph = Arc::clone(graph).write_owned().await;
        let outcome = tokio::task::spawn_blocking(move || {
            let storage = storage.write().map_err(|e| e.to_string())?;
            let current = storage.get_record(memory_id).map_err(|e| e.to_string())?;
            if !current.as_ref().is_some_and(|r| expected.matches(r)) {
                return Ok(());
            }
            let current = current.expect("snapshot match requires a record");
            let Some(projection) = evaluation
                .project_locked(memory_id, &expected, &storage, &graph)
                .map_err(|e| e.to_string())?
            else {
                return Ok(());
            };
            let raw_r = projection.strength;
            let effective_r = projection.decay_strength;
            if !storage
                .apply_legacy_decay(
                    memory_id,
                    &current,
                    &expected.namespace,
                    phase,
                    raw_r,
                    effective_r,
                )
                .map_err(|e| e.to_string())?
            {
                return Ok(());
            }
            let _ = graph.update_node_state(memory_id, phase, effective_r);
            Ok::<_, String>(())
        })
        .await;
        let error = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(e) => Some(e.to_string()),
        };
        if let Some(error) = error {
            result.errors.push(SweepRecordError {
                memory_id,
                phase,
                error,
            });
        }
    }

    /// Calculate the connection bonus for a memory based on spreading
    /// activation from its neighbors.
    ///
    /// Acquires a read lock on the graph. The lock is held only for
    /// the duration of the calculation (~1-10 us per memory).
    async fn calculate_connection_bonus(
        memory_id: MemoryId,
        graph: &SharedGraph,
        activation_config: &ActivationConfig,
    ) -> f32 {
        let graph_r = graph.read().await;
        let raw_bonus = connection_bonus(memory_id, &*graph_r, activation_config);
        raw_bonus.clamp(0.0, MAX_CONNECTION_BONUS)
    }

    /// Apply a batch of pending transitions to storage, graph, and cache.
    ///
    /// Each transition type has different side-effects:
    /// - Full -> Summary: ensure summary exists, delete full_text, update phase
    /// - Summary -> Ghost: delete summary, update phase
    /// - Ghost -> Deleted: remove everything (embedding, edges, metadata, indexes)
    async fn flush_transitions(
        transitions: &[PendingTransition],
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: &SharedGraph,
        cache: &Arc<CacheManager>,
        result: &mut SweepResult,
    ) {
        for t in transitions {
            let outcome = Self::apply_transition(t.clone(), storage, graph).await;
            // Even a partial storage failure may have changed the canonical phase.
            cache.invalidate(t.memory_id).await;
            match outcome {
                Ok(None) => continue, // Snapshot changed: retry on the next sweep.
                Ok(Some(transition)) => {
                    // Update stats
                    match t.from_phase {
                        DecayPhase::Full => result.full_to_summary += 1,
                        DecayPhase::Summary => result.summary_to_ghost += 1,
                        DecayPhase::Ghost => result.deletions += 1,
                        DecayPhase::Tombstone => {} // unreachable, handled above
                    }

                    result.transitions.push(transition);
                }
                Err(e) => {
                    result.errors.push(SweepRecordError {
                        memory_id: t.memory_id,
                        phase: t.from_phase,
                        error: format!("transition failed: {e}"),
                    });
                }
            }
        }
    }
}

// ── Transition Methods ──────────────────────────────────────────────

impl DecaySweepRunner {
    async fn apply_transition(
        t: PendingTransition,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: &SharedGraph,
    ) -> Result<Option<PhaseTransition>, SweepError> {
        let storage = Arc::clone(storage);
        let mut graph = Arc::clone(graph).write_owned().await;
        tokio::task::spawn_blocking(move || {
            let mut storage = storage
                .write()
                .map_err(|e| SweepError::Storage(e.to_string()))?;
            let current = storage
                .get_record(t.memory_id)
                .map_err(|e| SweepError::Storage(e.to_string()))?;
            if !current.as_ref().is_some_and(|r| t.expected.matches(r)) {
                return Ok(None);
            }
            let current = current.expect("snapshot match requires a record");
            let Some(projection) =
                t.evaluation
                    .project_locked(t.memory_id, &t.expected, &storage, &graph)?
            else {
                return Ok(None);
            };
            let Some(threshold) = t.evaluation.threshold(t.from_phase) else {
                return Ok(None);
            };
            if projection.decay_strength > threshold {
                return Ok(None);
            }
            let map_error = |e: crate::storage::StorageError| SweepError::Storage(e.to_string());
            let new_phase = match t.from_phase {
                DecayPhase::Full => DecayPhase::Summary,
                DecayPhase::Summary => DecayPhase::Ghost,
                DecayPhase::Ghost => {
                    let namespace = storage
                        .get_namespace(crate::model::NamespaceId::new(current.namespace_id))
                        .map_err(map_error)?;
                    if namespace
                        .as_ref()
                        .map(serde_json::to_vec)
                        .transpose()
                        .map_err(|e| SweepError::Storage(e.to_string()))?
                        != Some(
                            serde_json::to_vec(&t.expected.namespace)
                                .map_err(|e| SweepError::Storage(e.to_string()))?,
                        )
                    {
                        return Ok(None);
                    }
                    // Plan privately under the final expected-state and policy guards.
                    let mut planned = (*graph).clone();
                    let removal = planned.remove_memory_with_bridging(t.memory_id);
                    let bridges = removal
                        .bridge_created
                        .into_iter()
                        .map(|bridge| {
                            let source = planned
                                .get_node_by_key(bridge.source)
                                .ok_or_else(|| SweepError::Storage("bridge source missing".into()))?
                                .memory_id;
                            let target = planned
                                .get_node_by_key(bridge.target)
                                .ok_or_else(|| SweepError::Storage("bridge target missing".into()))?
                                .memory_id;
                            Ok(crate::storage::PersistedEdge {
                                source,
                                target,
                                edge_type: bridge.edge_type,
                                weight: bridge.weight,
                                auto_created: bridge.auto_created,
                                created_at: bridge.created_at as u64,
                            })
                        })
                        .collect::<Result<Vec<_>, SweepError>>()?;
                    let Some(_) = storage
                        .delete_memory_with_edges(t.memory_id, &bridges)
                        .map_err(map_error)?
                    else {
                        return Ok(None);
                    };
                    *graph = planned;
                    // Derived invalidations remain pending until complete publication.
                    return Ok(Some(PhaseTransition::Deleted {
                        id: t.memory_id,
                        old_phase: t.from_phase,
                        retrievability: projection.decay_strength,
                    }));
                }
                DecayPhase::Tombstone => return Ok(None),
            };
            if t.from_phase == DecayPhase::Full && !t.expected.has_summary {
                return Err(SweepError::Storage(
                    "missing_summary: retaining full text".into(),
                ));
            }
            if !storage
                .apply_legacy_decay(
                    t.memory_id,
                    &current,
                    &t.expected.namespace,
                    new_phase,
                    projection.strength,
                    projection.decay_strength,
                )
                .map_err(map_error)?
            {
                return Ok(None);
            }
            // Publish only after the atomic metadata phase/content commit.
            let _ = graph.update_node_state(t.memory_id, new_phase, projection.decay_strength);
            let transition = if new_phase == DecayPhase::Summary {
                PhaseTransition::Summarized {
                    id: t.memory_id,
                    old_phase: t.from_phase,
                    new_phase,
                    retrievability: projection.decay_strength,
                }
            } else {
                PhaseTransition::Ghosted {
                    id: t.memory_id,
                    old_phase: t.from_phase,
                    new_phase,
                    retrievability: projection.decay_strength,
                }
            };
            Ok(Some(transition))
        })
        .await
        .map_err(|e| SweepError::Storage(format!("transition task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiskRecord, NamespaceConfig};

    const DIM: usize = 4;

    #[tokio::test]
    async fn fixed_time_sweep_obeys_namespace_thresholds_and_preserves_content_phases() {
        use crate::api::StorageEngine as _;
        use crate::test_support::{Fixture, MemorySpec};
        let f = Fixture::new();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.name = "threshold".into();
        ns.embedding_dim = DIM as u32;
        ns.phase_thresholds.full_to_summary = 0.95;
        let namespace = f.storage.write().unwrap().create_namespace(&ns).unwrap();
        let full = f.insert_memory_in(namespace, &[]).await;
        let storage = crate::api::adapters::StorageEngineAdapter::new(
            f.storage.clone(),
            f.cache.clone(),
            f.vector_index.clone(),
            f.fts_index.clone(),
            f.entity_index.clone(),
            f.graph.clone(),
        )
        .with_clock(Arc::new(crate::time::ManualClock::new(0)));
        let missing = storage
            .create_memory(
                namespace,
                "initial valid summary",
                Some("only retained text"),
                &[],
                &[1., 0., 0., 0.],
                Some(1.0),
                None,
            )
            .await
            .unwrap()
            .id;
        // Model pre-existing content loss through the legacy storage patch;
        // public insertion now rejects empty summaries at its own boundary.
        f.storage.read().unwrap().strip_summary(missing).unwrap();
        assert_eq!(f.record(missing).unwrap().created_at, 0);
        let summary = f
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Summary,
                ..MemorySpec::new(namespace)
            })
            .await;
        // At day one R=0.9 for S=1: custom threshold degrades Full;
        // the default 0.7 does not. A batch size of one covers flush ordering.
        let config = SweepConfig {
            write_batch_size: 1,
            ..SweepConfig::default()
        };
        let decay = DecayConfig::default();
        let activation = ActivationConfig::default();
        let run = |now| {
            DecaySweepRunner::execute_sweep_at(
                &config,
                &decay,
                &activation,
                &f.storage,
                &f.graph,
                &f.cache,
                1.0,
                Some(now),
            )
        };
        let result = run(86_400_000).await;
        assert_eq!(f.record(full).unwrap().phase, DecayPhase::Summary);
        assert_eq!(f.record(summary).unwrap().phase, DecayPhase::Summary);
        assert_eq!(f.record(missing).unwrap().phase, DecayPhase::Full);
        assert_eq!(
            storage.get_full_text(missing).await.unwrap().as_deref(),
            Some("only retained text")
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.memory_id == missing && e.error.contains("missing_summary"))
        );
        run(i64::MAX).await;
        assert_eq!(
            f.record(full).unwrap().phase,
            DecayPhase::Ghost,
            "one destructive phase per sweep"
        );
        assert!(f.record(full).unwrap().summary.is_empty());
        assert_eq!(f.record(missing).unwrap().phase, DecayPhase::Full);
    }

    #[tokio::test]
    async fn manual_sweep_uses_injected_clock_and_zero_multiplier_skips_degradation() {
        use crate::api::FsrsEngine as _;
        use crate::test_support::Fixture;
        let f = Fixture::new();
        let clock = Arc::new(crate::time::ManualClock::new(0));
        let adapter = crate::api::adapters::FsrsEngineAdapter::new(
            f.storage.clone(),
            f.cache.clone(),
            f.graph.clone(),
            Arc::new(crate::config::RecalldConfig::default()),
            false,
        )
        .with_clock(clock.clone());
        let full = f.insert_memory().await;
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.name = "disabled".into();
        ns.embedding_dim = DIM as u32;
        ns.decay_rate_multiplier = Some(0.0);
        let namespace = f.storage.write().unwrap().create_namespace(&ns).unwrap();
        let disabled = f.insert_memory_in(namespace, &[]).await;
        adapter.trigger_sweep(None).await.unwrap();
        assert_eq!(f.record(full).unwrap().phase, DecayPhase::Full);
        clock.advance_to(i64::MAX).unwrap();
        adapter.trigger_sweep(None).await.unwrap();
        assert_eq!(f.record(full).unwrap().phase, DecayPhase::Summary);
        assert_eq!(f.record(disabled).unwrap().phase, DecayPhase::Full);
        assert_eq!(f.record(disabled).unwrap().last_accessed_at, 0);
    }

    #[tokio::test]
    async fn empty_successful_compaction_is_reported() {
        let fixture = crate::test_support::Fixture::new();
        let id = fixture
            .insert_memory_with(crate::test_support::MemorySpec {
                phase: DecayPhase::Ghost,
                summary: "",
                ..crate::test_support::MemorySpec::new(crate::model::NamespaceId::new(1))
            })
            .await;
        let before = fixture
            .storage
            .read()
            .unwrap()
            .meta_store()
            .text_selection()
            .unwrap()
            .unwrap();

        // No text was ever appended. Deleting the expired ghost still selects
        // a fresh empty generation and must report that maintenance commit.
        let result = DecaySweepRunner::execute_sweep_at(
            &SweepConfig::default(),
            &DecayConfig::default(),
            &ActivationConfig::default(),
            &fixture.storage,
            &fixture.graph,
            &fixture.cache,
            1.0,
            Some(1_000_000 * 86_400_000),
        )
        .await;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.deletions, 1);
        assert!(fixture.record(id).is_none());
        let after = fixture
            .storage
            .read()
            .unwrap()
            .meta_store()
            .text_selection()
            .unwrap()
            .unwrap();
        assert_ne!(before.id, after.id);
        assert_eq!(before.sealed_length, after.sealed_length);
        assert!(result.compaction_triggered);
    }

    /// An engine holding one live, Full-phase memory.
    fn engine_with_one_memory() -> (tempfile::TempDir, RedbStorageEngine, MemoryId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = RedbStorageEngine::open(dir.path()).expect("open engine");

        let mut config = NamespaceConfig::default_namespace(0);
        config.name = "sweep".to_string();
        config.embedding_dim = DIM as u32;
        let namespace_id = engine.create_namespace(&config).expect("create namespace");

        let id = MemoryId::new();
        let mut record = DiskRecord {
            schema: None,
            version: DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: namespace_id.get(),
            created_at: 0,
            last_accessed_at: 0,
            phase: DecayPhase::Full,
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
        engine
            .insert_memory(id, namespace_id, &mut record, &[0.0; DIM], None)
            .expect("insert memory");

        (dir, engine, id)
    }

    /// `sweep_phase` takes its list of ids once, up front, and loads each
    /// chunk later. A memory forgotten in between is still on that list,
    /// and `forget_memory` keeps the row, so nothing told the sweep it
    /// was looking at a tombstone: it evaluated the record against the
    /// phase it was sweeping and wrote that phase back.
    #[test]
    fn a_memory_tombstoned_after_the_phase_snapshot_is_not_swept() {
        let (_dir, engine, id) = engine_with_one_memory();
        let snapshot = engine.ids_in_phase(DecayPhase::Full).expect("ids in phase");
        assert_eq!(snapshot, vec![id]);

        engine.tombstone_memory(id).expect("tombstone");

        let (batch, errors) = DecaySweepRunner::load_metadata_batch(&engine, &snapshot);

        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            !batch.contains_key(&id),
            "a tombstone was handed to the sweep as a Full-phase memory"
        );
    }
}

#[cfg(test)]
mod independent_review_regressions {
    use super::*;
    use crate::api::FsrsEngine as _;
    use crate::model::{AccessKind, NamespaceConfig};
    use crate::test_support::Fixture;
    const DAY: i64 = 86_400_000;

    fn test_evaluation(meta: &DecayMetadata) -> SweepEvaluation {
        let policy =
            super::super::lifecycle::resolve_policy(&meta.namespace, &DecayConfig::default(), 1.0)
                .unwrap();
        let projection = super::super::lifecycle::project(
            meta.stability,
            meta.last_accessed_at,
            &policy,
            i64::MAX,
            0.0,
        )
        .unwrap();
        SweepEvaluation {
            projection,
            policy,
            activation: ActivationConfig::default(),
        }
    }

    fn adapter(f: &Fixture, now: i64) -> crate::api::adapters::FsrsEngineAdapter {
        crate::api::adapters::FsrsEngineAdapter::new(
            f.storage.clone(),
            f.cache.clone(),
            f.graph.clone(),
            Arc::new(crate::config::RecalldConfig::default()),
            false,
        )
        .with_clock(Arc::new(crate::time::ManualClock::new(now)))
    }

    #[tokio::test]
    async fn review_stale_sweep_must_not_overwrite_committed_feedback() {
        let f = Fixture::new();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.name = "review-feedback".into();
        ns.embedding_dim = crate::test_support::DIM as u32;
        ns.permastore_threshold = 2.0;
        let namespace = f.storage.write().unwrap().create_namespace(&ns).unwrap();
        let id = f.insert_memory_in(namespace, &[]).await;
        let (snapshot, errors) =
            DecaySweepRunner::load_metadata_batch(&f.storage.read().unwrap(), &[id]);
        assert!(errors.is_empty());
        let meta = snapshot.get(&id).unwrap();
        let decay = DecayConfig::default();
        let engine = FsrsEngine::new(&decay);
        let mut transitions = Vec::new();
        let mut result = SweepResult::default();
        let evaluation = DecaySweepRunner::evaluate_record(
            id,
            DecayPhase::Full,
            meta,
            &engine,
            &ActivationConfig::default(),
            &f.graph,
            8 * DAY,
            1.0,
            &mut transitions,
            &mut result,
        )
        .await
        .unwrap();
        assert_eq!(transitions.len(), 1);
        // Deterministically insert feedback exactly between scan/plan and apply.
        let generation = f
            .storage
            .read()
            .unwrap()
            .control_state()
            .unwrap()
            .committed_generation
            .get();
        let feedback = adapter(&f, 8 * DAY).reinforce(id, 3).await.unwrap();
        assert_eq!(
            f.storage
                .read()
                .unwrap()
                .control_state()
                .unwrap()
                .committed_generation
                .get(),
            generation + 1
        );
        assert!(feedback.is_permastore);
        assert_eq!(
            f.record(id).unwrap().access_history.last().unwrap().kind,
            AccessKind::ManualReinforcement
        );
        let committed = f.record(id).unwrap();
        DecaySweepRunner::update_record_state(
            id,
            DecayPhase::Full,
            &evaluation,
            meta,
            &f.storage,
            &f.graph,
            &mut result,
        )
        .await;
        DecaySweepRunner::flush_transitions(
            &transitions,
            &f.storage,
            &f.graph,
            &f.cache,
            &mut result,
        )
        .await;
        let after = f.record(id).unwrap();
        assert_eq!(
            (after.stability, after.is_permastore, after.phase),
            (
                committed.stability,
                committed.is_permastore,
                committed.phase
            )
        );
    }

    #[tokio::test]
    async fn review_successful_sweep_must_publish_final_graph_phase() {
        let f = Fixture::new();
        let id = f.insert_memory().await;
        let sweep = adapter(&f, i64::MAX);
        for _ in 0..2 {
            let generation = f
                .storage
                .read()
                .unwrap()
                .control_state()
                .unwrap()
                .committed_generation
                .get();
            let result = sweep.trigger_sweep(None).await.unwrap();
            assert!(result.errors.is_empty());
            assert_eq!(
                f.storage
                    .read()
                    .unwrap()
                    .control_state()
                    .unwrap()
                    .committed_generation
                    .get(),
                generation + 1 + u64::from(result.compaction_triggered),
                "one phase/content commit plus an explicitly reported compaction"
            );
        }
        assert_eq!(f.record(id).unwrap().phase, DecayPhase::Ghost);
        assert_eq!(
            f.graph.read().await.get_node(&id).unwrap().decay_phase,
            DecayPhase::Ghost
        );
    }

    #[tokio::test]
    async fn atomic_sweep_strips_real_payload_references_and_retains_v1_tags_after_reopen() {
        use crate::api::StorageEngine as _;
        let f = Fixture::new();
        let storage = crate::api::adapters::StorageEngineAdapter::new(
            f.storage.clone(),
            f.cache.clone(),
            f.vector_index.clone(),
            f.fts_index.clone(),
            f.entity_index.clone(),
            f.graph.clone(),
        )
        .with_clock(Arc::new(crate::time::ManualClock::new(0)));
        let id = storage
            .create_memory(
                crate::model::NamespaceId::new(1),
                "retained summary",
                Some("retained full text"),
                &["retained".into()],
                &[1., 0., 0., 0.],
                Some(1.),
                None,
            )
            .await
            .unwrap()
            .id;
        assert_eq!(
            storage.get_full_text(id).await.unwrap().as_deref(),
            Some("retained full text")
        );
        // Actual insertion now completes graph/index publication itself.
        assert!(f.graph.read().await.get_node(&id).is_some());
        let sweep = adapter(&f, i64::MAX);
        for phase in [DecayPhase::Summary, DecayPhase::Ghost] {
            let generation = f
                .storage
                .read()
                .unwrap()
                .control_state()
                .unwrap()
                .committed_generation
                .get();
            let result = sweep.trigger_sweep(None).await.unwrap();
            assert!(result.errors.is_empty());
            let record = f.record(id).unwrap();
            assert_eq!(record.phase, phase);
            assert_eq!((record.text_offset, record.text_length), (0, 0));
            assert_eq!(record.summary.is_empty(), phase == DecayPhase::Ghost);
            assert!(storage.get_full_text(id).await.unwrap().is_none());
            assert_eq!(
                f.storage
                    .read()
                    .unwrap()
                    .control_state()
                    .unwrap()
                    .committed_generation
                    .get(),
                generation + 1 + u64::from(result.compaction_triggered)
            );
            assert_eq!(
                f.graph.read().await.get_node(&id).unwrap().decay_phase,
                phase
            );
        }
        let expected = f.record(id).unwrap();
        drop((storage, sweep));
        let dir = f.close();
        let reopened = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get_record(id).unwrap().unwrap().to_bytes(),
            expected.to_bytes()
        );
        assert_eq!(reopened.ids_in_phase(DecayPhase::Ghost).unwrap(), vec![id]);
        assert_eq!(reopened.memories_with_tag("retained").unwrap(), vec![id]);
    }

    #[tokio::test]
    async fn review_access_must_not_move_feedback_anchor_backward() {
        let f = Fixture::new();
        let id = f.insert_memory().await;
        adapter(&f, 8 * DAY).reinforce(id, 3).await.unwrap();
        assert_eq!(f.record(id).unwrap().last_accessed_at, 8 * DAY);
        adapter(&f, DAY)
            .record_access(id, AccessKind::DirectRetrieval)
            .await;
        assert_eq!(f.record(id).unwrap().last_accessed_at, 8 * DAY);
    }
    #[tokio::test]
    async fn stale_transition_checks_each_expected_field_before_storage_or_graph_mutation() {
        use crate::test_support::MemorySpec;
        for phase in [DecayPhase::Full, DecayPhase::Summary, DecayPhase::Ghost] {
            for field in ["timestamp", "stability", "phase", "permastore"] {
                let f = Fixture::new();
                let id = f
                    .insert_memory_with(MemorySpec {
                        phase,
                        ..MemorySpec::new(crate::model::NamespaceId::new(1))
                    })
                    .await;
                let (snapshot, errors) =
                    DecaySweepRunner::load_metadata_batch(&f.storage.read().unwrap(), &[id]);
                assert!(errors.is_empty());
                let expected = snapshot[&id].clone();
                {
                    let storage = f.storage.write().unwrap();
                    if field == "timestamp" {
                        storage
                            .update_access(id, DAY, AccessKind::DirectRetrieval)
                            .unwrap();
                    } else if field == "phase" {
                        // Reach terminal state through the actual removal writer,
                        // so its content and removal evidence agree.
                        storage.tombstone_memory(id).unwrap();
                    } else {
                        storage
                            .update_decay_state(
                                id,
                                phase,
                                1.0,
                                1.0,
                                if field == "stability" { 2.0 } else { 1.0 },
                                field == "permastore",
                            )
                            .unwrap();
                    }
                }
                let before = f.record(id).unwrap();
                let graph_before = f.graph.read().await.get_node(&id).unwrap().decay_phase;
                let outcome = DecaySweepRunner::apply_transition(
                    PendingTransition {
                        memory_id: id,
                        from_phase: phase,
                        evaluation: test_evaluation(&expected),
                        expected,
                    },
                    &f.storage,
                    &f.graph,
                )
                .await
                .unwrap();
                assert!(outcome.is_none(), "{phase:?} {field}");
                let after = f.record(id).unwrap();
                assert_eq!(
                    (
                        after.phase,
                        after.stability,
                        after.is_permastore,
                        after.last_accessed_at,
                        after.summary
                    ),
                    (
                        before.phase,
                        before.stability,
                        before.is_permastore,
                        before.last_accessed_at,
                        before.summary
                    )
                );
                assert_eq!(
                    f.graph.read().await.get_node(&id).unwrap().decay_phase,
                    graph_before
                );
            }
        }
    }
    /// Hold graph publication back while purge commits and feedback completes.
    /// Predicates and task completion establish the interleaving; no sleeps.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn purge_releases_storage_before_graph_publication_and_stale_sweep_skips() {
        use crate::mcp::bridge::{DeleteNamespaceInput, NamespaceRegistry as _};
        let f = Fixture::new();
        let namespace = f.create_namespace("lock-order");
        let id = f.insert_memory_in(namespace, &[]).await;
        let live = f.insert_memory().await;
        let (snapshot, _) =
            DecaySweepRunner::load_metadata_batch(&f.storage.read().unwrap(), &[id]);
        let planned = PendingTransition {
            memory_id: id,
            from_phase: DecayPhase::Full,
            evaluation: test_evaluation(&snapshot[&id]),
            expected: snapshot[&id].clone(),
        };
        let graph_guard = f.graph.clone().write_owned().await;
        let namespaces = f.namespace_adapter();
        let purge = tokio::spawn(async move {
            namespaces
                .delete_namespace(DeleteNamespaceInput {
                    name: "lock-order".into(),
                    force: true,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let committed_and_unlocked = f
                    .storage
                    .try_read()
                    .is_ok_and(|storage| storage.get_namespace(namespace).unwrap().is_none());
                if committed_and_unlocked {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("purge must release storage before awaiting graph");
        tokio::time::timeout(Duration::from_secs(5), adapter(&f, DAY).reinforce(live, 3))
            .await
            .expect("feedback must not wait for graph holding storage")
            .unwrap();
        assert!(
            !purge.is_finished(),
            "purge publication must wait for graph"
        );
        let storage = f.storage.clone();
        let graph = f.graph.clone();
        let sweep = tokio::spawn(async move {
            DecaySweepRunner::apply_transition(planned, &storage, &graph).await
        });
        drop(graph_guard);
        tokio::time::timeout(Duration::from_secs(5), async {
            purge.await.unwrap().unwrap();
            assert!(sweep.await.unwrap().unwrap().is_none());
        })
        .await
        .expect("graph and storage users must finish after publication is released");
        assert!(f.record(id).is_none());
        assert!(!f.graph.read().await.contains(&id));
    }
}

#[cfg(test)]
mod storage_independent_review {
    use super::*;
    use crate::test_support::Fixture;
    #[tokio::test]
    async fn review_graph_change_must_revalidate_destructive_sweep_plan() {
        let f = Fixture::new();
        let id = f.insert_memory().await;
        let neighbor = f.insert_memory().await;
        let (snapshot, errors) =
            DecaySweepRunner::load_metadata_batch(&f.storage.read().unwrap(), &[id]);
        assert!(errors.is_empty());
        let meta = snapshot.get(&id).unwrap();
        let decay = DecayConfig::default();
        let engine = FsrsEngine::new(&decay);
        let activation = ActivationConfig::default();
        let mut transitions = Vec::new();
        let mut result = SweepResult::default();
        let evaluation = DecaySweepRunner::evaluate_record(
            id,
            DecayPhase::Full,
            meta,
            &engine,
            &activation,
            &f.graph,
            9 * 86_400_000 / 2,
            1.0,
            &mut transitions,
            &mut result,
        )
        .await
        .unwrap();
        let raw = evaluation.projection.strength;
        let old_effective = evaluation.projection.decay_strength;
        assert_eq!(
            transitions.len(),
            1,
            "fixture must plan destructive phase loss"
        );
        // A real canonical edge and graph publication win between plan and apply.
        {
            let mut graph = f.graph.write().await;
            let storage = f.storage.write().unwrap();
            storage
                .add_edge(
                    id,
                    neighbor,
                    crate::model::EdgeType::Associative,
                    1.,
                    false,
                    1,
                )
                .unwrap();
            storage.update_edge_count(id, 1).unwrap();
            graph
                .add_edge(id, neighbor, crate::model::EdgeType::Associative, 1., false)
                .unwrap();
        }
        let bonus = DecaySweepRunner::calculate_connection_bonus(id, &f.graph, &activation).await;
        let new_effective = engine.effective_retrievability(raw, bonus);
        eprintln!(
            "old={old_effective}, new={new_effective}, threshold={}",
            meta.namespace.phase_thresholds.full_to_summary
        );
        assert!(new_effective > meta.namespace.phase_thresholds.full_to_summary);
        let outcome =
            DecaySweepRunner::apply_transition(transitions.remove(0), &f.storage, &f.graph)
                .await
                .unwrap();
        assert!(
            outcome.is_none(),
            "stale graph-derived plan destroyed content despite a saving committed edge"
        );
        assert_eq!(f.record(id).unwrap().phase, DecayPhase::Full);
    }
    async fn publish_edge(f: &Fixture, id: MemoryId, neighbor: MemoryId) {
        let mut graph = f.graph.write().await;
        let storage = f.storage.write().unwrap();
        // A retained ghost may gain a live child; new associative ghost links are forbidden.
        let kind = if storage.get_record(id).unwrap().unwrap().phase == DecayPhase::Ghost {
            crate::model::EdgeType::ParentChild
        } else {
            crate::model::EdgeType::Associative
        };
        storage.add_edge(id, neighbor, kind, 1., false, 1).unwrap();
        storage.update_edge_count(id, 1).unwrap();
        graph.add_edge(id, neighbor, kind, 1., false).unwrap();
    }

    async fn publish_neighbor_strength(f: &Fixture, neighbor: MemoryId, strength: f32) {
        let mut graph = f.graph.write().await;
        let storage = f.storage.write().unwrap();
        storage
            .update_decay_state(neighbor, DecayPhase::Full, strength, strength, 1., false)
            .unwrap();
        graph
            .update_node_state(neighbor, DecayPhase::Full, strength)
            .unwrap();
    }

    #[tokio::test]
    async fn summary_and_ghost_plans_recheck_graph_before_removal_and_can_still_commit() {
        use crate::test_support::MemorySpec;
        for phase in [DecayPhase::Summary, DecayPhase::Ghost] {
            let f = Fixture::new();
            let id = f
                .insert_memory_with(MemorySpec {
                    phase,
                    summary: if phase == DecayPhase::Ghost {
                        ""
                    } else {
                        "retained summary"
                    },
                    ..MemorySpec::new(crate::model::NamespaceId::new(1))
                })
                .await;
            let neighbor = f.insert_memory().await;
            let (snapshot, errors) =
                DecaySweepRunner::load_metadata_batch(&f.storage.read().unwrap(), &[id]);
            assert!(errors.is_empty());
            let meta = &snapshot[&id];
            let decay = DecayConfig::default();
            let engine = FsrsEngine::new(&decay);
            let threshold = if phase == DecayPhase::Ghost {
                0.05
            } else {
                0.3
            };
            let days = (1..10000)
                .find(|days| engine.retrievability(*days as f32, 1., 1.) <= threshold)
                .unwrap();
            let mut pending = Vec::new();
            let mut result = SweepResult::default();
            let evaluation = DecaySweepRunner::evaluate_record(
                id,
                phase,
                meta,
                &engine,
                &ActivationConfig::default(),
                &f.graph,
                days * 86_400_000,
                1.,
                &mut pending,
                &mut result,
            )
            .await
            .unwrap();
            assert_eq!(pending.len(), 1);
            publish_edge(&f, id, neighbor).await;
            let bonus = DecaySweepRunner::calculate_connection_bonus(
                id,
                &f.graph,
                &ActivationConfig::default(),
            )
            .await;
            assert!(
                engine.effective_retrievability(evaluation.projection.strength, bonus) > threshold
            );
            let before = f.record(id).unwrap().to_bytes();
            assert!(
                DecaySweepRunner::apply_transition(pending[0].clone(), &f.storage, &f.graph)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(f.record(id).unwrap().to_bytes(), before);
            assert_eq!(
                f.graph.read().await.get_node(&id).unwrap().decay_phase,
                phase
            );
            // The same sampled time/policy is still valid. A later neighbor
            // strength publication removes the saving bonus before final apply.
            publish_neighbor_strength(&f, neighbor, 0.).await;
            assert!(
                DecaySweepRunner::apply_transition(pending.remove(0), &f.storage, &f.graph)
                    .await
                    .unwrap()
                    .is_some()
            );
            if phase == DecayPhase::Ghost {
                assert!(f.record(id).is_none());
            } else {
                let after = f.record(id).unwrap();
                assert_eq!(after.phase, DecayPhase::Ghost);
                assert!(after.summary.is_empty());
                assert_eq!(after.decay_strength, evaluation.projection.strength);
            }
        }
    }

    #[tokio::test]
    async fn strength_only_patch_reprojects_growing_and_shrinking_bonus_at_original_clock() {
        for shrink in [false, true] {
            let f = Fixture::new();
            let id = f.insert_memory().await;
            let neighbor = f.insert_memory().await;
            if shrink {
                publish_edge(&f, id, neighbor).await;
            }
            let (snapshot, _) =
                DecaySweepRunner::load_metadata_batch(&f.storage.read().unwrap(), &[id]);
            let meta = &snapshot[&id];
            // A nondefault cap and multiplier ensure the locked calculation
            // retains the operation's policy rather than constructing defaults.
            let decay = DecayConfig {
                max_connection_bonus: 0.01,
                ..DecayConfig::default()
            };
            let engine = FsrsEngine::new(&decay);
            let activation = ActivationConfig {
                scale: 0.1,
                ..ActivationConfig::default()
            };
            let mut pending = Vec::new();
            let mut result = SweepResult::default();
            let evaluation = DecaySweepRunner::evaluate_record(
                id,
                DecayPhase::Full,
                meta,
                &engine,
                &activation,
                &f.graph,
                86_400_000,
                0.5,
                &mut pending,
                &mut result,
            )
            .await
            .unwrap();
            assert!(
                pending.is_empty(),
                "this must exercise the no-transition path"
            );
            if shrink {
                publish_neighbor_strength(&f, neighbor, 0.).await;
            } else {
                publish_edge(&f, id, neighbor).await;
            }
            let bonus =
                DecaySweepRunner::calculate_connection_bonus(id, &f.graph, &activation).await;
            let expected = super::super::lifecycle::project(
                meta.stability,
                meta.last_accessed_at,
                &evaluation.policy,
                86_400_000,
                bonus,
            )
            .unwrap();
            assert_ne!(
                expected.decay_strength,
                evaluation.projection.decay_strength
            );
            DecaySweepRunner::update_record_state(
                id,
                DecayPhase::Full,
                &evaluation,
                meta,
                &f.storage,
                &f.graph,
                &mut result,
            )
            .await;
            assert!(result.errors.is_empty());
            let after = f.record(id).unwrap();
            assert_eq!(
                (after.strength, after.decay_strength),
                (expected.strength, expected.decay_strength)
            );
            assert_eq!(after.last_accessed_at, 0);
            assert_eq!(after.phase, DecayPhase::Full);
            assert_eq!(
                f.graph.read().await.get_node(&id).unwrap().strength,
                expected.decay_strength
            );
        }
    }
}
