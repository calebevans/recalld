//! Bounded, deterministic graph reads for internal retrieval semantics 2.
//! Legacy activation and mutation paths remain separate.
use std::collections::{BTreeMap, HashMap, HashSet};

use super::{RelationshipGraph, SpreadingActivationConfig, spreading_edge_factor};
use crate::model::{CachedRecord, DecayPhase, EdgeType, MemoryId, NamespaceId};

pub const MAX_GRAPH_STATES: usize = 400;
pub const MAX_GRAPH_EDGES: usize = 12_800;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GraphWork {
    pub expanded_states: usize,
    pub examined_edges: usize,
    pub state_limit: bool,
    pub edge_limit: bool,
}

#[derive(Debug, Clone)]
pub struct GraphHit {
    pub id: MemoryId,
    pub activation: f32,
    pub depth: u8,
}

#[derive(Debug, Default)]
pub struct GraphExpansion {
    pub hits: Vec<GraphHit>,
    pub work: GraphWork,
}

/// A bounded read of incoming structure; incomplete reads never prove currentness.
#[derive(Debug)]
pub struct SupersessionLinks {
    pub namespace: Option<NamespaceId>,
    pub successors: Vec<MemoryId>,
    pub examined_edges: usize,
    pub incomplete: bool,
    pub missing_endpoint: bool,
}

pub fn supersession_links(
    graph: &RelationshipGraph,
    id: MemoryId,
    allowance: usize,
) -> SupersessionLinks {
    let mut result = SupersessionLinks {
        namespace: None,
        successors: Vec::new(),
        examined_edges: 0,
        incomplete: false,
        missing_endpoint: false,
    };
    let Some(node) = graph.get_node(&id) else {
        return result;
    };
    result.namespace = Some(node.namespace_id);
    result.incomplete = node.incoming.len() > allowance;
    for key in node.incoming.iter().take(allowance) {
        result.examined_edges += 1;
        let Some(edge) = graph.edges.get(*key) else {
            result.missing_endpoint = true;
            continue;
        };
        if graph
            .nodes
            .get(edge.target)
            .is_none_or(|target| target.memory_id != id)
        {
            result.missing_endpoint = true;
            continue;
        }
        if edge.edge_type == EdgeType::Supersedes {
            if let Some(source) = graph.nodes.get(edge.source) {
                result.successors.push(source.memory_id);
            } else {
                result.missing_endpoint = true;
            }
        }
    }
    result.successors.sort();
    result.successors.dedup();
    result
}

/// Synchronous layers, with canonical eligibility/strength supplied by the caller.
/// Work caps are independent of depth. No mutation, cached-strength authority,
/// seed reinjection, or associative relay through ghosts/tombstones.
pub fn expand(
    graph: &RelationshipGraph,
    seeds: &[(MemoryId, f32)],
    namespace: NamespaceId,
    max_hops: u8,
    eligible: &HashMap<MemoryId, CachedRecord>,
) -> Result<GraphExpansion, &'static str> {
    let config = SpreadingActivationConfig::default();
    let live = |id: &MemoryId| {
        eligible
            .get(id)
            .filter(|r| {
                r.namespace_id == namespace
                    && !matches!(r.phase, DecayPhase::Ghost | DecayPhase::Tombstone)
            })
            .filter(|_| {
                graph
                    .get_node(id)
                    .is_some_and(|n| n.namespace_id == namespace)
            })
    };
    let mut layer = BTreeMap::<MemoryId, f32>::new();
    for &(id, score) in seeds {
        let Some(record) = live(&id) else {
            continue;
        };
        if !score.is_finite() || !record.strength.is_finite() {
            return Err("nonfinite graph seed");
        }
        let value = (score * record.strength).clamp(0.0, 1.0);
        if value >= config.firing_threshold {
            layer
                .entry(id)
                .and_modify(|best| *best = best.max(value))
                .or_insert(value);
        }
    }
    let seed_ids: HashSet<_> = layer.keys().copied().collect();
    let mut totals = BTreeMap::<MemoryId, (f32, u8)>::new();
    let mut result = GraphExpansion::default();
    'layers: for depth in 0..max_hops.min(3) {
        let mut next = BTreeMap::<MemoryId, f32>::new();
        for (id, activation) in layer {
            if activation < config.firing_threshold {
                continue;
            }
            if result.work.expanded_states == MAX_GRAPH_STATES {
                result.work.state_limit = true;
                break 'layers;
            }
            result.work.expanded_states += 1;
            let node = graph.get_node(&id).ok_or("missing graph node")?;
            let remaining = MAX_GRAPH_EDGES - result.work.examined_edges;
            let mut edges = Vec::new();
            for key in node.outgoing.iter().chain(&node.incoming).take(remaining) {
                result.work.examined_edges += 1;
                let edge = graph.edges.get(*key).ok_or("missing graph edge")?;
                let source = graph.nodes.get(edge.source).ok_or("missing graph source")?;
                let target = graph.nodes.get(edge.target).ok_or("missing graph target")?;
                if source.memory_id != id && target.memory_id != id {
                    return Err("invalid graph adjacency");
                }
                let neighbor = if source.memory_id == id {
                    target.memory_id
                } else {
                    source.memory_id
                };
                if !edge.weight.is_finite() {
                    return Err("nonfinite graph edge");
                }
                edges.push((
                    neighbor,
                    edge.edge_type.as_u8(),
                    source.memory_id,
                    target.memory_id,
                    edge.weight,
                ));
            }
            if node.degree() > remaining {
                // Do not admit an insertion-order-dependent partial adjacency list.
                result.work.edge_limit = true;
                break 'layers;
            }
            // Rebuild can load reciprocal stored edges even though add_edge
            // rejects them. Deduplicate the directed identity, not the neighbor:
            // A->B and B->A both contribute when spreading from A.
            edges.sort_by_key(|&(neighbor, kind, source, target, _)| {
                (neighbor, kind, source, target)
            });
            edges.dedup_by_key(|edge| (edge.0, edge.1, edge.2, edge.3));
            let fan = (config.s_max - (node.degree() as f32 + 1.0).ln()).max(0.0);
            for (neighbor, kind, _, _, weight) in edges {
                if seed_ids.contains(&neighbor) {
                    continue;
                }
                let Some(record) = live(&neighbor) else {
                    continue;
                };
                if !record.strength.is_finite() {
                    return Err("nonfinite graph strength");
                }
                let factor = spreading_edge_factor(EdgeType::from_u8(kind).unwrap());
                let spread =
                    activation * weight * factor * fan * config.hop_decay * record.strength;
                if !spread.is_finite() {
                    return Err("nonfinite graph activation");
                }
                if spread < config.min_spread {
                    continue;
                }
                let value = next.entry(neighbor).or_default();
                *value = (*value + spread).min(1.0);
                let total = totals.entry(neighbor).or_insert((0.0, depth + 1));
                total.0 = (total.0 + spread).min(1.0);
                total.1 = total.1.min(depth + 1);
            }
        }
        layer = next;
    }
    result.hits = totals
        .into_iter()
        .filter(|(_, (score, _))| *score >= config.output_threshold)
        .map(|(id, (activation, depth))| GraphHit {
            id,
            activation,
            depth,
        })
        .collect();
    result
        .hits
        .sort_by(|a, b| b.activation.total_cmp(&a.activation).then(a.id.cmp(&b.id)));
    Ok(result)
}

/// Actual incident edges; namespace and lifecycle eligibility remain canonical.
#[derive(Debug, Clone)]
pub struct RifEdge {
    pub target: MemoryId,
    pub namespace: NamespaceId,
    pub edge_type: EdgeType,
    pub weight: f32,
}

pub fn rif_edges(
    graph: &RelationshipGraph,
    anchor: MemoryId,
    namespace: NamespaceId,
) -> Result<Vec<RifEdge>, &'static str> {
    let node = graph.get_node(&anchor).ok_or("missing RIF anchor")?;
    if node.namespace_id != namespace {
        return Err("RIF anchor scope mismatch");
    }
    node.outgoing
        .iter()
        .chain(&node.incoming)
        .map(|key| {
            let edge = graph.edges.get(*key).ok_or("missing RIF edge")?;
            let source = graph.nodes.get(edge.source).ok_or("missing RIF source")?;
            let target = graph.nodes.get(edge.target).ok_or("missing RIF target")?;
            let other = if source.memory_id == anchor {
                target
            } else if target.memory_id == anchor {
                source
            } else {
                return Err("invalid RIF adjacency");
            };
            if !edge.weight.is_finite() || !(0.0..=1.0).contains(&edge.weight) {
                return Err("invalid RIF edge weight");
            }
            Ok(RifEdge {
                target: other.memory_id,
                namespace: other.namespace_id,
                edge_type: edge.edge_type,
                weight: edge.weight,
            })
        })
        .collect()
}
