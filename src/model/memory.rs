//! Memory record types: the public API representation, access events,
//! and access kind classification.

use serde::{Deserialize, Serialize};

use crate::model::constants::*;
use crate::model::decay::DecayPhase;
use crate::model::error::ValidationError;
use crate::model::id::MemoryId;
use crate::model::tag::Tag;

// ═══════════════════════════════════════════════════════════════════════
// AccessKind
// ═══════════════════════════════════════════════════════════════════════

/// How a memory was accessed. Influences FSRS stability update magnitude.
///
/// - `DirectRetrieval` and `ManualReinforcement` apply the full FSRS
///   SInc stability multiplier.
/// - `AssociativeRetrieval` applies 50% of SInc (weaker reinforcement).
/// - `DecaySweep` does not update stability at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    /// Returned as a direct search result.
    DirectRetrieval,
    /// Returned as a related memory via graph traversal.
    AssociativeRetrieval,
    /// Read during a decay sweep (not returned to user).
    DecaySweep,
    /// Explicitly strengthened by the caller.
    ManualReinforcement,
}

// ═══════════════════════════════════════════════════════════════════════
// AccessEvent
// ═══════════════════════════════════════════════════════════════════════

/// A timestamped record of a single memory access.
///
/// 12 bytes on-disk: 8 (timestamp) + 1 (kind) + 3 (padding/alignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessEvent {
    /// Milliseconds since Unix epoch (UTC).
    pub timestamp: i64,
    /// How the memory was accessed.
    pub kind: AccessKind,
}

impl AccessEvent {
    /// Create a new access event at the current time.
    pub fn now(kind: AccessKind) -> Self {
        Self {
            timestamp: chrono::Utc::now().timestamp_millis(),
            kind,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Memory — API representation
// ═══════════════════════════════════════════════════════════════════════

/// The public API representation of a memory record.
///
/// Serializes to/from JSON with `camelCase` field names. Optional fields
/// are omitted from serialization when `None` (not sent as `null`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    /// Unique identifier (UUID v7).
    pub id: MemoryId,
    /// Resolved namespace name (not the numeric ID).
    pub namespace: String,
    /// Milliseconds since Unix epoch.
    pub created_at: i64,
    /// Milliseconds since Unix epoch.
    pub last_accessed_at: i64,
    /// Short description, always present, max 2000 bytes.
    pub summary: String,

    /// Full content. Present only in Phase 1 (Full).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,

    /// Validated tags attached to this memory.
    pub tags: Vec<Tag>,
    /// Current decay phase.
    pub phase: DecayPhase,
    /// Raw FSRS retrievability R, in [0.0, 1.0].
    pub strength: f32,
    /// Effective retrievability including connection bonus, in [0.0, 1.0].
    pub decay_strength: f32,
    /// FSRS stability S in days.
    pub stability: f32,
    /// FSRS difficulty D, fixed at 5.0 for v1.
    pub difficulty: f32,
    /// True if stability exceeds the permastore threshold.
    pub is_permastore: bool,
    /// Cached count of outgoing edges.
    pub edge_count: u16,

    /// Embedding vector. Included only when explicitly requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,

    /// Access history. Included only in detailed/debug responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_history: Option<Vec<AccessEvent>>,
}

impl Memory {
    /// Validate all invariants on an existing Memory struct.
    ///
    /// Returns the first violation found. Call this after deserialization
    /// or manual construction to ensure the record is internally
    /// consistent. This does NOT validate namespace existence or
    /// embedding dimensions (those require external context).
    pub fn validate(&self) -> Result<(), ValidationError> {
        // Summary non-empty
        if self.summary.is_empty() {
            return Err(ValidationError::SummaryEmpty);
        }

        // Summary length
        if self.summary.len() > SUMMARY_MAX_BYTES {
            return Err(ValidationError::SummaryTooLong {
                len: self.summary.len(),
                max: SUMMARY_MAX_BYTES,
            });
        }

        // Full text length
        if let Some(ref ft) = self.full_text {
            if ft.len() > FULL_TEXT_MAX_BYTES {
                return Err(ValidationError::FullTextTooLong {
                    len: ft.len(),
                    max: FULL_TEXT_MAX_BYTES,
                });
            }
        }

        // Tag count
        if self.tags.len() > MAX_TAGS {
            return Err(ValidationError::TooManyTags {
                count: self.tags.len(),
                max: MAX_TAGS,
            });
        }

        // Strength range
        if !(0.0..=1.0).contains(&self.strength) {
            return Err(ValidationError::StrengthOutOfRange(self.strength));
        }

        // Decay strength range
        if !(0.0..=1.0).contains(&self.decay_strength) {
            return Err(ValidationError::DecayStrengthOutOfRange(
                self.decay_strength,
            ));
        }

        // Stability range
        if !(STABILITY_FLOOR..=STABILITY_CEILING).contains(&self.stability) {
            return Err(ValidationError::StabilityOutOfRange {
                value: self.stability,
                min: STABILITY_FLOOR,
                max: STABILITY_CEILING,
            });
        }

        // Difficulty range
        if !(DIFFICULTY_MIN..=DIFFICULTY_MAX).contains(&self.difficulty) {
            return Err(ValidationError::DifficultyOutOfRange {
                value: self.difficulty,
                min: DIFFICULTY_MIN,
                max: DIFFICULTY_MAX,
            });
        }

        // Timestamp ordering
        if self.created_at > self.last_accessed_at {
            return Err(ValidationError::TimestampOrdering {
                created: self.created_at,
                accessed: self.last_accessed_at,
            });
        }

        Ok(())
    }
}

