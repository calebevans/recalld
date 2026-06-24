//! Configuration for Retrieval-Induced Forgetting.
//!
//! [`RifConfig`] holds all tunable parameters for the RIF subsystem.
//! Defaults are calibrated to match the meta-analytic RIF effect size
//! of ~8.7% recall reduction (Murayama et al., 2014).

use serde::{Deserialize, Serialize};

/// Configuration for Retrieval-Induced Forgetting.
///
/// All thresholds and magnitudes are tunable per namespace.
/// Defaults are calibrated to match the meta-analytic RIF
/// effect size of ~8.7% recall reduction (Murayama et al., 2014).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RifConfig {
    /// Master switch. When `false`, `RifEngine::compute_effects` returns
    /// an empty `Vec` immediately. Default: `true`.
    pub enabled: bool,

    /// Discount factor for activation propagation per hop.
    /// Activation at hop `d` is multiplied by `gamma.powf(d)`.
    /// SAMPL best-fit value. Range: [0.0, 1.0]. Default: `0.3`.
    pub gamma: f32,

    /// Lower activation threshold. Below this, no plasticity effect.
    /// Default: `0.10`.
    pub activation_low: f32,

    /// Upper activation threshold. Above this, strengthening occurs.
    /// Default: `0.45`.
    pub activation_high: f32,

    /// Peak stability reduction fraction in the suppression band.
    /// Applied at the parabolic center of the MODERATE regime.
    /// `0.15` means the multiplier dips to `1.0 - 0.15 = 0.85` at center.
    /// Default: `0.15`.
    pub max_suppression: f32,

    /// Peak stability increase fraction for highly co-activated neighbors.
    /// Capped low because the direct retrieval already gives an FSRS SInc
    /// boost. Default: `0.05`.
    pub max_enhancement: f32,

    /// Maximum graph traversal depth. At `gamma = 0.3`, hop-3 activation
    /// is `0.027 * (other factors)` — negligible. Default: `2`.
    pub max_hops: u32,

    /// Hard floor on stability after RIF. Prevents a single RIF event
    /// from destroying a memory. In days. Default: `0.5`.
    pub stability_floor: f32,

    /// Safety cap on the number of neighbors evaluated per single
    /// retrieved memory. Neighbors are sorted by activation (highest
    /// first); processing stops at this cap. Default: `100`.
    pub max_neighbors: usize,

    /// Maximum cumulative stability reduction applied to any single
    /// neighbor within one query. Prevents compound RIF from
    /// multi-result queries. `0.75` means no more than 25%
    /// reduction per query. Default: `0.75`.
    pub max_reduction_per_query: f32,
}

impl Default for RifConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gamma: 0.3,
            activation_low: 0.10,
            activation_high: 0.45,
            max_suppression: 0.15,
            max_enhancement: 0.05,
            max_hops: 2,
            stability_floor: 0.5,
            max_neighbors: 100,
            max_reduction_per_query: 0.75,
        }
    }
}

impl RifConfig {
    /// Validates invariants. Call after deserialization from user config.
    /// Returns `Err` with a human-readable message on failure.
    pub fn validate(&self) -> Result<(), String> {
        if self.gamma < 0.0 || self.gamma > 1.0 {
            return Err(format!("gamma must be in [0.0, 1.0], got {}", self.gamma));
        }
        if self.activation_low < 0.0 || self.activation_low > 1.0 {
            return Err(format!(
                "activation_low must be in [0.0, 1.0], got {}",
                self.activation_low
            ));
        }
        if self.activation_high < 0.0 || self.activation_high > 1.0 {
            return Err(format!(
                "activation_high must be in [0.0, 1.0], got {}",
                self.activation_high
            ));
        }
        if self.activation_low >= self.activation_high {
            return Err(format!(
                "activation_low ({}) must be less than activation_high ({})",
                self.activation_low, self.activation_high
            ));
        }
        if self.max_suppression < 0.0 || self.max_suppression > 1.0 {
            return Err(format!(
                "max_suppression must be in [0.0, 1.0], got {}",
                self.max_suppression
            ));
        }
        if self.max_enhancement < 0.0 || self.max_enhancement > 1.0 {
            return Err(format!(
                "max_enhancement must be in [0.0, 1.0], got {}",
                self.max_enhancement
            ));
        }
        if self.max_hops == 0 {
            return Err("max_hops must be >= 1".to_string());
        }
        if self.stability_floor <= 0.0 {
            return Err(format!(
                "stability_floor must be > 0.0, got {}",
                self.stability_floor
            ));
        }
        if self.max_neighbors == 0 {
            return Err("max_neighbors must be >= 1".to_string());
        }
        if self.max_reduction_per_query <= 0.0 || self.max_reduction_per_query > 1.0 {
            return Err(format!(
                "max_reduction_per_query must be in (0.0, 1.0], got {}",
                self.max_reduction_per_query
            ));
        }
        Ok(())
    }
}
