//! Activation score calculation for RIF.
//!
//! Computes how strongly a neighbor memory is activated when a target
//! memory is retrieved. The activation score feeds into the nonmonotonic
//! plasticity function to determine whether the neighbor is weakened,
//! unchanged, or strengthened.

use crate::model::EdgeType;
use crate::rif::config::RifConfig;

/// Calculate how strongly a neighbor is activated by the retrieval of a target memory.
///
/// Returns a value in `[0.0, 1.0]` where `0.0` = not activated, `1.0` =
/// fully co-activated.
///
/// The four-factor multiplicative model:
///   `similarity * edge_weight * distance_decay * neighbor_retrievability * edge_type_factor`
///
/// When `similarity` is `None` (embedding not cache-resident), the
/// three-factor fallback is used:
///   `edge_weight * distance_decay * neighbor_retrievability * edge_type_factor`
///
/// # Arguments
///
/// * `similarity` -- Cosine similarity between the retrieved memory's
///   embedding and the neighbor's embedding. `None` when the neighbor's
///   embedding is not loaded in the RAM cache.
/// * `edge_weight` -- The `weight` field from the `Edge` connecting the
///   retrieved memory to this neighbor. Range `[0.0, 1.0]`.
/// * `edge_type` -- The `EdgeType` of the connecting edge.
/// * `graph_distance` -- Number of hops from the retrieved memory to
///   this neighbor (1 = direct neighbor, 2 = neighbor-of-neighbor).
/// * `neighbor_retrievability` -- The neighbor's current FSRS
///   retrievability `R` (the `strength` field on `Memory`).
///   Range `[0.0, 1.0]`.
/// * `config` -- The active `RifConfig`.
pub fn calculate_activation(
    similarity: Option<f32>,
    edge_weight: f32,
    edge_type: EdgeType,
    graph_distance: u32,
    neighbor_retrievability: f32,
    config: &RifConfig,
) -> f32 {
    // Factor 1: Embedding similarity (when available).
    // Cosine similarity can be negative; clamp to [0.0, 1.0].
    // When unavailable (not cache-resident), this factor is omitted
    // entirely -- the remaining factors provide a coarser estimate.
    let similarity_factor = similarity.map(|s| s.clamp(0.0, 1.0)).unwrap_or(1.0);

    // Factor 2: Edge weight (association strength).
    // Range already [0.0, 1.0] by Edge invariant.
    let edge_strength = edge_weight;

    // Factor 3: Distance decay.
    // gamma^d -- at gamma=0.3: hop 1 = 0.3, hop 2 = 0.09, hop 3 = 0.027
    let distance_decay = config.gamma.powf(graph_distance as f32);

    // Factor 4: Neighbor retrievability.
    // A nearly-forgotten memory (low R) is harder to activate.
    let neighbor_strength = neighbor_retrievability;

    // Factor 5: Edge type modifier.
    let edge_factor = rif_edge_factor(edge_type);

    // Combine: multiplicative model. Any zero factor kills activation.
    let raw = similarity_factor * edge_strength * distance_decay * neighbor_strength * edge_factor;

    raw.clamp(0.0, 1.0)
}

/// Edge type modifier for RIF activation propagation.
///
/// This function is RIF-specific. Spreading activation in the graph module
/// uses a separate `spreading_edge_factor(EdgeType)` with different weights.
/// In particular, `Contradicts` carries `1.0` here (strongest competitors
/// for suppression) but `0.0` in spreading activation (contradictions should
/// not reinforce each other's persistence).
///
/// | EdgeType     | Factor | Rationale                                  |
/// |------------- |--------|--------------------------------------------|
/// | Associative  | 1.0    | Primary competition pathway                |
/// | ParentChild  | 0.7    | Hierarchical, moderate competition         |
/// | Causal       | 0.3    | Sequential, not competing                  |
/// | Contradicts  | 1.0    | Strongest competitors -- opposing claims    |
/// | Entity       | 0.5    | Moderate competition via shared referent   |
/// | Temporal     | 0.2    | Minimal competition, co-occurrence only    |
pub fn rif_edge_factor(edge_type: EdgeType) -> f32 {
    match edge_type {
        EdgeType::Associative => 1.0,
        EdgeType::ParentChild => 0.7,
        EdgeType::Causal => 0.3,
        EdgeType::Contradicts => 1.0,
        EdgeType::Entity => 0.5,
        EdgeType::Temporal => 0.2,
        EdgeType::Supersedes => 0.0,
    }
}
