//! Internal retrieval semantics. Deliberately not serde/wire configuration.
//! A1 generation-pinned reads and A4 readiness must precede transport rollout.
use std::collections::{BTreeMap, HashMap, HashSet};

use super::*;
use crate::graph::query::GraphWork;

pub const MAX_SUPERSESSION_HOPS: usize = 64;
pub const MAX_SUPERSESSION_EDGES: usize = 12_800;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VersionMode {
    #[default]
    Current,
    History,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RetrievalSemantics {
    #[default]
    V1,
    V2(VersionMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnresolvedReason {
    MissingRecord,
    MissingStructure,
    ForeignNamespace,
    Branch,
    Cycle,
    HopLimit,
    WorkLimit,
    Ineligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionStatus {
    Current,
    Historical,
    Unresolved(UnresolvedReason),
}

/// Observed operation outcome, not an A1 readiness/generation certificate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FtsAvailability {
    #[default]
    NotRequested,
    Available,
    Unavailable,
}

#[derive(Debug, Default)]
pub struct FtsDiagnostics {
    pub availability: FtsAvailability,
    /// Present only for successful v2 refill. Legacy top-K coverage is unknown.
    pub coverage: Option<FtsCoverage>,
}

/// Aggregate failures never contain rejected/foreign IDs. Per-ID fields are
/// restricted to returned results. This is not full v2 coverage/readiness.
#[derive(Debug)]
pub struct SemanticsDiagnostics {
    pub semantics: RetrievalSemantics,
    /// True when v2 forces RIF off; false preserves the configured legacy engine.
    pub rif_off: bool,
    pub fts: FtsDiagnostics,
    pub supersession_edges_examined: usize,
    pub unresolved: BTreeMap<UnresolvedReason, usize>,
    pub graph: GraphWork,
    pub versions: BTreeMap<MemoryId, VersionStatus>,
    pub graph_depths: BTreeMap<MemoryId, u8>,
    pub metadata_candidate_limited: bool,
    pub result_limit_applied: bool,
}

impl SemanticsDiagnostics {
    pub(super) fn new(semantics: RetrievalSemantics) -> Self {
        Self {
            semantics,
            rif_off: semantics != RetrievalSemantics::V1,
            fts: FtsDiagnostics::default(),
            supersession_edges_examined: 0,
            unresolved: BTreeMap::new(),
            graph: GraphWork::default(),
            versions: BTreeMap::new(),
            graph_depths: BTreeMap::new(),
            metadata_candidate_limited: false,
            result_limit_applied: false,
        }
    }
    fn reject(&mut self, reason: UnresolvedReason) {
        *self.unresolved.entry(reason).or_default() += 1;
    }
    pub(super) fn retain_returned(&mut self, candidates: &[Candidate]) {
        let ids: HashSet<_> = candidates.iter().map(|c| c.record.id).collect();
        self.versions.retain(|id, _| ids.contains(id));
        self.graph_depths = candidates
            .iter()
            .filter_map(|c| c.graph_depth.map(|d| (c.record.id, d)))
            .collect();
    }
}

#[derive(Debug)]
pub struct SemanticsResponse {
    pub response: SearchResponse,
    pub diagnostics: SemanticsDiagnostics,
}

/// Internal typed resolution; path IDs never enter aggregate diagnostics.
enum Resolution {
    Current(CachedRecord),
    Resolved {
        terminal: CachedRecord,
        path: Vec<MemoryId>,
    },
    Unresolved(UnresolvedReason),
}

pub(super) fn validate_query(query: &SearchQuery) -> Result<()> {
    if !query.min_score.is_finite() || query.filter.min_strength.is_some_and(|x| !x.is_finite()) {
        return Err(SearchError::InvalidFilter(
            "nonfinite search threshold".into(),
        ));
    }
    if matches!((query.time_range_start, query.time_range_end), (Some(a), Some(b)) if a > b) {
        return Err(SearchError::InvalidFilter(
            "reversed temporal boost bounds".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_record(record: &CachedRecord) -> Result<()> {
    if !record.strength.is_finite() || !record.decay_strength.is_finite() {
        return Err(SearchError::MetadataError(
            "nonfinite canonical strength".into(),
        ));
    }
    Ok(())
}

impl QueryEngine {
    async fn resolve_path(
        &self,
        origin: CachedRecord,
        namespace: NamespaceId,
        diagnostics: &mut SemanticsDiagnostics,
    ) -> Result<Resolution> {
        let mut record = origin;
        let mut seen = HashSet::new();
        let mut path = Vec::new();
        loop {
            if record.namespace_id != namespace {
                return Ok(Resolution::Unresolved(UnresolvedReason::ForeignNamespace));
            }
            if !seen.insert(record.id) {
                return Ok(Resolution::Unresolved(UnresolvedReason::Cycle));
            }
            let links = self.graph.supersession_links(
                record.id,
                MAX_SUPERSESSION_EDGES - diagnostics.supersession_edges_examined,
            )?;
            diagnostics.supersession_edges_examined += links.examined_edges;
            if links.incomplete {
                return Ok(Resolution::Unresolved(UnresolvedReason::WorkLimit));
            }
            if links.missing_endpoint || links.namespace.is_none() {
                return Ok(Resolution::Unresolved(UnresolvedReason::MissingStructure));
            }
            if links.namespace != Some(namespace) {
                return Ok(Resolution::Unresolved(UnresolvedReason::ForeignNamespace));
            }
            match links.successors.as_slice() {
                [] => {
                    return Ok(if path.is_empty() {
                        Resolution::Current(record)
                    } else {
                        Resolution::Resolved {
                            terminal: record,
                            path,
                        }
                    });
                }
                [successor] => {
                    if path.len() == MAX_SUPERSESSION_HOPS {
                        return Ok(Resolution::Unresolved(UnresolvedReason::HopLimit));
                    }
                    path.push(record.id);
                    record = match self.meta_store.get(successor).await? {
                        Some(record) => record,
                        None => return Ok(Resolution::Unresolved(UnresolvedReason::MissingRecord)),
                    };
                    // Tombstones may carry structural identity in a same-scope chain.
                    // Their removed payload and scores are never used.
                }
                _ => return Ok(Resolution::Unresolved(UnresolvedReason::Branch)),
            }
        }
    }

    async fn resolve_versions2(
        &self,
        candidates: &mut Vec<Candidate>,
        query: &SearchQuery,
        namespace: NamespaceId,
        mode: VersionMode,
        diagnostics: &mut SemanticsDiagnostics,
    ) -> Result<()> {
        let originals: HashMap<_, _> = candidates
            .iter()
            .map(|c| (c.record.id, c.clone()))
            .collect();
        candidates.sort_by_key(|c| c.record.id); // Work-budget winners cannot depend on insertion order.
        let mut resolved = BTreeMap::<MemoryId, Candidate>::new();
        for candidate in std::mem::take(candidates) {
            let Some(origin) = self.meta_store.get(&candidate.record.id).await? else {
                diagnostics.reject(UnresolvedReason::MissingRecord);
                continue;
            };
            if !Self::passes_hard_filters(&origin, query, namespace) {
                diagnostics.reject(UnresolvedReason::Ineligible);
                continue;
            }
            validate_record(&origin)?;
            let resolution = self
                .resolve_path(origin.clone(), namespace, diagnostics)
                .await?;
            if mode == VersionMode::History {
                let status = match resolution {
                    Resolution::Current(_) => VersionStatus::Current,
                    Resolution::Resolved { .. } => VersionStatus::Historical,
                    Resolution::Unresolved(reason) => {
                        diagnostics.reject(reason);
                        VersionStatus::Unresolved(reason)
                    }
                };
                diagnostics.versions.insert(origin.id, status);
                resolved.insert(
                    origin.id,
                    Candidate {
                        record: origin,
                        ..candidate
                    },
                );
                continue;
            }
            let terminal = match resolution {
                Resolution::Current(record) => record,
                Resolution::Resolved { terminal, path } => {
                    debug_assert!(path.len() <= MAX_SUPERSESSION_HOPS);
                    terminal
                }
                Resolution::Unresolved(reason) => {
                    diagnostics.reject(reason);
                    continue;
                }
            };
            if !Self::passes_hard_filters(&terminal, query, namespace) {
                diagnostics.reject(UnresolvedReason::Ineligible);
                continue;
            }
            validate_record(&terminal)?;
            let id = terminal.id;
            diagnostics.versions.insert(id, VersionStatus::Current);
            let replacement = resolved.entry(id).or_insert_with(|| {
                let mut own = originals
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| Candidate::new(terminal.clone()));
                own.record = terminal;
                own.effective_r = Self::compute_effective_r(&own.record);
                own
            });
            replacement.composite_score =
                replacement.composite_score.max(candidate.composite_score);
            replacement.seed_score = replacement.seed_score.max(candidate.seed_score);
            replacement.graph_depth = match (replacement.graph_depth, candidate.graph_depth) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        *candidates = resolved.into_values().collect();
        Ok(())
    }

    pub(super) async fn finish_semantics2(
        &self,
        candidates: &mut Vec<Candidate>,
        query: &SearchQuery,
        namespace: NamespaceId,
        eligible: &HashMap<MemoryId, CachedRecord>,
        diagnostics: &mut SemanticsDiagnostics,
        timings: &mut StageTimings,
    ) -> Result<()> {
        let RetrievalSemantics::V2(mode) = diagnostics.semantics else {
            unreachable!("v2-only pipeline");
        };
        let stage_start = Instant::now();
        self.apply_filters(candidates, query, &query.entities, namespace);
        timings.apply_filters_us = stage_start.elapsed().as_micros() as u64;
        let stage_start = Instant::now();
        for c in candidates.iter_mut() {
            c.effective_r = Self::compute_effective_r(&c.record);
        }
        timings.calculate_r_us = stage_start.elapsed().as_micros() as u64;
        let stage_start = Instant::now();
        self.apply_boosts(candidates, query);
        for c in candidates.iter_mut() {
            if !c.composite_score.is_finite() {
                return Err(SearchError::Internal("nonfinite composite score".into()));
            }
            if c.composite_score == 0.0 {
                c.composite_score = 0.0;
            }
            c.seed_score = c.relevance_score.unwrap_or(c.composite_score);
            c.graph_depth = Some(0);
        }
        let rank = |a: &Candidate, b: &Candidate| {
            b.composite_score
                .total_cmp(&a.composite_score)
                .then(a.record.id.cmp(&b.record.id))
        };
        if query.query_mode == QueryMode::MetadataOnly {
            let budget = (query.limit * 8).clamp(100, SearchQuery::MAX_FETCH_K);
            candidates.sort_by(rank);
            diagnostics.metadata_candidate_limited = candidates.len() > budget;
            candidates.truncate(budget);
        }
        if mode == VersionMode::Current {
            self.resolve_versions2(candidates, query, namespace, mode, diagnostics)
                .await?;
        }
        timings.rank_us = stage_start.elapsed().as_micros() as u64;
        if query.graph_depth > 0 {
            let stage_start = Instant::now();
            let seeds: Vec<_> = candidates
                .iter()
                .map(|c| (c.record.id, c.seed_score))
                .collect();
            let expansion =
                self.graph
                    .expand_eligible(&seeds, namespace, query.graph_depth, eligible)?;
            diagnostics.graph = expansion.work;
            let existing: HashSet<_> = candidates.iter().map(|c| c.record.id).collect();
            let mut discovered = Vec::new();
            for hit in expansion.hits {
                if existing.contains(&hit.id) {
                    continue;
                }
                let Some(record) = self.meta_store.get(&hit.id).await? else {
                    continue;
                };
                if !Self::passes_hard_filters(&record, query, namespace)
                    || record.phase == DecayPhase::Ghost
                {
                    continue;
                }
                validate_record(&record)?;
                let mut c = Candidate::new(record);
                c.activation_score = Some(hit.activation);
                c.graph_depth = Some(hit.depth);
                c.effective_r = Self::compute_effective_r(&c.record);
                c.entity_score = crate::model::entity_overlap(&query.entities, &c.record.entities);
                discovered.push(c);
            }
            self.apply_boosts(&mut discovered, query);
            candidates.extend(discovered);
            timings.graph_expansion_us = stage_start.elapsed().as_micros() as u64;
        }
        let stage_start = Instant::now();
        // RIF is intentionally absent: v2 uses baseline formulas with no suppression.
        if mode == VersionMode::History || query.graph_depth > 0 {
            self.resolve_versions2(candidates, query, namespace, mode, diagnostics)
                .await?;
        }
        for c in candidates.iter_mut() {
            if !c.composite_score.is_finite() {
                return Err(SearchError::Internal("nonfinite final score".into()));
            }
            if c.composite_score == 0.0 {
                c.composite_score = 0.0;
            } // Canonical positive zero.
        }
        candidates.sort_by(rank);
        timings.rank_us += stage_start.elapsed().as_micros() as u64;
        Ok(())
    }
}
