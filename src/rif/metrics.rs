//! Lock-free counters for RIF health monitoring.
//!
//! [`RifMetrics`] provides atomic counters for tracking RIF evaluation
//! statistics. [`RifMetricsSnapshot`] is a serializable point-in-time
//! snapshot for reporting.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Lock-free counters for RIF health monitoring.
///
/// All fields are `AtomicU64` so `RifMetrics` is `Send + Sync` but
/// NOT `Clone`. The containing `RifEngine` must be wrapped in `Arc`
/// for shared ownership across async tasks.
///
/// Fractional values (multipliers) are stored as `(value * 10_000) as u64`
/// for atomic accumulation.
#[derive(Debug, Default)]
pub struct RifMetrics {
    /// Total retrievals that triggered RIF evaluation.
    pub retrievals_evaluated: AtomicU64,
    /// Total neighbors evaluated across all retrievals.
    pub neighbors_evaluated: AtomicU64,
    /// Total neighbors whose stability was actually changed.
    pub neighbors_affected: AtomicU64,
    /// Count of neighbors that fell in the LOW regime (no effect).
    pub low_regime_count: AtomicU64,
    /// Count of neighbors that fell in the MODERATE regime (suppressed).
    pub moderate_regime_count: AtomicU64,
    /// Count of neighbors that fell in the HIGH regime (strengthened).
    pub high_regime_count: AtomicU64,
    /// Sum of all suppression multipliers x 10_000 (for averaging).
    /// E.g., a multiplier of 0.8776 is stored as 8776.
    pub total_suppression_x10k: AtomicU64,
    /// Count of suppression events (denominator for averaging).
    pub suppression_count: AtomicU64,
    /// Sum of all enhancement multipliers x 10_000.
    pub total_enhancement_x10k: AtomicU64,
    /// Count of enhancement events.
    pub enhancement_count: AtomicU64,
    /// Count of updates that were clamped by `QueryRifContext`.
    pub dedup_clamps: AtomicU64,
    /// Count of updates where stability floor was enforced.
    pub floor_enforced: AtomicU64,
}

/// A snapshot of RifMetrics for reporting.
///
/// All values are plain integers/floats -- safe to serialize and log.
#[derive(Debug, Clone, Serialize)]
pub struct RifMetricsSnapshot {
    /// Total retrievals that triggered RIF evaluation.
    pub retrievals_evaluated: u64,
    /// Total neighbors evaluated across all retrievals.
    pub neighbors_evaluated: u64,
    /// Total neighbors whose stability was actually changed.
    pub neighbors_affected: u64,
    /// Count of neighbors in the LOW regime.
    pub low_regime_count: u64,
    /// Count of neighbors in the MODERATE regime.
    pub moderate_regime_count: u64,
    /// Count of neighbors in the HIGH regime.
    pub high_regime_count: u64,
    /// Average suppression multiplier, if any suppression events occurred.
    pub avg_suppression_multiplier: Option<f64>,
    /// Average enhancement multiplier, if any enhancement events occurred.
    pub avg_enhancement_multiplier: Option<f64>,
    /// Count of updates clamped by per-query dedup.
    pub dedup_clamps: u64,
    /// Count of updates where stability floor was enforced.
    pub floor_enforced: u64,
}

impl RifMetrics {
    /// Record the evaluation of one retrieval's neighborhood.
    ///
    /// Called once per retrieved memory after `compute_effects` finishes.
    pub fn record_evaluation(&self, neighbors_checked: usize, updates: &[super::StabilityUpdate]) {
        self.retrievals_evaluated.fetch_add(1, Ordering::Relaxed);
        self.neighbors_evaluated
            .fetch_add(neighbors_checked as u64, Ordering::Relaxed);
        self.neighbors_affected
            .fetch_add(updates.len() as u64, Ordering::Relaxed);

        for update in updates {
            match update.regime {
                super::ActivationRegime::Low => {
                    self.low_regime_count.fetch_add(1, Ordering::Relaxed);
                }
                super::ActivationRegime::Moderate => {
                    self.moderate_regime_count.fetch_add(1, Ordering::Relaxed);
                    let scaled = (update.multiplier as f64 * 10_000.0) as u64;
                    self.total_suppression_x10k
                        .fetch_add(scaled, Ordering::Relaxed);
                    self.suppression_count.fetch_add(1, Ordering::Relaxed);
                }
                super::ActivationRegime::High => {
                    self.high_regime_count.fetch_add(1, Ordering::Relaxed);
                    let scaled = (update.multiplier as f64 * 10_000.0) as u64;
                    self.total_enhancement_x10k
                        .fetch_add(scaled, Ordering::Relaxed);
                    self.enhancement_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Record that `QueryRifContext::clamp_multiplier` modified a multiplier.
    pub fn record_dedup_clamp(&self) {
        self.dedup_clamps.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that the stability floor was enforced during apply.
    pub fn record_floor_enforced(&self) {
        self.floor_enforced.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a point-in-time snapshot for reporting.
    ///
    /// Non-atomic across fields (acceptable for monitoring -- not transactional).
    pub fn snapshot(&self) -> RifMetricsSnapshot {
        let suppression_n = self.suppression_count.load(Ordering::Relaxed);
        let enhancement_n = self.enhancement_count.load(Ordering::Relaxed);

        let avg_suppression = if suppression_n > 0 {
            let sum = self.total_suppression_x10k.load(Ordering::Relaxed);
            Some(sum as f64 / suppression_n as f64 / 10_000.0)
        } else {
            None
        };

        let avg_enhancement = if enhancement_n > 0 {
            let sum = self.total_enhancement_x10k.load(Ordering::Relaxed);
            Some(sum as f64 / enhancement_n as f64 / 10_000.0)
        } else {
            None
        };

        RifMetricsSnapshot {
            retrievals_evaluated: self.retrievals_evaluated.load(Ordering::Relaxed),
            neighbors_evaluated: self.neighbors_evaluated.load(Ordering::Relaxed),
            neighbors_affected: self.neighbors_affected.load(Ordering::Relaxed),
            low_regime_count: self.low_regime_count.load(Ordering::Relaxed),
            moderate_regime_count: self.moderate_regime_count.load(Ordering::Relaxed),
            high_regime_count: self.high_regime_count.load(Ordering::Relaxed),
            avg_suppression_multiplier: avg_suppression,
            avg_enhancement_multiplier: avg_enhancement,
            dedup_clamps: self.dedup_clamps.load(Ordering::Relaxed),
            floor_enforced: self.floor_enforced.load(Ordering::Relaxed),
        }
    }
}
