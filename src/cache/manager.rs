//! Cache manager: moka-backed RAM cache with weight-based eviction.
//!
//! Provides the `CacheManager` struct wrapping a `moka::future::Cache` with
//! centrality-adjusted weight, eviction notifications, and RIF cache coherency.

use std::sync::Arc;

use moka::future::Cache;
use moka::notification::RemovalCause;

use crate::cache::weight;
use crate::cache::CacheError;
use crate::model::{CachedRecord, DecayPhase, MemoryId};

/// Type alias for the moka record cache.
type RecordCache = Cache<MemoryId, Arc<CachedRecord>>;

/// Configuration for the RAM cache layer.
///
/// Constructed from the application configuration file or CLI flags.
/// Passed to `CacheManager::new()`.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum capacity of the moka record cache in bytes.
    ///
    /// Set to 0 to let the system auto-calculate from available RAM.
    /// Default: 0 (auto).
    pub max_capacity_bytes: u64,

    /// Optional time-to-idle duration.
    ///
    /// Default: `None`.
    pub time_to_idle: Option<std::time::Duration>,

    /// Optional time-to-live duration.
    ///
    /// Default: `None`.
    pub time_to_live: Option<std::time::Duration>,

    /// Embedding dimensionality.
    pub embedding_dim: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_capacity_bytes: 0, // auto-calculate
            time_to_idle: None,
            time_to_live: None,
            embedding_dim: 1536,
        }
    }
}

/// An eviction event emitted to external listeners.
#[derive(Debug, Clone)]
pub struct EvictionEvent {
    /// The evicted memory's ID.
    pub id: MemoryId,
    /// Why the entry was evicted.
    pub cause: EvictionCause,
}

/// Simplified eviction cause (maps from moka's RemovalCause).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionCause {
    /// Capacity-based eviction (TinyLFU selected this entry).
    Size,
    /// Explicit invalidation by caller.
    Explicit,
    /// Entry was replaced by a new value for the same key.
    Replaced,
    /// TTL or TTI expiration (not expected in normal operation).
    Expired,
}

impl From<RemovalCause> for EvictionCause {
    fn from(cause: RemovalCause) -> Self {
        match cause {
            RemovalCause::Size => EvictionCause::Size,
            RemovalCause::Explicit => EvictionCause::Explicit,
            RemovalCause::Replaced => EvictionCause::Replaced,
            RemovalCause::Expired => EvictionCause::Expired,
        }
    }
}

/// Snapshot of cache state for monitoring and diagnostics.
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Number of entries in the record cache.
    pub entry_count: u64,
    /// Total weighted size of all entries in bytes (centrality-adjusted).
    pub weighted_size_bytes: u64,
    /// Configured maximum capacity in bytes.
    pub configured_capacity_bytes: u64,
    /// Cache utilization: `weighted_size / configured_capacity`.
    pub utilization: f64,
}

/// The central cache management component.
///
/// Owns the moka record cache and provides the public API for
/// cache operations. The vector buffer, reverse neighborhood index,
/// prefetch system, and memory pressure monitor are managed by
/// CS-13 (Cache Warming & Memory Pressure) and composed with
/// CacheManager at the system assembly level (CS-20).
///
/// CacheManager is cheaply `Clone`able (moka::Cache uses internal
/// Arc). Pass handles freely across tasks and threads.
#[derive(Clone)]
pub struct CacheManager {
    /// Primary record cache (moka future::Cache).
    cache: RecordCache,

    /// The configured capacity in bytes (immutable after construction).
    configured_capacity: u64,
}

impl CacheManager {
    /// Create a new CacheManager with the given configuration.
    ///
    /// The moka cache is constructed with weight-based capacity,
    /// centrality-adjusted weigher, and an eviction listener for
    /// metrics and optional external notification.
    pub fn new(
        config: CacheConfig,
        eviction_tx: Option<tokio::sync::mpsc::UnboundedSender<EvictionEvent>>,
    ) -> Self {
        let capacity = if config.max_capacity_bytes > 0 {
            config.max_capacity_bytes
        } else {
            Self::auto_capacity()
        };

        let mut builder = Cache::builder()
            .name("recalld-records")
            .max_capacity(capacity)
            .weigher(weight::moka_weigher);

        // Optional TTI/TTL (not normally configured).
        if let Some(tti) = config.time_to_idle {
            builder = builder.time_to_idle(tti);
        }
        if let Some(ttl) = config.time_to_live {
            builder = builder.time_to_live(ttl);
        }

        // Eviction listener: logs, metrics, and optional external
        // notification channel.
        let tx = eviction_tx;
        builder = builder.eviction_listener(
            move |key: Arc<MemoryId>, _value: Arc<CachedRecord>, cause: RemovalCause| {
                Self::on_eviction(&key, cause, &tx);
            },
        );

        let cache = builder.build();

        Self {
            cache,
            configured_capacity: capacity,
        }
    }

    /// Auto-calculate cache capacity from system RAM.
    ///
    /// Uses 10% of total RAM, clamped to [64 MB, 8 GB].
    /// The record cache portion is ~10% of total budget at 1536-dim.
    fn auto_capacity() -> u64 {
        let total_ram = Self::total_system_memory();

        let ram_fraction = 0.10;
        let raw_budget = (total_ram as f64 * ram_fraction) as u64;
        let total_budget = raw_budget.clamp(64 * 1024 * 1024, 8 * 1024 * 1024 * 1024);

        // Record cache is ~10% of total budget at 1536-dim.
        let record_fraction = 700.0 / (700.0 + (1536.0 * 4.0));
        (total_budget as f64 * record_fraction) as u64
    }

    #[cfg(target_os = "macos")]
    fn total_system_memory() -> u64 {
        unsafe {
            let mut size: u64 = 0;
            let mut len = std::mem::size_of::<u64>();
            let name = b"hw.memsize\0";
            libc::sysctlbyname(
                name.as_ptr() as *const _,
                &mut size as *mut u64 as *mut _,
                &mut len,
                std::ptr::null_mut(),
                0,
            );
            if size == 0 { 16 * 1024 * 1024 * 1024 } else { size }
        }
    }

    #[cfg(target_os = "linux")]
    fn total_system_memory() -> u64 {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
            .unwrap_or(16 * 1024 * 1024 * 1024)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn total_system_memory() -> u64 {
        16 * 1024 * 1024 * 1024
    }

    /// Internal eviction listener. Called by moka during maintenance.
    ///
    /// This function MUST NOT panic. moka catches panics via
    /// `catch_unwind` and permanently disables the listener.
    fn on_eviction(
        key: &MemoryId,
        cause: RemovalCause,
        tx: &Option<tokio::sync::mpsc::UnboundedSender<EvictionEvent>>,
    ) {
        // Metrics.
        match cause {
            RemovalCause::Size => {
                metrics::counter!("cache.evictions.size").increment(1);
                tracing::debug!(memory_id = ?key, "cache eviction: size pressure");
            }
            RemovalCause::Explicit => {
                metrics::counter!("cache.evictions.explicit").increment(1);
                tracing::debug!(memory_id = ?key, "cache eviction: explicit invalidation");
            }
            RemovalCause::Replaced => {
                metrics::counter!("cache.evictions.replaced").increment(1);
                // No debug log for replacements -- too noisy during
                // RIF batch updates.
            }
            RemovalCause::Expired => {
                metrics::counter!("cache.evictions.expired").increment(1);
                tracing::warn!(
                    memory_id = ?key,
                    "unexpected TTL/TTI expiration -- TTL/TTI should not be configured"
                );
            }
        }

        // Notify external listeners (vector buffer, reverse index).
        if let Some(tx) = tx {
            // Best-effort: if the receiver is dropped, silently discard.
            let _ = tx.send(EvictionEvent {
                id: *key,
                cause: cause.into(),
            });
        }
    }

    /// Retrieve a cached record by ID.
    ///
    /// Returns `Some(Arc<CachedRecord>)` on cache hit, `None` on miss.
    pub async fn get(&self, id: MemoryId) -> Option<Arc<CachedRecord>> {
        self.cache.get(&id).await
    }

    /// Insert a record into the cache.
    pub async fn insert(&self, id: MemoryId, record: CachedRecord) {
        self.cache.insert(id, Arc::new(record)).await;
    }

    /// Insert a pre-wrapped Arc<CachedRecord>.
    pub async fn insert_arc(&self, id: MemoryId, record: Arc<CachedRecord>) {
        self.cache.insert(id, record).await;
    }

    /// Invalidate (remove) a single entry from the cache.
    pub async fn invalidate(&self, id: MemoryId) {
        self.cache.invalidate(&id).await;
    }

    /// Invalidate multiple entries from the cache in batch.
    ///
    /// Defers `run_pending_tasks()` to the end, amortizing moka's
    /// internal maintenance across all invalidations.
    pub async fn batch_invalidate(&self, ids: &[MemoryId]) {
        for id in ids {
            self.cache.invalidate(id).await;
        }
        self.cache.run_pending_tasks().await;
    }

    /// Check whether an entry exists in the cache without affecting
    /// its access frequency or recency tracking.
    pub fn contains(&self, id: &MemoryId) -> bool {
        self.cache.contains_key(id)
    }

    /// Return the approximate number of entries in the cache.
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Return the approximate total weighted size of all entries in bytes.
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Force moka to process all pending maintenance tasks.
    pub async fn run_pending_tasks(&self) {
        self.cache.run_pending_tasks().await;
    }

    /// Get a record by ID with read-through semantics.
    ///
    /// On cache hit: returns the cached `Arc<CachedRecord>`.
    /// On cache miss: calls the provided `loader` to fetch from disk,
    /// inserts the result into the cache, and returns it.
    ///
    /// **Coalescing**: moka's `try_get_with` ensures only ONE loader
    /// invocation occurs for concurrent requests for the same key.
    pub async fn get_or_load<F, Fut>(
        &self,
        id: MemoryId,
        loader: F,
    ) -> super::Result<Option<Arc<CachedRecord>>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<
            Output = std::result::Result<Option<CachedRecord>, anyhow::Error>,
        >,
    {
        // Fast path: direct cache lookup.
        if let Some(record) = self.cache.get(&id).await {
            metrics::counter!("cache.hits").increment(1);
            return Ok(Some(record));
        }

        metrics::counter!("cache.misses").increment(1);

        // Slow path: load from disk.
        match loader().await {
            Ok(Some(record)) => {
                let arc_record = Arc::new(record);
                self.cache.insert(id, Arc::clone(&arc_record)).await;
                Ok(Some(arc_record))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(CacheError::StorageError(e)),
        }
    }

    /// Update a cached record's stability and strength after RIF processing.
    ///
    /// If the record is not cached, this is a no-op.
    pub async fn update_rif_state(
        &self,
        id: MemoryId,
        new_stability: f32,
        new_strength: f32,
    ) {
        if let Some(existing) = self.cache.get(&id).await {
            let mut updated = (*existing).clone();
            updated.stability = new_stability;
            updated.strength = new_strength;
            self.cache.insert(id, Arc::new(updated)).await;
        }
    }

    /// Batch-update cached records after RIF processing.
    ///
    /// Defers `run_pending_tasks()` to the end, amortizing moka's
    /// internal maintenance overhead across ~25 updates.
    pub async fn batch_update_rif_state(&self, updates: &[(MemoryId, f32, f32)]) {
        for &(id, new_stability, new_strength) in updates {
            self.update_rif_state(id, new_stability, new_strength).await;
        }
        self.cache.run_pending_tasks().await;
        metrics::counter!("cache.rif_batch_updates").increment(updates.len() as u64);
    }

    /// Update a cached record's decay state after a decay sweep.
    ///
    /// If `deleted` is true, the record is invalidated entirely.
    pub async fn update_decay_state(
        &self,
        id: MemoryId,
        new_strength: f32,
        new_phase: DecayPhase,
        deleted: bool,
    ) {
        if deleted {
            self.cache.invalidate(&id).await;
            return;
        }

        if let Some(existing) = self.cache.get(&id).await {
            let mut updated = (*existing).clone();
            updated.strength = new_strength;
            updated.phase = new_phase;
            self.cache.insert(id, Arc::new(updated)).await;
        }
    }

    /// Update a cached record's edge count and re-insert to refresh
    /// the centrality-adjusted weight.
    pub async fn update_edge_count(&self, id: MemoryId, new_edge_count: u16) {
        if let Some(existing) = self.cache.get(&id).await {
            let mut updated = (*existing).clone();
            updated.edge_count = new_edge_count;
            self.cache.insert(id, Arc::new(updated)).await;
        }
    }

    /// Return a snapshot of cache statistics.
    pub async fn stats(&self) -> CacheStats {
        self.cache.run_pending_tasks().await;
        let entry_count = self.cache.entry_count();
        let weighted_size = self.cache.weighted_size();
        CacheStats {
            entry_count,
            weighted_size_bytes: weighted_size,
            configured_capacity_bytes: self.configured_capacity,
            utilization: if self.configured_capacity > 0 {
                weighted_size as f64 / self.configured_capacity as f64
            } else {
                0.0
            },
        }
    }

    /// Expose a reference to the underlying moka cache for advanced operations.
    pub fn inner(&self) -> &RecordCache {
        &self.cache
    }

    /// Iterate over all cached entries.
    ///
    /// moka's iterators are lock-free and do not block concurrent operations.
    pub fn iter(&self) -> impl Iterator<Item = (MemoryId, Arc<CachedRecord>)> + '_ {
        self.cache.iter().map(|(k, v)| (*k, v))
    }
}
