//! Decay phase enum and phase transition logic based on FSRS retrievability.

use serde::{Deserialize, Serialize};

use crate::model::namespace::PhaseThresholds;

/// The degradation phase of a memory, determining what content is retained.
///
/// Phases form a one-way ratchet during decay sweeps: Full -> Summary ->
/// Ghost -> deletion. Access (which increases strength) can promote a
/// memory back up, but in practice this is rare.
///
/// `Tombstone` is a terminal phase used for user-initiated deletion:
/// content is stripped and the memory is removed from all search indexes,
/// but the graph node and edges are preserved so that relationship chains
/// remain intact for spreading activation traversal.
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
    /// Content stripped, removed from search indexes, but graph node
    /// and edges preserved for relationship chain continuity.
    Tombstone = 4,
}

impl DecayPhase {
    /// Determine the correct phase for a given retrievability value,
    /// using the provided thresholds.
    pub fn from_strength(strength: f32, thresholds: &PhaseThresholds) -> Self {
        // NOTE: Tombstone is never derived from strength — it is set
        // explicitly by delete_memory. This function only maps the
        // natural decay progression: Full -> Summary -> Ghost.
        if strength > thresholds.full_to_summary {
            DecayPhase::Full
        } else if strength > thresholds.summary_to_ghost {
            DecayPhase::Summary
        } else {
            // Anything at or below summary_to_ghost but above deletion
            // threshold (ghost_to_delete) is Ghost. The caller handles
            // deletion for values below ghost_to_delete.
            DecayPhase::Ghost
        }
    }

    /// Convert from the on-disk `u8` discriminant.
    /// Returns `None` for unrecognized values (including sentinel 0).
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(DecayPhase::Full),
            2 => Some(DecayPhase::Summary),
            3 => Some(DecayPhase::Ghost),
            4 => Some(DecayPhase::Tombstone),
            _ => None,
        }
    }

    /// Return the `u8` discriminant for on-disk storage.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

}

impl Default for DecayPhase {
    /// New memories start in Full phase.
    fn default() -> Self {
        DecayPhase::Full
    }
}
