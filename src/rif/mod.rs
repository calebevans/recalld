//! Retrieval-Induced Forgetting (RIF) subsystem.
//!
//! When a memory is retrieved, moderately-activated neighbors have their
//! FSRS stability reduced (suppression). Highly-activated neighbors
//! receive a small stability boost (integration). Based on the
//! Nonmonotonic Plasticity Hypothesis (Ritvo et al., 2019) and the
//! SAMPL model (Sievers & Momennejad, 2019).
//!
//! References:
//! - Anderson, M. C., Bjork, R. A., & Bjork, E. L. (1994). "Remembering
//!   can cause forgetting: Retrieval dynamics in long-term memory."
//!   J. Exp. Psychol: Learning, Memory, and Cognition, 20, 1063-1087.
//! - Ritvo, V. J. H., Turk-Browne, N. B., & Norman, K. A. (2019).
//!   "Nonmonotonic Plasticity: How Memory Retrieval Drives Learning."
//!   Trends in Cognitive Sciences, 23(9), 726-742.
//! - Murayama, K., Miyatsu, T., Buchli, D., & Storm, B. C. (2014).
//!   "Forgetting as a consequence of retrieval: A meta-analytic review
//!   of retrieval-induced forgetting." Psychological Bulletin, 140(5),
//!   1383-1409.

/// Activation score calculation for neighbors.
pub mod activation;
/// Configuration for the RIF subsystem.
pub mod config;
/// RIF engine orchestration and per-query context.
pub mod engine;
/// Lock-free counters for RIF health monitoring.
pub mod metrics;
/// Nonmonotonic plasticity function mapping activation to stability multipliers.
pub mod plasticity;

// Re-export the public API.
pub use activation::{calculate_activation, rif_edge_factor};
pub use config::RifConfig;
pub use engine::{NeighborInfo, QueryRifContext, RifEngine, StabilityUpdate};
pub use metrics::{RifMetrics, RifMetricsSnapshot};
pub use plasticity::{ActivationRegime, classify_regime, plasticity_multiplier};
