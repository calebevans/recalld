//! Decay phase enum and phase transition logic based on FSRS retrievability.

use serde::{Deserialize, Serialize};

use crate::model::namespace::PhaseThresholds;

/// The degradation phase of a memory, determining what content is retained.
///
/// Phases form a one-way ratchet during decay sweeps: Full -> Summary ->
/// Ghost -> deletion. Access (which increases strength) can promote a
/// memory back up, but in practice this is rare.
///
/// `repr(u8)` for single-byte on-disk storage. Discriminants start at 1
/// so that 0 serves as a sentinel/padding value in binary formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum DecayPhase {
    /// Full text, embedding, tags, relationships preserved.
    Full = 1,
    /// Summary + embedding + tags + relationships; full_text removed.
    Summary = 2,
    /// Embedding + relationships only; summary removed.
    Ghost = 3,
}

impl DecayPhase {
    /// Determine the correct phase for a given retrievability value,
    /// using the provided thresholds.
    pub fn from_strength(strength: f32, thresholds: &PhaseThresholds) -> Self {
        if strength > thresholds.full_threshold {
            DecayPhase::Full
        } else if strength > thresholds.summary_threshold {
            DecayPhase::Summary
        } else {
            // Anything at or below summary_threshold but above deletion
            // threshold (ghost_threshold) is Ghost. The caller handles
            // deletion for values below ghost_threshold.
            DecayPhase::Ghost
        }
    }

    /// Determine the correct phase using the default thresholds.
    pub fn from_strength_default(strength: f32) -> Self {
        Self::from_strength(strength, &PhaseThresholds::default())
    }

    /// Convert from the on-disk `u8` discriminant.
    /// Returns `None` for unrecognized values (including sentinel 0).
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(DecayPhase::Full),
            2 => Some(DecayPhase::Summary),
            3 => Some(DecayPhase::Ghost),
            _ => None,
        }
    }

    /// Return the `u8` discriminant for on-disk storage.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// True if full text should be retained in this phase.
    pub fn retains_full_text(self) -> bool {
        matches!(self, DecayPhase::Full)
    }

    /// True if the summary should be retained in this phase.
    pub fn retains_summary(self) -> bool {
        matches!(self, DecayPhase::Full | DecayPhase::Summary)
    }
}

impl Default for DecayPhase {
    /// New memories start in Full phase.
    fn default() -> Self {
        DecayPhase::Full
    }
}
