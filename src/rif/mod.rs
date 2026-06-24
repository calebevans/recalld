//! Retrieval-Induced Forgetting (RIF) subsystem.
//!
//! When a memory is retrieved, moderately-activated neighbors have their
//! FSRS stability reduced (suppression). Highly-activated neighbors
//! receive a small stability boost (integration). Based on the
//! Nonmonotonic Plasticity Hypothesis (Ritvo et al., 2019) and the
//! SAMPL model.

pub mod activation;
pub mod config;
pub mod engine;
pub mod metrics;
pub mod plasticity;

// Re-export the public API.
pub use activation::{calculate_activation, rif_edge_factor};
pub use config::RifConfig;
pub use engine::{NeighborInfo, QueryRifContext, RifEngine, StabilityUpdate};
pub use metrics::{RifMetrics, RifMetricsSnapshot};
pub use plasticity::{classify_regime, plasticity_multiplier, ActivationRegime};
