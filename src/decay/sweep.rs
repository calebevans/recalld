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
//! serialized at the storage transaction level.

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
    /// FSRS stability S in days.
    pub stability: f32,
    /// Milliseconds since Unix epoch.
    pub last_accessed_at: i64,
    /// Whether the memory is exempt from decay sweeps.
    pub is_permastore: bool,
    /// Namespace-specific decay rate multiplier, if set.
    /// None means inherit from global config.
    pub decay_rate_multiplier: Option<f32>,
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

    /// Dead-space ratio in text.log that triggers compaction.
    /// When the fraction of dead (reclaimed) space exceeds this
    /// threshold, text.log is compacted at the end of the sweep.
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
    /// Whether text.log compaction was triggered.
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
#[derive(Debug)]
struct PendingTransition {
    memory_id: MemoryId,
    from_phase: DecayPhase,
    effective_r: f32,
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
            config,
            decay_config,
            activation_config,
            storage,
            graph,
            cache,
            global_decay_multiplier,
            shutdown_tx,
            trigger_notify: Arc::new(Notify::new()),
        })
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
                let result = Self::execute_sweep(
                    &config,
                    &decay_config,
                    &activation_config,
                    &storage,
                    &graph,
                    &cache,
                    global_decay_multiplier,
                )
                .await;
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

                let result = Self::execute_sweep(
                    &config,
                    &decay_config,
                    &activation_config,
                    &storage,
                    &graph,
                    &cache,
                    global_decay_multiplier,
                )
                .await;
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
    async fn execute_sweep(
        config: &SweepConfig,
        decay_config: &DecayConfig,
        activation_config: &ActivationConfig,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: &SharedGraph,
        cache: &Arc<CacheManager>,
        global_decay_multiplier: f64,
    ) -> SweepResult {
        let start = Instant::now();
        let now_millis = chrono::Utc::now().timestamp_millis();
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
        // text_dead_ratio is not directly available from the storage
        // trait; compaction can be triggered via compact_text_log.
        {
            let mut storage_w = storage.write().unwrap_or_else(|e| {
                error!("storage lock poisoned during compaction check: {e}");
                e.into_inner()
            });
            match storage_w.compact_text_log() {
                Ok(cr) => {
                    if cr.entries_removed > 0 {
                        result.compaction_triggered = true;
                    }
                }
                Err(e) => {
                    error!(error = %e, "text.log compaction failed");
                }
            }
        }

        // -- Persist updated phase bitmaps ------------------------
        {
            let storage_r = storage.read().unwrap_or_else(|e| {
                error!("storage lock poisoned during phase index persist: {e}");
                e.into_inner()
            });
            if let Err(e) = storage_r.persist_phase_index() {
                error!(error = %e, "failed to persist phase bitmap index");
            }
        }

        result.duration = start.elapsed();
        result
    }

    /// Process all memories in a single phase.
    ///
    /// Loads memory IDs from the PhaseIndex (RoaringBitmap) for the given
    /// phase, then iterates them. For each memory:
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
        let memory_ids = {
            let storage_r = storage.read().unwrap_or_else(|e| {
                error!("storage lock poisoned: {e}");
                e.into_inner()
            });
            match storage_r.ids_in_phase(phase) {
                Ok(ids) => ids,
                Err(e) => {
                    warn!(
                        phase = ?phase,
                        error = %e,
                        "PhaseIndex lookup failed, falling back to full table scan"
                    );
                    // Fallback: scan meta.db filtering by decay_phase field.
                    match storage_r.scan_phase_records(phase) {
                        Ok(records) => records
                            .into_iter()
                            .map(|(id, _): (MemoryId, _)| id)
                            .collect(),
                        Err(e2) => {
                            error!(
                                phase = ?phase,
                                error = %e2,
                                "full table scan also failed, skipping phase"
                            );
                            return;
                        }
                    }
                }
            }
        };

        debug!(phase = ?phase, count = memory_ids.len(), "scanning phase");

        let mut pending_transitions: Vec<PendingTransition> = Vec::new();

        for (i, memory_id) in memory_ids.iter().enumerate() {
            // Cooperative yielding: give query tasks a chance to run
            if i > 0 && i % config.yield_every_n == 0 {
                tokio::task::yield_now().await;
            }

            result.memories_scanned += 1;

            // Load decay-relevant metadata for this memory via storage adapter.
            let meta = {
                let storage_r = storage.read().unwrap_or_else(|e| {
                    error!("storage lock poisoned: {e}");
                    e.into_inner()
                });
                match super::storage_adapter::get_decay_metadata(&*storage_r, *memory_id) {
                    Ok(Some(m)) => m,
                    Ok(None) => {
                        // Memory was deleted between bitmap load and now.
                        debug!(id = %memory_id, "memory not found, skipping");
                        continue;
                    }
                    Err(e) => {
                        result.errors.push(SweepRecordError {
                            memory_id: *memory_id,
                            phase,
                            error: format!("failed to load metadata: {e}"),
                        });
                        continue;
                    }
                }
            };

            // -- Permastore exemption -----------------------------
            if meta.is_permastore {
                result.permastore_skipped += 1;
                continue;
            }

            // -- Get effective decay rate multiplier ---------------
            // Namespace override > global default
            let multiplier = meta
                .decay_rate_multiplier
                .map(|m| m as f64)
                .unwrap_or(global_decay_multiplier);

            // -- Decay disabled check -----------------------------
            if multiplier == 0.0 {
                result.decay_disabled_skipped += 1;
                continue;
            }

            // -- Calculate raw retrievability ---------------------
            let elapsed_millis = (now_millis - meta.last_accessed_at).max(0) as f64;
            let elapsed_days = (elapsed_millis / 86_400_000.0) as f32;
            let raw_r =
                engine.retrievability(elapsed_days, meta.stability, multiplier as f32);

            // -- Calculate connection bonus -----------------------
            let connection_bonus =
                Self::calculate_connection_bonus(*memory_id, graph, activation_config).await;
            let effective_r = engine.effective_retrievability(raw_r, connection_bonus);

            // -- Determine transition -----------------------------
            let threshold = match phase {
                DecayPhase::Ghost => engine.config.phase_3_threshold,
                DecayPhase::Summary => engine.config.phase_2_threshold,
                DecayPhase::Full => engine.config.phase_1_threshold,
            };

            if effective_r <= threshold {
                pending_transitions.push(PendingTransition {
                    memory_id: *memory_id,
                    from_phase: phase,
                    effective_r,
                });
            } else if raw_r <= threshold && effective_r > threshold {
                // Raw R crossed the threshold, but connection bonus
                // pulled effective R back above it.
                result.saved_by_connection_bonus += 1;
            }

            // -- Batch flush --------------------------------------
            if pending_transitions.len() >= config.write_batch_size {
                Self::flush_transitions(&pending_transitions, storage, graph, cache, result).await;
                pending_transitions.clear();
            }

            // Update decay_strength in metadata regardless of transition.
            {
                let storage_r = storage.read().unwrap_or_else(|e| {
                    error!("storage lock poisoned: {e}");
                    e.into_inner()
                });
                if let Err(e) = storage_r.update_decay_state(
                    *memory_id,
                    phase,
                    effective_r,
                    meta.stability,
                    meta.is_permastore,
                ) {
                    result.errors.push(SweepRecordError {
                        memory_id: *memory_id,
                        phase,
                        error: format!("failed to update decay_strength: {e}"),
                    });
                }
            }

            // Sync graph node state so next phase's connection bonus
            // calculations use current values.
            {
                let mut graph_w = graph.write().await;
                let _ = graph_w.update_node_state(*memory_id, phase, effective_r);
            }
        }

        // Flush remaining transitions
        if !pending_transitions.is_empty() {
            Self::flush_transitions(&pending_transitions, storage, graph, cache, result).await;
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
            let outcome = match t.from_phase {
                DecayPhase::Full => {
                    Self::transition_full_to_summary(t.memory_id, t.effective_r, storage).await
                }
                DecayPhase::Summary => {
                    Self::transition_summary_to_ghost(t.memory_id, t.effective_r, storage).await
                }
                DecayPhase::Ghost => {
                    Self::transition_ghost_to_deleted(t.memory_id, t.effective_r, storage, graph)
                        .await
                }
            };

            match outcome {
                Ok(transition) => {
                    // Update stats
                    match t.from_phase {
                        DecayPhase::Full => result.full_to_summary += 1,
                        DecayPhase::Summary => result.summary_to_ghost += 1,
                        DecayPhase::Ghost => result.deletions += 1,
                    }

                    // Invalidate cache entry
                    cache.invalidate(t.memory_id).await;

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
    /// Phase 1 -> Phase 2: Full -> Summary.
    ///
    /// 1. Verify summary exists (it should -- summaries are created at
    ///    memory creation time). If missing, log a warning; the memory
    ///    transitions anyway but will have no summary text.
    /// 2. Delete full_text from text.log (zero the pointer in meta.db,
    ///    mark the space as dead -- no physical deletion until compaction).
    /// 3. Update decay_phase = Summary in meta.db.
    /// 4. Update PhaseIndex bitmap (clear Full bit, set Summary bit).
    async fn transition_full_to_summary(
        memory_id: MemoryId,
        effective_r: f32,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    ) -> Result<PhaseTransition, SweepError> {
        let storage_r = storage
            .read()
            .map_err(|e| SweepError::Storage(format!("storage lock poisoned: {e}")))?;

        // Safety net: verify summary exists before deleting full_text.
        match super::storage_adapter::has_summary(&*storage_r, memory_id) {
            Ok(false) => {
                warn!(
                    id = %memory_id,
                    "memory has no summary at Full->Summary transition; \
                     proceeding anyway -- summary was expected at creation time"
                );
            }
            Err(e) => {
                warn!(
                    id = %memory_id,
                    error = %e,
                    "failed to check summary existence, proceeding with transition"
                );
            }
            Ok(true) => {}
        }

        // Load stability from metadata to preserve it during phase change.
        let meta = super::storage_adapter::get_decay_metadata(&*storage_r, memory_id)?;
        let (stability, is_permastore) = match meta {
            Some(m) => (m.stability, m.is_permastore),
            None => return Err(SweepError::MemoryNotFound(memory_id)),
        };

        // Update phase in meta.db + PhaseIndex bitmap.
        storage_r
            .update_decay_state(
                memory_id,
                DecayPhase::Summary,
                effective_r,
                stability,
                is_permastore,
            )
            .map_err(|e| SweepError::Storage(e.to_string()))?;

        Ok(PhaseTransition::Summarized {
            id: memory_id,
            old_phase: DecayPhase::Full,
            new_phase: DecayPhase::Summary,
            retrievability: effective_r,
        })
    }

    /// Phase 2 -> Phase 3: Summary -> Ghost.
    ///
    /// 1. Delete summary text from the record.
    /// 2. Update decay_phase = Ghost in meta.db.
    /// 3. Update PhaseIndex bitmap.
    ///
    /// After this, only the embedding and relationship edges remain.
    async fn transition_summary_to_ghost(
        memory_id: MemoryId,
        effective_r: f32,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
    ) -> Result<PhaseTransition, SweepError> {
        let storage_r = storage
            .read()
            .map_err(|e| SweepError::Storage(format!("storage lock poisoned: {e}")))?;

        // Load stability to preserve it during phase change.
        let meta = super::storage_adapter::get_decay_metadata(&*storage_r, memory_id)?;
        let (stability, is_permastore) = match meta {
            Some(m) => (m.stability, m.is_permastore),
            None => return Err(SweepError::MemoryNotFound(memory_id)),
        };

        storage_r
            .update_decay_state(
                memory_id,
                DecayPhase::Ghost,
                effective_r,
                stability,
                is_permastore,
            )
            .map_err(|e| SweepError::Storage(e.to_string()))?;

        Ok(PhaseTransition::Ghosted {
            id: memory_id,
            old_phase: DecayPhase::Summary,
            new_phase: DecayPhase::Ghost,
            retrievability: effective_r,
        })
    }

    /// Phase 3 -> Deleted: Ghost -> complete removal.
    ///
    /// Removes the memory from all storage in a specific order:
    /// 1. Zero out embedding slot in vectors.dat (add to free list).
    /// 2. Remove edges via graph's bridging-aware deletion.
    /// 3. Remove from all secondary indexes (tags, namespace, PhaseIndex).
    /// 4. Remove metadata record from meta.db.
    ///
    /// The order matters: edges and indexes are cleaned up BEFORE the
    /// metadata record is removed, so that any concurrent query that
    /// finds the metadata can still resolve edges. If the metadata were
    /// removed first, a concurrent edge scan could find dangling references.
    async fn transition_ghost_to_deleted(
        memory_id: MemoryId,
        effective_r: f32,
        storage: &Arc<std::sync::RwLock<RedbStorageEngine>>,
        graph: &SharedGraph,
    ) -> Result<PhaseTransition, SweepError> {
        // 1. Remove all edges via the graph's bridging-aware deletion.
        //    Acquires a WRITE lock on the graph.
        {
            let mut graph_w = graph.write().await;
            let removal_result = graph_w.remove_memory_with_bridging(memory_id);
            debug!(
                id = %memory_id,
                edges_removed = removal_result.removed_edges.len(),
                "removed edges with bridging"
            );
        } // write lock released

        // 2. Remove edge records from storage.
        {
            let storage_r = storage
                .read()
                .map_err(|e| SweepError::Storage(format!("storage lock poisoned: {e}")))?;
            let _ = storage_r
                .remove_all_edges(memory_id)
                .map_err(|e| SweepError::Storage(e.to_string()))?;
        }

        // 3. Remove metadata record (must acquire write lock for delete_memory).
        {
            let mut storage_w = storage
                .write()
                .map_err(|e| SweepError::Storage(format!("storage lock poisoned: {e}")))?;
            storage_w
                .delete_memory(memory_id)
                .map_err(|e| SweepError::Storage(e.to_string()))?;
        }

        Ok(PhaseTransition::Deleted {
            id: memory_id,
            old_phase: DecayPhase::Ghost,
            retrievability: effective_r,
        })
    }
}
