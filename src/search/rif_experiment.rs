//! Operator-only, query-local A2 experiment. Not a wire option or promotion gate.
use super::*;
use crate::model::EdgeType;
use crate::rif::{QueryRifContext, RifConfig, calculate_activation, plasticity_multiplier};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_RIF_TARGETS_PER_ANCHOR: usize = 100;
pub const MAX_RIF_PAIRS: usize = 1_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RifMode {
    #[default]
    Off,
    Shadow,
    Experimental,
}

/// Aggregate diagnostics, plus multipliers restricted to returned eligible IDs.
#[derive(Debug, Clone, Default)]
pub struct RifDiagnostics {
    pub mode: RifMode,
    pub anchors: usize,
    pub examined_edges: usize,
    pub eligible_pairs: usize,
    pub duplicate_pairs: usize,
    pub admitted_pairs: usize,
    pub per_anchor_cap_hits: usize,
    pub pair_cap_hit: bool,
    pub unique_targets: usize,
    pub suppressed_targets: usize,
    pub cumulative_cap_hits: usize,
    pub similarity_available: usize,
    pub similarity_unavailable: usize,
    pub unchanged_score_channels: BTreeMap<RetrievalChannel, usize>,
    pub returned_multipliers: BTreeMap<MemoryId, f32>,
}

// Original independently scored evidence, grouped by resolved terminal. Kept
// only for an opted-in experiment. The same origin can have independent metadata
// and graph-rediscovery evidence: retain both, never overwrite by UUID.
// No recursive Candidate/lineage clones.
pub(super) type ScoreContributions = BTreeMap<MemoryId, Vec<Candidate>>;

pub(super) fn remember(inputs: &mut ScoreContributions, candidates: &[Candidate]) {
    for candidate in candidates {
        inputs
            .entry(candidate.record.id)
            .or_default()
            .push(candidate.clone());
    }
}

struct Pair {
    anchor: MemoryId,
    target: MemoryId,
    kind: EdgeType,
    activation: f32,
}

fn cosine(a: &[f32], b: &[f32]) -> Result<f32> {
    if a.len() != b.len() || a.is_empty() || a.iter().chain(b).any(|v| !v.is_finite()) {
        return Err(SearchError::Internal("invalid RIF embedding".into()));
    }
    // Actual vectors, not query relevance or a fabricated similarity. f64
    // accumulation avoids overflow from finite, non-unit stored inputs.
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| f64::from(x) * f64::from(y))
        .sum();
    let aa: f64 = a.iter().map(|&x| f64::from(x).powi(2)).sum();
    let bb: f64 = b.iter().map(|&x| f64::from(x).powi(2)).sum();
    if aa == 0.0 || bb == 0.0 {
        return Err(SearchError::Internal("zero RIF embedding".into()));
    }
    Ok((dot / (aa.sqrt() * bb.sqrt())).clamp(-1.0, 1.0) as f32)
}

impl QueryEngine {
    /// Explicit Rust operator/harness selection; V1 keeps its configured legacy
    /// processor. No serde, transport activation, or lifecycle-policy override.
    pub fn with_rif_mode(mut self, mode: RifMode) -> Self {
        self.rif_mode = mode;
        self
    }

    pub(super) fn apply_rif_experiment(
        &self,
        candidates: &mut [Candidate],
        query: &SearchQuery,
        namespace: NamespaceId,
        eligible: &HashMap<MemoryId, CachedRecord>,
        inputs: &ScoreContributions,
    ) -> Result<RifDiagnostics> {
        let mut diagnostic = RifDiagnostics {
            mode: self.rif_mode,
            ..Default::default()
        };
        let config = RifConfig {
            max_hops: 1,
            max_enhancement: 0.0,
            ..Default::default()
        };
        let live = |id: &MemoryId| {
            eligible.get(id).filter(|r| {
                Self::passes_hard_filters(r, query, namespace)
                    && !matches!(r.phase, DecayPhase::Ghost | DecayPhase::Tombstone)
            })
        };
        // Already off-ranked and resolved; freeze top-L before suppression.
        let mut pairs = BTreeMap::<(MemoryId, MemoryId), Pair>::new();
        for anchor in candidates.iter().take(query.limit) {
            if live(&anchor.record.id).is_none() {
                continue;
            }
            diagnostic.anchors += 1;
            let anchor_id = anchor.record.id;
            let edges = self.graph.rif_edges(anchor_id, namespace)?;
            diagnostic.examined_edges += edges.len();
            let anchor_vector = self.vector_indexes.get_vector(anchor_id);
            let mut similarities = HashMap::new();
            for edge in edges {
                if edge.target == anchor_id
                    || edge.edge_type == EdgeType::Supersedes
                    || edge.namespace != namespace
                {
                    continue;
                }
                let Some(target) = live(&edge.target) else {
                    continue;
                };
                if !target.stability.is_finite()
                    || target.stability <= 0.0
                    || !target.strength.is_finite()
                    || !(0.0..=1.0).contains(&target.strength)
                    || !edge.weight.is_finite()
                    || !(0.0..=1.0).contains(&edge.weight)
                {
                    return Err(SearchError::MetadataError(
                        "invalid canonical RIF inputs".into(),
                    ));
                }
                let similarity = if let Some(value) = similarities.get(&edge.target) {
                    *value
                } else {
                    let target_vector = self.vector_indexes.get_vector(edge.target);
                    let value = match (anchor_vector.as_deref(), target_vector.as_deref()) {
                        (Some(a), Some(b)) => Some(cosine(a, b)?),
                        _ => None,
                    };
                    similarities.insert(edge.target, value);
                    if value.is_some() {
                        diagnostic.similarity_available += 1;
                    } else {
                        diagnostic.similarity_unavailable += 1;
                    }
                    value
                };
                let activation = calculate_activation(
                    similarity,
                    edge.weight,
                    edge.edge_type,
                    1,
                    target.strength,
                    &config,
                );
                let activation = if activation == 0.0 { 0.0 } else { activation };
                let pair = Pair {
                    anchor: anchor_id,
                    target: edge.target,
                    kind: edge.edge_type,
                    activation,
                };
                let key = (anchor_id, edge.target);
                match pairs.get_mut(&key) {
                    Some(previous) => {
                        diagnostic.duplicate_pairs += 1;
                        if activation > previous.activation
                            || (activation == previous.activation
                                && edge.edge_type.as_u8() < previous.kind.as_u8())
                        {
                            *previous = pair;
                        }
                    }
                    None => {
                        pairs.insert(key, pair);
                    }
                }
            }
        }
        diagnostic.eligible_pairs = pairs.len();
        let mut pairs: Vec<_> = pairs.into_values().collect();
        pairs.sort_by(|a, b| {
            b.activation
                .total_cmp(&a.activation)
                .then(a.anchor.cmp(&b.anchor))
                .then(a.target.cmp(&b.target))
        });
        let mut per_anchor = HashMap::<MemoryId, usize>::new();
        let mut capped = BTreeSet::new();
        pairs.retain(|pair| {
            let count = per_anchor.entry(pair.anchor).or_default();
            if *count == MAX_RIF_TARGETS_PER_ANCHOR {
                capped.insert(pair.anchor);
                false
            } else {
                *count += 1;
                true
            }
        });
        diagnostic.per_anchor_cap_hits = capped.len();
        diagnostic.pair_cap_hit = pairs.len() > MAX_RIF_PAIRS;
        pairs.truncate(MAX_RIF_PAIRS);
        diagnostic.admitted_pairs = pairs.len();
        let mut context = QueryRifContext::new(&config);
        let mut multipliers = BTreeMap::<MemoryId, f32>::new();
        for pair in pairs {
            let cumulative = multipliers.entry(pair.target).or_insert(1.0);
            // Use actual target stability and the cumulative query-local state;
            // never materialize StabilityUpdate or raise a value to the floor.
            let stability = eligible[&pair.target].stability * *cumulative;
            let raw = plasticity_multiplier(pair.activation, &config).min(1.0);
            let floor_limited =
                (stability * raw).max(config.stability_floor).min(stability) / stability;
            let (applied, capped) = context.clamp_multiplier(pair.target, floor_limited);
            diagnostic.cumulative_cap_hits += usize::from(capped);
            *cumulative = (*cumulative * applied).clamp(0.75, 1.0);
        }
        diagnostic.unique_targets = multipliers.len();
        diagnostic.suppressed_targets = multipliers.values().filter(|&&m| m < 1.0).count();
        for candidate in candidates {
            let mut own = None;
            let mut inherited: Option<f32> = None;
            for baseline in &inputs[&candidate.record.id] {
                let origin = baseline.record.id;
                let mut scored = baseline.clone();
                scored.effective_r *= multipliers.get(&origin).copied().unwrap_or(1.0);
                // Recompute from the original channel inputs, not a merged or
                // already-boosted final score. Temporal boost is applied once.
                self.apply_boosts(std::slice::from_mut(&mut scored), query);
                if scored.composite_score == baseline.composite_score {
                    for &channel in &baseline.provenance.own_channels {
                        *diagnostic
                            .unchanged_score_channels
                            .entry(channel)
                            .or_default() += 1;
                    }
                }
                if origin == candidate.record.id {
                    own = Some(own.map_or(scored.composite_score, |v: f32| {
                        v.max(scored.composite_score)
                    }));
                } else {
                    inherited = Some(
                        inherited.map_or(scored.composite_score, |v| v.max(scored.composite_score)),
                    );
                }
            }
            let multiplier = multipliers
                .get(&candidate.record.id)
                .copied()
                .unwrap_or(1.0);
            diagnostic
                .returned_multipliers
                .insert(candidate.record.id, multiplier);
            if self.rif_mode == RifMode::Experimental {
                candidate.effective_r *= multiplier;
                candidate.provenance.own_score = own;
                candidate.provenance.inherited_floor = inherited;
                candidate.composite_score = own
                    .into_iter()
                    .chain(inherited)
                    .reduce(f32::max)
                    .ok_or_else(|| SearchError::Internal("RIF lost score evidence".into()))?;
            }
        }
        Ok(diagnostic)
    }
}
