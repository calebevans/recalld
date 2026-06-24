//! Configuration for the decay engine.
//!
//! [`DecayConfig`] collects all tunable parameters that can vary per
//! namespace. Non-tunable constants (DECAY, FACTOR, w8-w10) live in
//! `fsrs.rs` and are not exposed here -- they are trained values that
//! should not be modified without retraining.

use super::fsrs::{
    DEFAULT_DIFFICULTY, DEFAULT_INITIAL_STABILITY, DEFAULT_MAX_CONNECTION_BONUS,
    DEFAULT_PARTIAL_ACCESS_WEIGHT, DEFAULT_PERMASTORE_THRESHOLD,
    DEFAULT_PHASE_1_THRESHOLD, DEFAULT_PHASE_2_THRESHOLD, DEFAULT_PHASE_3_THRESHOLD,
    STABILITY_CEILING, STABILITY_FLOOR,
};

/// All tunable parameters for the FSRS decay engine.
///
/// These can be configured per namespace. The FSRS curve constants
/// (DECAY, FACTOR, w8, w9, w10) are intentionally excluded -- they are
/// trained on 500M+ reviews and should not be modified without
/// retraining the model.
///
/// # Defaults
///
/// All fields have sensible defaults derived from FSRS v4.5 and the
/// Recalld design spec. Construct with `DecayConfig::default()`.
#[derive(Debug, Clone)]
pub struct DecayConfig {
    /// Initial stability for new memories, in days.
    ///
    /// How long a newly created memory lasts before its retrievability
    /// drops to 90%. Higher values make memories decay more slowly from
    /// the start.
    ///
    /// Default: 3.7145 (FSRS v4.5 "Good" rating).
    /// Typical range: 0.5 (ephemeral) to 30.0 (persistent).
    pub initial_stability: f32,

    /// Fixed difficulty for all memories in this namespace.
    ///
    /// FSRS D parameter, range [1.0, 10.0]. Lower values make memories
    /// strengthen faster on access. Fixed at 5.0 for v1 since Recalld
    /// has no user ratings to derive per-item difficulty.
    ///
    /// Default: 5.0 (population midpoint).
    pub difficulty: f32,

    /// Retrievability threshold for Phase 1 (Full).
    ///
    /// When effective_R drops to this value or below, the memory
    /// transitions from Full to Summary and full_text is deleted.
    ///
    /// Default: 0.70.
    pub phase_1_threshold: f32,

    /// Retrievability threshold for Phase 2 (Summary).
    ///
    /// When effective_R drops to this value or below, the memory
    /// transitions from Summary to Ghost and the summary is deleted.
    ///
    /// Default: 0.30.
    pub phase_2_threshold: f32,

    /// Retrievability threshold for Phase 3 (Ghost).
    ///
    /// When effective_R drops to this value or below, the memory
    /// is eligible for deletion. Embedding and relationships are
    /// removed.
    ///
    /// Default: 0.05.
    pub phase_3_threshold: f32,

    /// Stability threshold for permastore status, in days.
    ///
    /// Once a memory's stability crosses this threshold, it is
    /// permanently exempt from decay sweeps. The flag is set on the
    /// access that causes the crossing and is never automatically
    /// cleared.
    ///
    /// Default: 1500.0 (~4.1 years).
    pub permastore_threshold: f32,

    /// Maximum connection bonus from spreading activation.
    ///
    /// Caps the effective retrievability boost that graph connections
    /// can provide. Range: [0.0, 1.0].
    ///
    /// Default: 0.15 (15 percentage points maximum).
    pub max_connection_bonus: f32,

    /// Partial access weight for associative (neighbor) retrievals.
    ///
    /// Scales the SInc multiplier for memories accessed as graph
    /// neighbors rather than direct search results. Range: [0.0, 1.0].
    ///
    /// Default: 0.5 (half the reinforcement of direct access).
    pub partial_access_weight: f32,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            initial_stability: DEFAULT_INITIAL_STABILITY,
            difficulty: DEFAULT_DIFFICULTY,
            phase_1_threshold: DEFAULT_PHASE_1_THRESHOLD,
            phase_2_threshold: DEFAULT_PHASE_2_THRESHOLD,
            phase_3_threshold: DEFAULT_PHASE_3_THRESHOLD,
            permastore_threshold: DEFAULT_PERMASTORE_THRESHOLD,
            max_connection_bonus: DEFAULT_MAX_CONNECTION_BONUS,
            partial_access_weight: DEFAULT_PARTIAL_ACCESS_WEIGHT,
        }
    }
}

impl DecayConfig {
    /// Validate that all config values are within legal ranges.
    ///
    /// Returns `Err` with a descriptive message if any value is invalid.
    /// Called at namespace creation and config update time.
    pub fn validate(&self) -> Result<(), String> {
        if self.initial_stability < STABILITY_FLOOR
            || self.initial_stability > STABILITY_CEILING
        {
            return Err(format!(
                "initial_stability must be in [{STABILITY_FLOOR}, {STABILITY_CEILING}], \
                 got {}",
                self.initial_stability
            ));
        }
        if self.difficulty < 1.0 || self.difficulty > 10.0 {
            return Err(format!(
                "difficulty must be in [1.0, 10.0], got {}",
                self.difficulty
            ));
        }
        if self.phase_1_threshold <= self.phase_2_threshold {
            return Err(format!(
                "phase_1_threshold ({}) must be > phase_2_threshold ({})",
                self.phase_1_threshold, self.phase_2_threshold
            ));
        }
        if self.phase_2_threshold <= self.phase_3_threshold {
            return Err(format!(
                "phase_2_threshold ({}) must be > phase_3_threshold ({})",
                self.phase_2_threshold, self.phase_3_threshold
            ));
        }
        if self.phase_3_threshold < 0.0 || self.phase_3_threshold >= 1.0 {
            return Err(format!(
                "phase_3_threshold must be in [0.0, 1.0), got {}",
                self.phase_3_threshold
            ));
        }
        if self.permastore_threshold < 1.0 {
            return Err(format!(
                "permastore_threshold must be >= 1.0 day, got {}",
                self.permastore_threshold
            ));
        }
        if self.max_connection_bonus < 0.0 || self.max_connection_bonus > 1.0 {
            return Err(format!(
                "max_connection_bonus must be in [0.0, 1.0], got {}",
                self.max_connection_bonus
            ));
        }
        if self.partial_access_weight < 0.0 || self.partial_access_weight > 1.0 {
            return Err(format!(
                "partial_access_weight must be in [0.0, 1.0], got {}",
                self.partial_access_weight
            ));
        }
        Ok(())
    }
}
