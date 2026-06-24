//! Edge type enum for directed relationships between memories.

use serde::{Deserialize, Serialize};

/// The type of directed relationship between two memories.
///
/// Direction semantics:
/// - `ParentChild`: source is parent, target is child.
/// - `Causal`: source caused/preceded target.
/// - `Contradicts` and `Associative`: direction is less meaningful but
///   preserved for consistency.
///
/// `repr(u8)` for single-byte storage in `edges.db` composite keys.
/// Discriminants start at 1; 0 is reserved as a sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum EdgeType {
    /// Hierarchical parent-child relationship.
    ParentChild = 1,
    /// Neutral topical relation.
    Associative = 2,
    /// Temporal or logical causation.
    Causal = 3,
    /// Conflicting information.
    Contradicts = 4,
    /// Shared named entity (person, place, organization).
    Entity = 5,
    /// Temporal co-occurrence within a time window.
    Temporal = 6,
    /// Source supersedes (replaces/updates) target.
    Supersedes = 7,
}

impl EdgeType {
    /// Convert from on-disk `u8` discriminant.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(EdgeType::ParentChild),
            2 => Some(EdgeType::Associative),
            3 => Some(EdgeType::Causal),
            4 => Some(EdgeType::Contradicts),
            5 => Some(EdgeType::Entity),
            6 => Some(EdgeType::Temporal),
            7 => Some(EdgeType::Supersedes),
            _ => None,
        }
    }

    /// Return the `u8` discriminant for on-disk storage.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}
