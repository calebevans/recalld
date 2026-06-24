//! Decay engine -- FSRS-based forgetting curve for Recalld memories.
//!
//! This module implements the adapted FSRS v4.5 algorithm that governs
//! how memories strengthen with use and fade with neglect. It is a pure
//! computation layer with no I/O dependencies.
//!
//! # Architecture
//!
//! - [`config::DecayConfig`] -- all tunable parameters with sensible defaults
//! - [`fsrs::FsrsEngine`] -- stateless calculator: retrievability, stability
//!   growth, phase classification, connection bonus
//! - [`DecayState`] -- mutable per-memory state that the engine reads and writes
//! - [`sweep::DecaySweepRunner`] -- background task that iterates memories,
//!   computes effective retrievability, executes phase transitions, and deletes
//!   expired memories
//!
//! The sweep runner calls into the FSRS engine but is defined separately to
//! keep I/O concerns out of the math.

pub mod config;
pub mod fsrs;
pub mod storage_adapter;
pub mod sweep;

pub use config::DecayConfig;
pub use fsrs::FsrsEngine;
pub use sweep::{
    DecaySweepRunner, PhaseTransition, SweepConfig, SweepConfigError, SweepRecordError, SweepResult,
};

use crate::model::{AccessEvent, DecayPhase, MemoryId};

/// Mutable decay-related fields on a memory record.
///
/// These are the fields the FSRS engine reads and writes during access
/// processing and sweep evaluation. The storage layer is responsible for
/// persisting changes after mutation.
///
/// # Field Semantics
///
/// - `stability`: FSRS S parameter -- days until retrievability drops to 0.9.
///   Range: [0.01, 36500.0]. Updated only on access (never by the sweep).
/// - `strength`: Raw FSRS retrievability R, without connection bonus.
///   Recomputed lazily from (elapsed_days, stability). Range: [0.0, 1.0].
/// - `decay_strength`: Effective retrievability including connection bonus.
///   Computed during sweeps. Used for phase-transition decisions.
///   Range: [0.0, 1.0].
/// - `difficulty`: FSRS D parameter. Fixed at 5.0 for v1 (no user ratings).
/// - `phase`: Current degradation phase, derived from decay_strength thresholds.
/// - `is_permastore`: Once true, the memory is exempt from decay sweeps.
///   Set when stability crosses the permastore threshold. Never cleared
///   automatically.
/// - `last_accessed_at`: Milliseconds since Unix epoch. Updated on every
///   non-sweep access.
/// - `access_history`: Bounded ring buffer of recent access events (max 32).
///   Used for debugging and future FSRS personalization; not consumed by
///   v1 SInc calculations.
#[derive(Debug, Clone)]
pub struct DecayState {
    /// FSRS stability S in days.
    pub stability: f32,
    /// Raw FSRS retrievability R, in [0.0, 1.0].
    pub strength: f32,
    /// Effective retrievability including connection bonus, in [0.0, 1.0].
    pub decay_strength: f32,
    /// FSRS difficulty D, fixed at 5.0 for v1.
    pub difficulty: f32,
    /// Current decay phase.
    pub phase: DecayPhase,
    /// True if stability exceeds the permastore threshold.
    pub is_permastore: bool,
    /// Milliseconds since Unix epoch.
    pub last_accessed_at: i64,
    /// Bounded ring buffer of recent access events.
    pub access_history: Vec<AccessEvent>,
}

impl DecayState {
    /// Maximum number of access events retained in the history ring buffer.
    pub const MAX_ACCESS_HISTORY: usize = 32;

    /// Create a new DecayState for a freshly created memory.
    ///
    /// Uses the provided config for initial stability and difficulty.
    /// All retrievability fields start at 1.0 (just created = fully retrievable).
    pub fn new(config: &DecayConfig, now_millis: i64) -> Self {
        Self {
            stability: config.initial_stability,
            strength: 1.0,
            decay_strength: 1.0,
            difficulty: config.difficulty,
            phase: DecayPhase::Full,
            is_permastore: false,
            last_accessed_at: now_millis,
            access_history: Vec::new(),
        }
    }

    /// Push an access event into the bounded history ring buffer.
    ///
    /// If the buffer is full (>= MAX_ACCESS_HISTORY), the oldest entry
    /// is removed before pushing.
    pub fn push_access(&mut self, event: AccessEvent) {
        if self.access_history.len() >= Self::MAX_ACCESS_HISTORY {
            self.access_history.remove(0);
        }
        self.access_history.push(event);
    }
}

/// Events emitted by the FSRS engine when state changes occur.
///
/// The caller (sweep runner or access handler) is responsible for
/// acting on these events (e.g., deleting text, emitting metrics).
#[derive(Debug, Clone, PartialEq)]
pub enum DecayEvent {
    /// Stability crossed the permastore threshold on this access.
    PermastoreAchieved {
        /// The memory that achieved permastore status.
        /// May be `MemoryId::nil()` if the engine does not know the
        /// concrete identity; the caller must replace it.
        id: MemoryId,
    },
}
