//! Spreading activation (connection bonus) and edge factors.
//!
//! Implements ACT-R spreading activation adapted for Recalld: fan-effect
//! attenuation, 2-hop propagation with decay, and the connection bonus
//! formula. Also provides batch recomputation for decay sweeps.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::graph::structure::{GraphNode, NodeKey, RelationshipGraph};
use crate::model::{DecayPhase, EdgeType, MemoryId, NamespaceId};
use crate::storage::PersistedEdge;

// ═══════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════

/// Maximum connection bonus to prevent over-accumulation.
/// A single memory's bonus cannot exceed this regardless of neighbor count.
pub const MAX_CONNECTION_BONUS: f32 = 0.15;

/// Default S_max: maximum associative strength from ACT-R.
/// At fan >= 7, spreading activation drops to zero.
pub const DEFAULT_S_MAX: f32 = 2.0;

/// Default scale factor: controls total bonus magnitude.
pub const DEFAULT_SCALE: f32 = 0.05;

/// Decay multiplier for 2-hop contributions.
/// Second-hop neighbors contribute at half strength.
pub const HOP_2_DECAY: f32 = 0.5;

// ═══════════════════════════════════════════════════════════════════════
// ActivationConfig
// ═══════════════════════════════════════════════════════════════════════

/// Configuration for spreading activation calculation.
/// Passed to `connection_bonus` and used by the decay sweep.
#[derive(Debug, Clone)]
pub struct ActivationConfig {
    /// Maximum associative strength (ACT-R S_max).
    /// Higher values extend the fan threshold.
    /// Default: 2.0 (zero activation at fan >= 7).
    pub s_max: f32,

    /// Scale factor that limits total bonus magnitude.
    /// Default: 0.05
    pub scale: f32,

    /// Whether to include 2-hop neighbors in the calculation.
    /// Default: true. Set to false for performance-critical paths
    /// or namespaces where 2-hop cost is unacceptable.
    pub include_2hop: bool,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            s_max: DEFAULT_S_MAX,
            scale: DEFAULT_SCALE,
            include_2hop: true,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SpreadingActivationConfig
// ═══════════════════════════════════════════════════════════════════════

/// Configuration for priority-queue spreading activation (PQSA).
///
/// Unlike `ActivationConfig` (used by the batch decay sweep's
/// `connection_bonus`), this config governs the query-time spreading
/// activation that discovers related memories from a set of search
/// result seeds.
///
/// Termination is signal-based: nodes whose accumulated activation
/// falls below `firing_threshold` are not expanded, and propagation
/// increments below `min_spread` are discarded.  The `max_budget`
/// provides a hard upper bound on nodes processed to prevent runaway
/// on pathologically dense subgraphs.
#[derive(Debug, Clone)]
pub struct SpreadingActivationConfig {
    /// Maximum associative strength (ACT-R S_max) for fan attenuation.
    /// At `degree >= ceil(e^s_max) - 1`, the fan factor drops to zero.
    /// With the default 2.0, nodes with degree >= 7 contribute nothing.
    /// Reuses the same semantic as `ActivationConfig::s_max`.
    pub s_max: f32,

    /// Multiplicative decay applied at each hop.  A value of 0.5 means
    /// each hop halves the signal strength before edge-specific factors
    /// are applied.
    pub hop_decay: f32,

    /// Minimum activation required to expand (fire) a node.  Nodes
    /// below this threshold remain in the activation map but are not
    /// expanded further.  This is the primary signal-based termination
    /// mechanism.
    pub firing_threshold: f32,

    /// Minimum activation increment to propagate along an edge.
    /// Increments below this value are discarded to avoid pushing
    /// negligible updates through the queue.
    pub min_spread: f32,

    /// Minimum accumulated activation for a node to appear in the
    /// output.  Should be <= `firing_threshold`.
    pub output_threshold: f32,

    /// Maximum number of nodes to fire (expand).  Provides a hard
    /// budget ceiling independent of the signal-based threshold.
    pub max_budget: usize,
}

impl Default for SpreadingActivationConfig {
    fn default() -> Self {
        Self {
            s_max: DEFAULT_S_MAX, // 2.0
            hop_decay: 0.75,
            firing_threshold: 0.03,
            min_spread: 0.005,
            output_threshold: 0.02,
            max_budget: 100,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// spreading_edge_factor
// ═══════════════════════════════════════════════════════════════════════

/// Weight factor for spreading activation (connection bonus).
///
/// These weights are INTENTIONALLY different from `rif_edge_factor`:
///
/// | Edge Type    | Spreading | RIF   | Rationale                                   |
/// |-------------|-----------|-------|---------------------------------------------|
/// | Associative | 1.0       | 1.0   | Primary pathway in both contexts            |
/// | ParentChild | 0.7       | 0.7   | Hierarchical: moderate in both              |
/// | Causal      | 0.5       | 0.3   | Sequential; more relevant for spreading     |
/// | Contradicts | 0.0       | 1.0   | Contradictions must NOT boost each other     |
/// | Entity      | 0.6       | 0.5   | Co-reference: moderate boost, mild competition |
/// | Temporal    | 0.6       | 0.2   | Co-occurrence: moderate boost, minimal competition |
///
/// See Spec 03 section 4.2 for the detailed rationale on the Contradicts
/// divergence.
pub fn spreading_edge_factor(edge_type: EdgeType) -> f32 {
    match edge_type {
        EdgeType::Associative => 1.0,
        EdgeType::ParentChild => 0.7,
        EdgeType::Causal => 0.5,
        EdgeType::Contradicts => 0.0,
        EdgeType::Entity => 0.6,
        EdgeType::Temporal => 0.6,
        EdgeType::Supersedes => 0.0,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// rif_edge_factor (defined here for co-location, separate from spreading)
// ═══════════════════════════════════════════════════════════════════════

/// Weight factor for RIF competitor activation propagation.
///
/// Separate from `spreading_edge_factor` because the two serve different
/// cognitive functions: spreading activation asks "should retrieving A
/// help preserve B?", while RIF asks "does retrieving A compete with B?"
///
/// | Edge Type    | Factor | Rationale                                  |
/// |-------------|--------|--------------------------------------------|
/// | Associative | 1.0    | Primary competition pathway                |
/// | ParentChild | 0.7    | Hierarchical, moderate competition         |
/// | Causal      | 0.3    | Sequential, not competing                  |
/// | Contradicts | 1.0    | Strongest competitors -- opposing claims   |
/// | Entity      | 0.5    | Moderate competition via shared referent   |
/// | Temporal    | 0.2    | Minimal competition, co-occurrence only    |
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

// ═══════════════════════════════════════════════════════════════════════
// connection_bonus
// ═══════════════════════════════════════════════════════════════════════

/// Calculate the connection bonus for a memory based on its graph neighbors.
///
/// Returns a value in `[0.0, MAX_CONNECTION_BONUS]` representing the
/// raw spreading activation. The caller applies the damping formula:
///
/// ```text
/// R_effective = R_base + bonus * (1.0 - R_base)
/// ```
///
/// The `(1.0 - R_base)` factor is NOT applied inside this function.
/// The bonus is the raw spreading activation value; damping belongs
/// in the `R_effective` formula only. See Spec 05 section 5.3.
///
/// # ACT-R fan effect
///
/// The association strength from neighbor j is:
///     s_ji = max(0, S_MAX - ln(fan_j + 1))
///
/// where fan_j is the total degree of neighbor j. High-fan nodes
/// (degree >= 7 with S_MAX=2.0) contribute zero activation --
/// a memory connected to everything is connected to nothing.
///
/// # Returns
/// Connection bonus in [0.0, 0.15]. Returns 0.0 if the memory
/// is not in the graph or has no neighbors.
pub fn connection_bonus(
    memory_id: MemoryId,
    graph: &RelationshipGraph,
    config: &ActivationConfig,
) -> f32 {
    let Some(&node_key) = graph.id_index.get(&memory_id) else {
        return 0.0;
    };
    let Some(node) = graph.nodes.get(node_key) else {
        return 0.0;
    };

    let mut bonus: f32 = 0.0;

    // 1-hop neighbors
    let hop1: Vec<(NodeKey, f32)> =
        collect_neighbor_contributions(graph, node_key, node, config.s_max);

    for (_, contribution) in &hop1 {
        bonus += contribution * config.scale;
    }

    // Optional 2-hop with decay
    if config.include_2hop {
        for (neighbor_key, _) in &hop1 {
            if let Some(neighbor_node) = graph.nodes.get(*neighbor_key) {
                let hop2 = collect_neighbor_contributions(
                    graph,
                    *neighbor_key,
                    neighbor_node,
                    config.s_max,
                );
                for (hop2_key, contribution) in hop2 {
                    // Skip if hop-2 is the original node (would double-count)
                    if hop2_key == node_key {
                        continue;
                    }
                    bonus += contribution * config.scale * HOP_2_DECAY;
                }
            }
        }
    }

    bonus.min(MAX_CONNECTION_BONUS)
}

// ═══════════════════════════════════════════════════════════════════════
// collect_neighbor_contributions (internal)
// ═══════════════════════════════════════════════════════════════════════

/// Collect fan-attenuated activation contributions from a node's
/// direct neighbors.
///
/// For each neighbor j connected to `node_key`:
///     contribution_j = s_ji * R_j * edge_weight * spreading_edge_factor
///
/// where s_ji = max(0, s_max - ln(fan_j + 1))
///
/// Returns (neighbor_key, contribution) pairs. Neighbors with zero
/// contribution (fan too high, ghost with R=0, contradicts edge)
/// are excluded.
fn collect_neighbor_contributions(
    graph: &RelationshipGraph,
    node_key: NodeKey,
    node: &GraphNode,
    s_max: f32,
) -> Vec<(NodeKey, f32)> {
    let mut results = Vec::new();

    let all_edge_keys = node.outgoing.iter().chain(node.incoming.iter());

    for &edge_key in all_edge_keys {
        let Some(edge) = graph.edges.get(edge_key) else {
            continue;
        };

        // Apply edge type factor (Contradicts -> 0.0, skipped)
        let edge_factor = spreading_edge_factor(edge.edge_type);
        if edge_factor == 0.0 {
            continue;
        }

        let neighbor_key = if edge.source == node_key {
            edge.target
        } else {
            edge.source
        };

        let Some(neighbor) = graph.nodes.get(neighbor_key) else {
            continue;
        };

        // Fan = total degree of the NEIGHBOR (not the source node)
        let fan_j = (neighbor.outgoing.len() + neighbor.incoming.len()) as f32;
        let s_ji = (s_max - (fan_j + 1.0).ln()).max(0.0);

        if s_ji > 0.0 {
            let contribution = s_ji
                * neighbor.strength // R_j: neighbor's retrievability
                * edge.weight // edge associative strength
                * edge_factor; // edge type modifier
            results.push((neighbor_key, contribution));
        }
    }

    results
}

// ═══════════════════════════════════════════════════════════════════════
// effective_retrievability
// ═══════════════════════════════════════════════════════════════════════

/// Compute R_effective from base retrievability and connection bonus.
///
/// ```text
/// R_effective = R_base + bonus * (1.0 - R_base)
/// ```
///
/// This ensures R_effective never exceeds 1.0. The (1 - R_base)
/// factor means the bonus matters more for weaker memories.
///
/// Used by the decay sweep (phase transition checks) and the
/// query engine (result ranking).
pub fn effective_retrievability(r_base: f32, connection_bonus: f32) -> f32 {
    (r_base + connection_bonus * (1.0 - r_base)).min(1.0)
}

// ═══════════════════════════════════════════════════════════════════════
// recompute_centrality (batch sweep)
// ═══════════════════════════════════════════════════════════════════════

/// Recompute connection bonuses for all nodes in the graph.
///
/// Called by the decay sweep (at most daily). Iterates all non-ghost
/// nodes, computes their connection bonus, and returns a map of
/// memory_id -> bonus for the sweep to apply as R_effective adjustments.
///
/// # Performance
/// At 100K nodes, degree 10, with 2-hop enabled:
///   - 1-hop: 100K * 10 = 1M neighbor lookups
///   - 2-hop: 100K * 10 * 10 = 10M lookups (with dedup)
///   - Total: ~1-2 seconds
///
/// This is acceptable for a daily batch operation. If profiling shows
/// it's too slow, set `config.include_2hop = false` per-namespace.
pub fn recompute_centrality(
    graph: &RelationshipGraph,
    config: &ActivationConfig,
) -> HashMap<MemoryId, f32> {
    let mut bonuses = HashMap::with_capacity(graph.nodes.len());

    for (_, node) in graph.nodes.iter() {
        // Skip ghosts -- they don't benefit from connection bonus
        // (they're below R=0.3 and heading toward deletion)
        if node.decay_phase == DecayPhase::Ghost {
            continue;
        }

        let bonus = connection_bonus(node.memory_id, graph, config);
        if bonus > 0.0 {
            bonuses.insert(node.memory_id, bonus);
        }
    }

    bonuses
}

// ═══════════════════════════════════════════════════════════════════════
// Priority-Queue Spreading Activation (PQSA)
// ═══════════════════════════════════════════════════════════════════════

/// Entry in the PQSA priority queue.  Ordered by activation
/// descending so `BinaryHeap::pop` yields the highest-activation
/// node.
#[derive(PartialEq)]
struct ActivationEntry {
    activation: f32,
    node_key: NodeKey,
    memory_id: MemoryId,
}

impl Eq for ActivationEntry {}

impl Ord for ActivationEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher activation = higher priority.
        // f32 does not impl Ord, so use partial_cmp with a fallback.
        self.activation
            .partial_cmp(&other.activation)
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for ActivationEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority-queue spreading activation (PQSA) for query-time
/// graph expansion.
///
/// Given a set of seed memories (typically from vector search and/or
/// FTS), spreads activation through the relationship graph to discover
/// additional related memories.  Returns non-seed memories whose
/// accumulated activation exceeds `config.output_threshold`, sorted
/// by activation descending.
///
/// # Algorithm
///
/// 1. **Seed**: Each seed's initial activation = `similarity_score *
///    node.strength` (FSRS retrievability gate).  All seeds are
///    inserted into a max-heap.
///
/// 2. **Spread**: Pop the highest-activation node.  For each neighbor
///    connected by a non-Contradicts edge in the same namespace:
///    - Compute spread = `current_activation * edge.weight *
///      spreading_edge_factor * fan_attenuation * hop_decay *
///      neighbor.strength`
///    - If spread >= `min_spread`, accumulate on the neighbor
///      (additive, clamped to 1.0) and push to the heap if the
///      neighbor is above `firing_threshold` and has not yet fired.
///
/// 3. **Collect**: Return all non-seed entries above
///    `output_threshold`, sorted by activation descending.
///
/// # Termination
///
/// - **Signal-based**: nodes below `firing_threshold` are not
///   expanded, so activation naturally dies out in sparse or
///   low-relevance regions.
/// - **Budget**: at most `max_budget` nodes are fired.
/// - No fixed depth limit.
///
/// # Convergence
///
/// Nodes reachable from multiple seeds accumulate activation
/// additively (clamped to 1.0).  This produces "convergence
/// amplification": a memory related to several search hits ranks
/// higher than one related to a single hit.
///
/// # Relationship to existing code
///
/// - Reuses `spreading_edge_factor()` for per-edge-type weights.
/// - Reuses the ACT-R fan attenuation formula from
///   `collect_neighbor_contributions()`:
///   `s_ji = max(0, s_max - ln(degree + 1))`.
/// - Does NOT modify `connection_bonus()` or `ActivationConfig` --
///   those serve the batch decay sweep, a separate concern.
///
/// # Arguments
///
/// * `graph` - The relationship graph (caller holds the read lock).
/// * `seeds` - `(memory_id, similarity_score)` pairs from vector/FTS
///   search.  `similarity_score` is in `[0.0, 1.0]`.
/// * `namespace_id` - Only nodes in this namespace participate.
/// * `config` - Tuning parameters for the spreading algorithm.
///
/// # Returns
///
/// `(memory_id, accumulated_activation)` pairs for non-seed memories
/// above `output_threshold`, sorted by activation descending.
pub fn spreading_activation(
    graph: &RelationshipGraph,
    seeds: &[(MemoryId, f32)],
    namespace_id: NamespaceId,
    config: &SpreadingActivationConfig,
) -> Vec<(MemoryId, f32)> {
    // -- Accumulated activation per node --
    let mut activation: HashMap<NodeKey, f32> = HashMap::with_capacity(seeds.len() * 4);
    // -- Nodes that have already been expanded --
    let mut fired: HashSet<NodeKey> = HashSet::with_capacity(seeds.len() * 4);
    // -- Track which NodeKeys are seeds (excluded from output) --
    let mut seed_keys: HashSet<NodeKey> = HashSet::with_capacity(seeds.len());
    // -- Priority queue (max-heap by activation) --
    let mut queue: BinaryHeap<ActivationEntry> = BinaryHeap::with_capacity(seeds.len() * 4);
    // -- Budget counter --
    let mut nodes_processed: usize = 0;

    // ── PHASE 1: SEED ──────────────────────────────────────────────
    for &(memory_id, similarity_score) in seeds {
        let Some(&node_key) = graph.id_index.get(&memory_id) else {
            continue; // seed not in graph
        };
        let Some(node) = graph.nodes.get(node_key) else {
            continue;
        };

        // Namespace gate: skip seeds outside the target namespace
        if node.namespace_id != namespace_id {
            continue;
        }

        // Ghost gate: ghosts don't seed activation
        if node.decay_phase == DecayPhase::Ghost {
            continue;
        }

        // Initial activation = similarity * FSRS retrievability
        let initial = (similarity_score * node.strength).clamp(0.0, 1.0);
        if initial < config.firing_threshold {
            continue; // too weak to fire
        }

        activation.insert(node_key, initial);
        seed_keys.insert(node_key);
        queue.push(ActivationEntry {
            activation: initial,
            node_key,
            memory_id,
        });
    }

    // ── PHASE 2: SPREAD ────────────────────────────────────────────
    while let Some(entry) = queue.pop() {
        // Budget check
        if nodes_processed >= config.max_budget {
            break;
        }

        // Already-fired check (a node can be pushed multiple times
        // as its activation accumulates; only expand it once)
        if fired.contains(&entry.node_key) {
            continue;
        }

        // Retrieve the current accumulated activation (may be higher
        // than entry.activation if other paths have since contributed)
        let current_activation = match activation.get(&entry.node_key) {
            Some(&a) => a,
            None => continue,
        };

        // Firing threshold check
        if current_activation < config.firing_threshold {
            continue;
        }

        let Some(node) = graph.nodes.get(entry.node_key) else {
            continue;
        };

        // Ghost gate: don't fire ghost nodes
        if node.decay_phase == DecayPhase::Ghost {
            continue;
        }

        // Mark as fired, increment budget
        fired.insert(entry.node_key);
        nodes_processed += 1;

        // Fan attenuation for THIS node (the source of the spread).
        // Reuses the ACT-R formula from collect_neighbor_contributions,
        // but applied to the SOURCE node's fan, not the neighbor's fan.
        // This matches the semantics: a highly-connected node spreads
        // its activation thinly.
        let degree = node.degree() as f32;
        let fan_attenuation = (config.s_max - (degree + 1.0).ln()).max(0.0);
        if fan_attenuation == 0.0 {
            continue; // hub node: degree too high to spread
        }

        // Iterate ALL edges (both outgoing and incoming -- the graph
        // is treated as undirected for spreading activation)
        let all_edge_keys = node.outgoing.iter().chain(node.incoming.iter());

        for &edge_key in all_edge_keys {
            let Some(edge) = graph.edges.get(edge_key) else {
                continue;
            };

            // Edge type factor (Contradicts -> 0.0, blocks propagation)
            let edge_factor = spreading_edge_factor(edge.edge_type);
            if edge_factor == 0.0 {
                continue;
            }

            // Determine the neighbor (the other end of the edge)
            let neighbor_key = if edge.source == entry.node_key {
                edge.target
            } else {
                edge.source
            };

            let Some(neighbor) = graph.nodes.get(neighbor_key) else {
                continue;
            };

            // Namespace gate
            if neighbor.namespace_id != namespace_id {
                continue;
            }

            // Ghost gate: ghosts don't receive activation
            if neighbor.decay_phase == DecayPhase::Ghost {
                continue;
            }

            // Compute spread amount
            let spread = current_activation
                * edge.weight           // edge associative strength [0,1]
                * edge_factor           // edge type modifier [0,1]
                * fan_attenuation       // ACT-R fan effect [0, s_max]
                * config.hop_decay      // per-hop signal decay
                * neighbor.strength; // FSRS retrievability gate

            // Drop negligible increments
            if spread < config.min_spread {
                continue;
            }

            // Accumulate on neighbor (additive, clamped to 1.0)
            let neighbor_activation = activation.entry(neighbor_key).or_insert(0.0);
            let new_activation = (*neighbor_activation + spread).min(1.0);
            *neighbor_activation = new_activation;

            // Push to queue if above firing threshold and not yet fired
            if new_activation >= config.firing_threshold && !fired.contains(&neighbor_key) {
                queue.push(ActivationEntry {
                    activation: new_activation,
                    node_key: neighbor_key,
                    memory_id: neighbor.memory_id,
                });
            }
        }
    }

    // ── PHASE 3: COLLECT ───────────────────────────────────────────
    let mut results: Vec<(MemoryId, f32)> = activation
        .iter()
        .filter_map(|(&node_key, &act)| {
            // Exclude seeds from output (caller already has them)
            if seed_keys.contains(&node_key) {
                return None;
            }
            // Output threshold gate
            if act < config.output_threshold {
                return None;
            }
            let node = graph.nodes.get(node_key)?;
            Some((node.memory_id, act))
        })
        .collect();

    // Sort by activation descending
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

    results
}

// ═══════════════════════════════════════════════════════════════════════
// rebuild_from_storage
// ═══════════════════════════════════════════════════════════════════════

/// Rebuild the in-memory graph from edges.db records.
///
/// Called at startup AFTER meta.db has been loaded (so id_index is
/// populated with all MemoryId -> NodeKey mappings).
///
/// # Sequence
/// 1. meta.db load populates nodes + id_index (CS-10 / CS-07)
/// 2. This function iterates edges.db and inserts edges
/// 3. For each edge: resolve source/target MemoryId to NodeKey
///    via id_index, create GraphEdge, push EdgeKey into
///    source.outgoing and target.incoming
///
/// # Error handling
/// Edges referencing unknown MemoryIds are logged and skipped.
/// This can happen if meta.db was compacted but edges.db was not
/// (a consistency violation that should be rare but survivable).
///
/// # Returns
/// Count of successfully loaded edges.
pub fn rebuild_from_storage(
    graph: &mut RelationshipGraph,
    edge_iter: impl Iterator<Item = PersistedEdge>,
) -> usize {
    let mut loaded: usize = 0;
    let mut skipped: usize = 0;

    for persisted in edge_iter {
        let source_key = match graph.id_index.get(&persisted.source) {
            Some(&k) => k,
            None => {
                skipped += 1;
                tracing::debug!(
                    source = %persisted.source,
                    "rebuild_from_storage: edge source not found in id_index"
                );
                continue;
            }
        };
        let target_key = match graph.id_index.get(&persisted.target) {
            Some(&k) => k,
            None => {
                skipped += 1;
                tracing::debug!(
                    target = %persisted.target,
                    "rebuild_from_storage: edge target not found in id_index"
                );
                continue;
            }
        };

        let edge = crate::graph::structure::GraphEdge {
            source: source_key,
            target: target_key,
            edge_type: persisted.edge_type,
            weight: persisted.weight,
            auto_created: persisted.auto_created,
            created_at: persisted.created_at as i64,
        };

        let ek = graph.edges.insert(edge);

        if let Some(source_node) = graph.nodes.get_mut(source_key) {
            source_node.outgoing.push(ek);
        }
        if let Some(target_node) = graph.nodes.get_mut(target_key) {
            target_node.incoming.push(ek);
        }

        loaded += 1;
    }

    if skipped > 0 {
        tracing::warn!(
            loaded,
            skipped,
            "rebuild_from_storage: skipped edges with missing endpoints"
        );
    }

    loaded
}

// ═══════════════════════════════════════════════════════════════════════
// graph_stats (free function)
// ═══════════════════════════════════════════════════════════════════════

/// Compute graph statistics for monitoring and debugging.
///
/// Exposed via the API server's `/debug/graph` endpoint.
/// Delegates to `RelationshipGraph::stats()`.
pub fn graph_stats(graph: &RelationshipGraph) -> crate::graph::structure::GraphStats {
    graph.stats()
}

// ═══════════════════════════════════════════════════════════════════════
// Unit tests: Spreading activation (PQSA)
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod spreading_tests {
    use super::*;
    use crate::graph::structure::RelationshipGraph;
    use crate::model::{DecayPhase, EdgeType, MemoryId, NamespaceId};

    fn test_ns() -> NamespaceId {
        NamespaceId::new(1)
    }
    fn other_ns() -> NamespaceId {
        NamespaceId::new(2)
    }

    /// Helper: add a node with Full phase and strength 1.0.
    fn add_node(graph: &mut RelationshipGraph, ns: NamespaceId) -> MemoryId {
        let id = MemoryId::new();
        graph
            .add_node(id, ns, DecayPhase::Full, 1.0, 0)
            .expect("add_node");
        id
    }

    /// Helper: add a node with a specific strength and decay phase.
    fn add_node_with(
        graph: &mut RelationshipGraph,
        ns: NamespaceId,
        phase: DecayPhase,
        strength: f32,
    ) -> MemoryId {
        let id = MemoryId::new();
        graph
            .add_node(id, ns, phase, strength, 0)
            .expect("add_node");
        id
    }

    /// Helper: add an edge with weight 1.0.
    fn link(graph: &mut RelationshipGraph, src: MemoryId, tgt: MemoryId, edge_type: EdgeType) {
        graph
            .add_edge(src, tgt, edge_type, 1.0, false)
            .expect("add_edge");
    }

    #[test]
    fn test_spreading_basic() {
        let mut graph = RelationshipGraph::new();
        let a = add_node(&mut graph, test_ns());
        let b = add_node(&mut graph, test_ns());
        let c = add_node(&mut graph, test_ns());

        link(&mut graph, a, b, EdgeType::Associative);
        link(&mut graph, b, c, EdgeType::Associative);

        let config = SpreadingActivationConfig::default();
        let results = spreading_activation(&graph, &[(a, 1.0)], test_ns(), &config);

        // Both B and C should appear
        let b_act = results.iter().find(|(id, _)| *id == b).map(|(_, a)| *a);
        let c_act = results.iter().find(|(id, _)| *id == c).map(|(_, a)| *a);

        assert!(b_act.is_some(), "B should be activated");
        assert!(c_act.is_some(), "C should be activated");
        assert!(
            b_act.unwrap() > c_act.unwrap(),
            "B ({:?}) should have higher activation than C ({:?})",
            b_act,
            c_act
        );

        // Seed A should NOT appear in results
        assert!(
            results.iter().all(|(id, _)| *id != a),
            "Seed A should be excluded from results"
        );
    }

    #[test]
    fn test_spreading_multi_seed() {
        let mut graph = RelationshipGraph::new();
        let a = add_node(&mut graph, test_ns());
        let b = add_node(&mut graph, test_ns());
        let c = add_node(&mut graph, test_ns()); // shared neighbor
        let d = add_node(&mut graph, test_ns()); // A-only neighbor
        let e = add_node(&mut graph, test_ns()); // B-only neighbor

        link(&mut graph, a, c, EdgeType::Associative);
        link(&mut graph, b, c, EdgeType::Associative);
        link(&mut graph, a, d, EdgeType::Associative);
        link(&mut graph, b, e, EdgeType::Associative);

        let config = SpreadingActivationConfig::default();
        let results = spreading_activation(&graph, &[(a, 1.0), (b, 1.0)], test_ns(), &config);

        let c_act = results.iter().find(|(id, _)| *id == c).map(|(_, a)| *a);
        let d_act = results.iter().find(|(id, _)| *id == d).map(|(_, a)| *a);
        let e_act = results.iter().find(|(id, _)| *id == e).map(|(_, a)| *a);

        assert!(c_act.is_some(), "shared neighbor C should be activated");
        assert!(d_act.is_some(), "D should be activated");
        assert!(e_act.is_some(), "E should be activated");

        // Convergence: C gets activation from BOTH A and B
        assert!(
            c_act.unwrap() > d_act.unwrap(),
            "C ({:?}) should have higher activation than D ({:?}) due to convergence",
            c_act,
            d_act
        );
        assert!(
            c_act.unwrap() > e_act.unwrap(),
            "C ({:?}) should have higher activation than E ({:?}) due to convergence",
            c_act,
            e_act
        );
    }

    #[test]
    fn test_spreading_fan_attenuation() {
        let mut graph = RelationshipGraph::new();
        let a = add_node(&mut graph, test_ns());
        let hub = add_node(&mut graph, test_ns());
        let beyond = add_node(&mut graph, test_ns());

        // Connect A -> hub
        link(&mut graph, a, hub, EdgeType::Associative);
        // Connect hub -> beyond
        link(&mut graph, hub, beyond, EdgeType::Associative);

        // Give hub 6 more outgoing edges to push degree to 8 (>= 7)
        for _ in 0..6 {
            let extra = add_node(&mut graph, test_ns());
            link(&mut graph, hub, extra, EdgeType::Associative);
        }

        // hub now has degree = 1 (incoming from A) + 7 (outgoing) = 8
        assert!(graph.degree(&hub) >= 7, "hub should have degree >= 7");

        let config = SpreadingActivationConfig::default();
        let results = spreading_activation(&graph, &[(a, 1.0)], test_ns(), &config);

        // Hub itself may or may not appear (it receives activation
        // from A, which has low degree, so A can spread to it).
        // But the hub should NOT spread further because its own fan
        // attenuation is zero.
        let beyond_act = results.iter().find(|(id, _)| *id == beyond);
        assert!(
            beyond_act.is_none(),
            "beyond-hub node should not be activated (hub fan attenuation = 0)"
        );
    }

    #[test]
    fn test_spreading_contradicts_blocked() {
        let mut graph = RelationshipGraph::new();
        let a = add_node(&mut graph, test_ns());
        let b = add_node(&mut graph, test_ns());

        link(&mut graph, a, b, EdgeType::Contradicts);

        let config = SpreadingActivationConfig::default();
        let results = spreading_activation(&graph, &[(a, 1.0)], test_ns(), &config);

        assert!(
            results.iter().all(|(id, _)| *id != b),
            "Contradicts edge should block activation propagation to B"
        );
    }

    #[test]
    fn test_spreading_decay_stops() {
        let mut graph = RelationshipGraph::new();
        let mut chain: Vec<MemoryId> = Vec::with_capacity(20);

        for _ in 0..20 {
            chain.push(add_node(&mut graph, test_ns()));
        }
        for i in 0..19 {
            link(&mut graph, chain[i], chain[i + 1], EdgeType::Associative);
        }

        let config = SpreadingActivationConfig::default();
        let results = spreading_activation(&graph, &[(chain[0], 1.0)], test_ns(), &config);

        // Node at position 15+ should not appear (signal too weak)
        for &distant_id in &chain[15..] {
            assert!(
                results.iter().all(|(id, _)| *id != distant_id),
                "distant node should not be activated (signal decayed)"
            );
        }

        // But near nodes (position 1-3) should appear
        assert!(
            results.iter().any(|(id, _)| *id == chain[1]),
            "immediate neighbor should be activated"
        );
    }

    #[test]
    fn test_spreading_namespace_isolation() {
        let mut graph = RelationshipGraph::new();
        let a = add_node(&mut graph, test_ns());
        let b = add_node(&mut graph, other_ns()); // different namespace
        let c = add_node(&mut graph, test_ns());

        link(&mut graph, a, b, EdgeType::Associative);
        link(&mut graph, a, c, EdgeType::Associative);

        let config = SpreadingActivationConfig::default();
        let results = spreading_activation(&graph, &[(a, 1.0)], test_ns(), &config);

        assert!(
            results.iter().all(|(id, _)| *id != b),
            "node in different namespace should not be activated"
        );
        assert!(
            results.iter().any(|(id, _)| *id == c),
            "node in same namespace should be activated"
        );
    }

    #[test]
    fn test_spreading_ghost_skipped() {
        let mut graph = RelationshipGraph::new();
        let a = add_node(&mut graph, test_ns());
        let ghost = add_node_with(&mut graph, test_ns(), DecayPhase::Ghost, 0.1);
        let c = add_node(&mut graph, test_ns());

        link(&mut graph, a, ghost, EdgeType::Associative);
        link(&mut graph, ghost, c, EdgeType::Associative);

        let config = SpreadingActivationConfig::default();
        let results = spreading_activation(&graph, &[(a, 1.0)], test_ns(), &config);

        // Ghost should not appear in results
        assert!(
            results.iter().all(|(id, _)| *id != ghost),
            "ghost node should not appear in results"
        );

        // C is behind the ghost -- activation should not pass through
        assert!(
            results.iter().all(|(id, _)| *id != c),
            "node beyond ghost should not be activated (ghost blocks propagation)"
        );
    }
} // mod spreading_tests
