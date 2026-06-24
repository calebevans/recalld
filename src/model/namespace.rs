//! Namespace configuration types and phase threshold definitions.

use serde::{Deserialize, Serialize};

use crate::model::constants::*;
use crate::model::id::NamespaceId;

// ═══════════════════════════════════════════════════════════════════════
// PhaseThresholds
// ═══════════════════════════════════════════════════════════════════════

/// Retrievability thresholds that govern phase transitions.
///
/// - `R > full_threshold` => Full (Phase 1)
/// - `summary_threshold < R <= full_threshold` => Summary (Phase 2)
/// - `ghost_threshold < R <= summary_threshold` => Ghost (Phase 3)
/// - `R <= ghost_threshold` => eligible for deletion
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseThresholds {
    /// Retrievability above this triggers Full phase.
    pub full_threshold: f32,
    /// Retrievability above this (below full) triggers Summary phase.
    pub summary_threshold: f32,
    /// Retrievability above this (below summary) triggers Ghost phase.
    pub ghost_threshold: f32,
}

impl PhaseThresholds {
    /// Validate that thresholds are ordered: full > summary > ghost > 0,
    /// and all are in (0.0, 1.0).
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.full_threshold <= 0.0 || self.full_threshold >= 1.0 {
            return Err("full_threshold must be in (0.0, 1.0)");
        }
        if self.summary_threshold <= 0.0 || self.summary_threshold >= self.full_threshold {
            return Err("summary_threshold must be in (0.0, full_threshold)");
        }
        if self.ghost_threshold <= 0.0 || self.ghost_threshold >= self.summary_threshold {
            return Err("ghost_threshold must be in (0.0, summary_threshold)");
        }
        Ok(())
    }

}

impl Default for PhaseThresholds {
    fn default() -> Self {
        Self {
            full_threshold: DEFAULT_FULL_THRESHOLD,
            summary_threshold: DEFAULT_SUMMARY_THRESHOLD,
            ghost_threshold: DEFAULT_GHOST_THRESHOLD,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// NamespaceConfig
// ═══════════════════════════════════════════════════════════════════════

/// Configuration for a memory namespace.
///
/// A namespace is an isolated partition with its own embedding
/// dimensionality, decay parameters, and vector file. The `id` and
/// `embedding_dim` are immutable after creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceConfig {
    /// Sequential integer ID, starting at 1. Immutable.
    pub id: NamespaceId,

    /// Human-readable name. Must be unique across all namespaces.
    /// 1-64 chars, alphanumeric + hyphens + underscores.
    pub name: String,

    /// Embedding vector dimensionality. Immutable after creation.
    pub embedding_dim: u32,

    /// Initial FSRS stability for new memories (days).
    pub initial_stability: f32,

    /// Default FSRS difficulty for new memories.
    /// Fixed at 5.0 for v1; included for forward compatibility.
    pub default_difficulty: f32,

    /// Phase transition thresholds.
    pub phase_thresholds: PhaseThresholds,

    /// Stability above which a memory becomes permastore (days).
    pub permastore_threshold: f32,

    /// When this namespace was created (millis since epoch).
    pub created_at: i64,

    /// Target retention rate for FSRS interval scheduling.
    pub desired_retention: f32,

    /// Namespace-specific decay rate multiplier.
    /// - None (default) = inherit from global [decay] config
    /// - Some(1.0) = normal decay
    /// - Some(0.0) = decay disabled for this namespace
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decay_rate_multiplier: Option<f32>,
}

impl NamespaceConfig {
    /// Create the default namespace (ID 1, "default", 1536 dims).
    pub fn default_namespace(now_millis: i64) -> Self {
        Self {
            id: NamespaceId::new(1),
            name: "default".to_string(),
            embedding_dim: DEFAULT_EMBEDDING_DIM,
            initial_stability: DEFAULT_INITIAL_STABILITY,
            default_difficulty: DEFAULT_DIFFICULTY,
            phase_thresholds: PhaseThresholds::default(),
            permastore_threshold: DEFAULT_PERMASTORE_THRESHOLD,
            created_at: now_millis,
            desired_retention: DEFAULT_DESIRED_RETENTION,
            decay_rate_multiplier: None,
        }
    }
}
