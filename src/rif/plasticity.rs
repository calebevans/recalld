//! Nonmonotonic plasticity function for RIF.
//!
//! Maps activation levels to stability multipliers following the
//! U-shaped curve from the Nonmonotonic Plasticity Hypothesis
//! (Ritvo, Turk-Browne, & Norman, 2019).

use serde::{Deserialize, Serialize};

use crate::rif::config::RifConfig;

/// The regime of the nonmonotonic plasticity curve an activation score falls into.
///
/// Used for logging, metrics, and test assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationRegime {
    /// Below `activation_low`: no effect.
    Low,
    /// Between `activation_low` and `activation_high`: suppression (RIF).
    Moderate,
    /// Above `activation_high`: strengthening (integration).
    High,
}

/// Determine which regime an activation score falls into.
pub fn classify_regime(activation: f32, config: &RifConfig) -> ActivationRegime {
    if activation < config.activation_low {
        ActivationRegime::Low
    } else if activation < config.activation_high {
        ActivationRegime::Moderate
    } else {
        ActivationRegime::High
    }
}

/// The continuous nonmonotonic plasticity function.
///
/// Maps an activation level to a stability multiplier:
/// - `< 1.0` means weakening (RIF)
/// - `= 1.0` means no change
/// - `> 1.0` means strengthening
///
/// The shape is the U-curve from the Nonmonotonic Plasticity Hypothesis
/// (Ritvo, Turk-Browne, & Norman, 2019), implemented as a continuous
/// piecewise function with smooth transitions at band boundaries.
///
/// ## Regimes
///
/// | Activation range                    | Regime   | Effect        |
/// |-------------------------------------|----------|---------------|
/// | `[0.0, activation_low)`             | LOW      | `1.0` (none)  |
/// | `[activation_low, activation_high)` | MODERATE | `< 1.0` (RIF) |
/// | `[activation_high, 1.0]`            | HIGH     | `> 1.0` (strengthen) |
///
/// In the MODERATE band, a parabolic dip centered at
/// `(activation_low + activation_high) / 2` produces peak suppression
/// of `1.0 - max_suppression` at the center and tapers to `1.0` at
/// both edges.
pub fn plasticity_multiplier(activation: f32, config: &RifConfig) -> f32 {
    let low = config.activation_low;
    let high = config.activation_high;

    if activation < low {
        // LOW regime: no change.
        1.0
    } else if activation < high {
        // MODERATE regime: parabolic suppression dip.
        //
        // Center of the dip is the midpoint of the suppression band.
        // `normalized` maps [low, high] -> [-1.0, 1.0] with 0.0 at center.
        // `dip_depth` is 1.0 at center, 0.0 at edges (inverted parabola).
        let dip_center = (low + high) / 2.0;
        let half_width = (high - low) / 2.0;
        let normalized = (activation - dip_center) / half_width;
        let dip_depth = 1.0 - normalized * normalized;
        1.0 - config.max_suppression * dip_depth
    } else {
        // HIGH regime: linear rise from 1.0 to 1.0 + max_enhancement.
        //
        // `normalized` maps [high, 1.0] -> [0.0, 1.0].
        // Clamped so activations above 1.0 (shouldn't happen, but
        // defensive) don't exceed max_enhancement.
        let range = 1.0 - high;
        if range <= 0.0 {
            // Edge case: activation_high == 1.0, no strengthening zone.
            1.0
        } else {
            let normalized = ((activation - high) / range).min(1.0);
            1.0 + config.max_enhancement * normalized
        }
    }
}
