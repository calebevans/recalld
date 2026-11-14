//! Internal admission/provenance observations, not readiness or wire contracts.
use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::{Candidate, FtsAvailability, SemanticsDiagnostics};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RetrievalChannel {
    Vector,
    Fts,
    Entity,
    Metadata,
    Graph,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChannelParticipation {
    #[default]
    NotRequested,
    /// The operation returned successfully; not an A1 readiness certificate.
    /// Source errors still fail v2 queries rather than returning this diagnostic.
    Completed,
}

/// Counts overlap across channels and must not be summed as unique matches.
/// None means unmeasured, not zero or exhausted. Source-level counts precede
/// downstream fusion/minScore/version resolution and final result truncation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelDiagnostics {
    pub participation: ChannelParticipation,
    /// Distinct IDs returned by the channel interface before union/deduplication.
    pub offered: Option<usize>,
    /// Eligible channel members entering the candidate pool after its cap.
    pub admitted: Option<usize>,
    pub examined: Option<usize>,
    pub eligible: Option<usize>,
    pub exhausted: Option<bool>,
    pub candidate_limited: Option<bool>,
    pub budget_exhausted: Option<bool>,
    pub returned_own: usize,
    pub returned_inherited: usize,
}

/// Evidence for one returned UUID. No predecessor, rejected, or foreign IDs.
/// Inherited channels describe all resolved predecessor evidence, including
/// evidence below the winning inherited floor; they are not additive scores.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultProvenance {
    pub own_channels: BTreeSet<RetrievalChannel>,
    pub inherited_channels: BTreeSet<RetrievalChannel>,
    pub own_vector_score: Option<f32>,
    pub own_fts_score: Option<f32>,
    pub own_fused_relevance: Option<f32>,
    pub own_graph_activation: Option<f32>,
    pub own_graph_depth: Option<u8>,
    /// Own baseline score including boosts, before supersession inheritance.
    /// None for an injected terminal with no independently admitted evidence.
    pub own_score: Option<f32>,
    pub inherited_floor: Option<f32>,
    pub final_score: f32,
}

impl ResultProvenance {
    pub(super) fn inherit(&mut self, predecessor: &Candidate) {
        self.inherited_channels
            .extend(predecessor.provenance.own_channels.iter().copied());
        self.inherited_channels
            .extend(predecessor.provenance.inherited_channels.iter().copied());
        self.inherited_floor = Some(
            self.inherited_floor
                .map_or(predecessor.composite_score, |floor| {
                    floor.max(predecessor.composite_score)
                }),
        );
    }
}

pub(super) fn empty_channels() -> BTreeMap<RetrievalChannel, ChannelDiagnostics> {
    use RetrievalChannel::*;
    [Vector, Fts, Entity, Metadata, Graph]
        .into_iter()
        .map(|channel| (channel, ChannelDiagnostics::default()))
        .collect()
}

impl SemanticsDiagnostics {
    pub(super) fn observe_channel(
        &mut self,
        channel: RetrievalChannel,
        offered: usize,
        admitted: usize,
    ) {
        if let Some(channels) = &mut self.channels {
            let observation = channels.get_mut(&channel).unwrap();
            observation.participation = ChannelParticipation::Completed;
            observation.offered = Some(offered);
            observation.admitted = Some(admitted);
        }
    }

    pub(super) fn observe_direct_channels(
        &mut self,
        candidates: &[Candidate],
        vector_offered: Option<usize>,
        entity_offered: Option<usize>,
        metadata_eligible: Option<usize>,
    ) {
        if self.channels.is_none() {
            return;
        }
        let admitted = |channel| {
            candidates
                .iter()
                .filter(|c| c.provenance.own_channels.contains(&channel))
                .map(|c| c.record.id)
                .collect::<HashSet<_>>()
                .len()
        };
        if let Some(offered) = vector_offered {
            self.observe_channel(
                RetrievalChannel::Vector,
                offered,
                admitted(RetrievalChannel::Vector),
            );
        }
        if let Some(offered) = entity_offered {
            self.observe_channel(
                RetrievalChannel::Entity,
                offered,
                admitted(RetrievalChannel::Entity),
            );
        }
        if self.fts.availability == FtsAvailability::Available {
            // Successful v2 FTS supplies measured coverage. Never manufacture it
            // from the number of matches in a legacy top-K response.
            if let Some(coverage) = &self.fts.coverage {
                let observation = ChannelDiagnostics {
                    participation: ChannelParticipation::Completed,
                    offered: Some(coverage.admitted),
                    admitted: Some(admitted(RetrievalChannel::Fts)),
                    examined: Some(coverage.examined),
                    eligible: Some(coverage.eligible),
                    exhausted: Some(coverage.exhausted),
                    candidate_limited: Some(coverage.candidate_limited),
                    budget_exhausted: Some(coverage.budget_exhausted),
                    ..Default::default()
                };
                self.channels
                    .as_mut()
                    .unwrap()
                    .insert(RetrievalChannel::Fts, observation);
            }
        }
        if let Some(eligible) = metadata_eligible {
            self.observe_channel(RetrievalChannel::Metadata, eligible, eligible);
            let metadata = self
                .channels
                .as_mut()
                .unwrap()
                .get_mut(&RetrievalChannel::Metadata)
                .unwrap();
            metadata.eligible = Some(eligible);
            metadata.exhausted = Some(true); // Canonical namespace enumeration completed.
            metadata.budget_exhausted = Some(false); // No early examination ceiling.
        }
    }

    pub(super) fn observe_returned(&mut self, candidates: &[Candidate]) {
        let (Some(channels), Some(provenance)) = (&mut self.channels, &mut self.provenance) else {
            return;
        };
        for observation in channels.values_mut() {
            observation.returned_own = 0;
            observation.returned_inherited = 0;
        }
        provenance.clear();
        for candidate in candidates {
            let mut evidence = candidate.provenance.clone();
            evidence.own_vector_score = candidate.raw_vector_score;
            evidence.own_fts_score = candidate.raw_fts_score;
            evidence.own_fused_relevance = candidate.relevance_score;
            evidence.own_graph_activation =
                evidence.own_graph_activation.or(candidate.activation_score);
            evidence.final_score = candidate.composite_score;
            for channel in &evidence.own_channels {
                channels.get_mut(channel).unwrap().returned_own += 1;
            }
            for channel in &evidence.inherited_channels {
                channels.get_mut(channel).unwrap().returned_inherited += 1;
            }
            provenance.insert(candidate.record.id, evidence);
        }
    }
}
