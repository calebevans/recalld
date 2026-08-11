//! Edge type enum for directed relationships between memories.

use serde::{Deserialize, Serialize};

use super::id::MemoryId;

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

// ═══════════════════════════════════════════════════════════════════════
// Supersedes outcome
// ═══════════════════════════════════════════════════════════════════════

/// What actually happened to the `Supersedes` edge requested by a store.
///
/// A store that names a `supersedes` target is pre-validated *before*
/// anything is written, so a missing, cross-namespace, or self-referential
/// target fails the whole store instead of producing an outcome here. Every
/// variant below therefore describes a store that succeeded; the variant
/// says how durable and how new the edge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SupersedesStatus {
    /// The edge was created in the graph and persisted to `edges.db`.
    Applied,
    /// An edge of this type already connected the two memories. Supersedes
    /// is a desired end state rather than an event, so a repeat store is
    /// idempotent rather than an error.
    AlreadyApplied,
    /// The edge is live in the in-memory graph for this process, but
    /// writing it to `edges.db` failed. Recall honours it until restart.
    AppliedNotDurable,
    /// The edge could not be created at all. Only reachable if the target
    /// disappeared between pre-validation and the write.
    Failed,
}

/// The `supersedes` half of a store's result, echoed back to the caller.
///
/// Present only when the store named a `supersedes` target. Making the
/// outcome explicit is what keeps "silently succeeded while doing nothing"
/// unrepresentable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupersedesOutcome {
    /// The memory this store was asked to supersede.
    pub target: MemoryId,
    /// What happened to the edge.
    pub status: SupersedesStatus,
    /// Human-readable explanation, present for the non-`Applied` statuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SupersedesOutcome {
    /// Build an outcome with no detail message.
    pub fn new(target: MemoryId, status: SupersedesStatus) -> Self {
        Self {
            target,
            status,
            detail: None,
        }
    }

    /// Build an outcome carrying an explanatory detail message.
    pub fn with_detail(
        target: MemoryId,
        status: SupersedesStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            target,
            status,
            detail: Some(detail.into()),
        }
    }
}
