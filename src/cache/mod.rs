//! RAM cache for Recalld memory records.
//!
//! The cache manager wraps a `moka::future::Cache` with weight-based
//! capacity, centrality-adjusted eviction, and an eviction listener
//! that keeps auxiliary structures (vector buffer, reverse index) in
//! sync. All operations are async and safe to call from Tokio tasks.
//!
//! ## Modules
//!
//! - `manager` -- `CacheManager` struct and core operations (CS-12).
//! - `weight`  -- `CachedRecord` weight calculation, centrality discount.
//! - `warming` -- `warm.bin` I/O, startup warming, prefetch worker (CS-13).
//! - `pressure` -- Memory pressure monitoring and response (CS-13).

pub mod manager;
pub mod pressure;
pub mod warming;
pub mod warming_adapters;
pub mod weight;

// ── Re-exports ──────────────────────────────────────────────────────

pub use manager::{CacheConfig, CacheManager, CacheStats, EvictionCause, EvictionEvent};
pub use pressure::{
    NoOpPressureMonitor, PressureLevel, PressureMonitor, create_pressure_monitor, evict_to_target,
    start_pressure_monitor,
};
pub use warming::{
    PrefetchMetrics, PrefetchRequest, WarmEntry, WarmHeader, WarmLoadResult, WarmSnapshot,
    compute_priority, enqueue_neighbors_for_prefetch, load_warm_file, prefetch_worker, warm_cache,
    write_warm_file,
};
pub use weight::{calculate_weight, moka_weigher, weigh_cached_record};

// ── Error Type ──────────────────────────────────────────────────────

use crate::model::MemoryId;

/// Errors that can occur during cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The requested memory was not found in cache or on disk.
    #[error("memory not found: {0}")]
    NotFound(MemoryId),

    /// A storage backend error occurred during cache loading.
    #[error("storage error during cache load: {0}")]
    StorageError(#[from] anyhow::Error),

    /// The cache is over capacity (informational, moka handles this
    /// automatically via eviction).
    #[error("cache capacity exceeded: weighted_size={current}, budget={budget}")]
    CapacityExceeded {
        /// Current weighted size in bytes.
        current: u64,
        /// Configured budget in bytes.
        budget: u64,
    },
}

/// Convenience alias used throughout this module.
pub type Result<T> = std::result::Result<T, CacheError>;
