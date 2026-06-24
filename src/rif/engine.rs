//! RIF engine -- orchestration of activation, plasticity, and metrics.
//!
//! [`RifEngine`] is the main entry point for computing RIF effects.
//! [`QueryRifContext`] provides per-query deduplication to prevent
//! over-suppression when a query returns multiple results.

use std::collections::HashMap;

use crate::model::{EdgeType, MemoryId};
use crate::rif::activation::calculate_activation;
use crate::rif::config::RifConfig;
use crate::rif::metrics::RifMetrics;
use crate::rif::plasticity::{ActivationRegime, classify_regime, plasticity_multiplier};

/// A pending stability update produced by the RIF engine.
///
/// The engine computes these but does NOT apply them -- the caller
/// writes them to storage, enabling batching and testability.
#[derive(Debug, Clone)]
pub struct StabilityUpdate {
    /// The memory being affected (the neighbor).
    pub memory_id: MemoryId,
    /// The memory that was retrieved (cause of this effect).
    pub triggered_by: MemoryId,
    /// Old stability value before this update.
    pub old_stability: f32,
    /// New stability value after this update.
    pub new_stability: f32,
    /// The activation score that produced this update.
    pub activation: f32,
    /// The raw stability multiplier (before any per-query clamping).
    pub multiplier: f32,
    /// The regime this activation fell into.
    pub regime: ActivationRegime,
}

/// Pre-gathered data about one neighbor, passed in by the caller.
///
/// This decouples the RIF engine from the graph and storage APIs.
/// The query engine's integration layer gathers this data from the
/// graph traversal and RAM cache, then hands it to `compute_effects`.
#[derive(Debug, Clone)]
pub struct NeighborInfo {
    /// The neighbor's memory ID.
    pub memory_id: MemoryId,
    /// Edge weight connecting the retrieved memory to this neighbor.
    /// Range: [0.0, 1.0].
    pub edge_weight: f32,
    /// Edge type of the connecting edge.
    pub edge_type: EdgeType,
    /// Graph distance in hops (1 = direct neighbor).
    pub graph_distance: u32,
    /// Neighbor's current FSRS retrievability (the `strength` field).
    pub retrievability: f32,
    /// Neighbor's current FSRS stability.
    pub stability: f32,
    /// Cosine similarity between the retrieved memory's embedding and
    /// this neighbor's embedding. `None` when the neighbor's embedding
    /// is not loaded in the RAM cache.
    pub similarity: Option<f32>,
}

/// The Retrieval-Induced Forgetting engine.
///
/// Computes stability updates when memories are retrieved.
/// Designed to run asynchronously after retrieval returns.
///
/// **Must be wrapped in `Arc`**: `RifMetrics` contains `AtomicU64`
/// fields which are `!Clone`. Construct with
/// `Arc::new(RifEngine::new(config))` and clone the `Arc` for sharing
/// across async tasks.
pub struct RifEngine {
    config: RifConfig,
    metrics: RifMetrics,
}

impl RifEngine {
    /// Create a new RIF engine with the given configuration.
    pub fn new(config: RifConfig) -> Self {
        Self {
            config,
            metrics: RifMetrics::default(),
        }
    }

    /// Read-only access to the active config.
    pub fn config(&self) -> &RifConfig {
        &self.config
    }

    /// Read-only access to metrics for reporting.
    pub fn metrics(&self) -> &RifMetrics {
        &self.metrics
    }

    /// Compute RIF effects for one retrieved memory.
    ///
    /// Given a retrieved memory's ID and its pre-gathered neighbors,
    /// returns a list of `StabilityUpdate`s to apply.
    ///
    /// **Does NOT apply updates** -- the caller writes them to storage.
    /// This separation enables batching across multiple results and
    /// makes the function pure (testable without storage).
    ///
    /// # Behavior
    ///
    /// 1. Return empty `Vec` if `config.enabled == false`.
    /// 2. Compute activation for each neighbor via `calculate_activation`.
    /// 3. Sort by activation descending; cap at `config.max_neighbors`.
    /// 4. Map each activation through `plasticity_multiplier`.
    /// 5. Skip no-ops (multiplier == 1.0).
    /// 6. Build `StabilityUpdate` with `new_stability = old_stability * multiplier`,
    ///    floored at `config.stability_floor`.
    /// 7. Record metrics.
    pub fn compute_effects(
        &self,
        retrieved_id: MemoryId,
        neighbors: &[NeighborInfo],
    ) -> Vec<StabilityUpdate> {
        if !self.config.enabled {
            return Vec::new();
        }

        // Score all neighbors.
        let mut scored: Vec<(usize, f32)> = neighbors
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let activation = calculate_activation(
                    n.similarity,
                    n.edge_weight,
                    n.edge_type,
                    n.graph_distance,
                    n.retrievability,
                    &self.config,
                );
                (i, activation)
            })
            .collect();

        // Sort by activation descending so the max_neighbors cap
        // drops the least-activated (least relevant) neighbors.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let evaluated = scored.len().min(self.config.max_neighbors);
        let mut updates = Vec::new();

        for &(idx, activation) in scored.iter().take(self.config.max_neighbors) {
            let neighbor = &neighbors[idx];
            let multiplier = plasticity_multiplier(activation, &self.config);

            // Skip no-ops.
            if (multiplier - 1.0).abs() < f32::EPSILON {
                continue;
            }

            let regime = classify_regime(activation, &self.config);

            let new_stability = (neighbor.stability * multiplier).max(self.config.stability_floor);

            updates.push(StabilityUpdate {
                memory_id: neighbor.memory_id,
                triggered_by: retrieved_id,
                old_stability: neighbor.stability,
                new_stability,
                activation,
                multiplier,
                regime,
            });
        }

        self.metrics.record_evaluation(evaluated, &updates);
        updates
    }
}

/// Per-query context that prevents over-suppression of shared neighbors
/// when a query returns multiple results.
///
/// When a query returns 10 results and 3 of them neighbor memory N,
/// N would receive compounding suppression of up to `0.85^3 = 0.614` --
/// a 39% stability reduction from a single query. The dedup context
/// caps cumulative reduction at `max_reduction_per_query` (default 25%).
///
/// # Usage
///
/// Create one per query. Pass each `StabilityUpdate` through
/// `clamp_multiplier` before applying to storage.
pub struct QueryRifContext {
    /// Tracks the cumulative stability multiplier per affected neighbor.
    /// Key: neighbor MemoryId. Value: product of all multipliers
    /// applied so far within this query.
    affected: HashMap<MemoryId, f32>,
    /// Minimum allowed cumulative multiplier. Default: `0.75` (no more
    /// than 25% reduction per query).
    max_reduction_per_query: f32,
}

impl QueryRifContext {
    /// Create a new per-query RIF context from the given config.
    pub fn new(config: &RifConfig) -> Self {
        Self {
            affected: HashMap::new(),
            max_reduction_per_query: config.max_reduction_per_query,
        }
    }

    /// Clamp a multiplier so the cumulative effect on `memory_id`
    /// does not exceed the per-query reduction cap.
    ///
    /// Returns `(effective_multiplier, was_clamped)`.
    ///
    /// - `effective_multiplier`: the multiplier to actually apply
    ///   (may be closer to 1.0 than `raw_multiplier` if the cap was hit).
    /// - `was_clamped`: `true` if the multiplier was adjusted.
    pub fn clamp_multiplier(&mut self, memory_id: MemoryId, raw_multiplier: f32) -> (f32, bool) {
        let cumulative = self.affected.get(&memory_id).copied().unwrap_or(1.0);
        let proposed = cumulative * raw_multiplier;
        let clamped_cumulative = proposed.max(self.max_reduction_per_query);
        let effective = clamped_cumulative / cumulative;
        let was_clamped = (effective - raw_multiplier).abs() > f32::EPSILON;

        self.affected.insert(memory_id, clamped_cumulative);
        (effective, was_clamped)
    }
}
