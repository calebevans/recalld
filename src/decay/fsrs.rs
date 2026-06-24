//! FSRS v4.5 forgetting curve engine adapted for Recalld.
//!
//! This module contains the pure mathematical core: retrievability
//! calculation, stability growth (SInc), connection bonus (spreading
//! activation), phase classification, and access processing.
//!
//! No I/O. No async. No storage dependencies.

use super::config::DecayConfig;
use super::{DecayEvent, DecayState};
use crate::model::{AccessEvent, AccessKind, DecayPhase, MemoryId};

// ── FSRS v4.5 Forgetting Curve Constants ────────────────────────────

/// FSRS v4.5 power-law decay exponent.
///
/// The forgetting curve follows R(t, S) = (1 + FACTOR * t / S) ^ DECAY.
/// The value -0.5 is the population-average exponent from the original
/// FSRS training on 700M+ Anki reviews. FSRS-6 makes this trainable
/// (w20), but Recalld uses the fixed v4.5 value since there is no
/// per-user data to personalize against.
///
/// Changing this constant invalidates all retrievability calculations
/// in the system. It is intentionally not configurable.
const DECAY: f32 = -0.5;

/// FSRS v4.5 scaling factor for the forgetting curve.
///
/// Derived from the defining constraint R(S, S) = 0.9:
///   0.9 = (1 + FACTOR) ^ -0.5
///   (1 + FACTOR) = 0.9 ^ (-2) = 100/81
///   FACTOR = 19/81 ~ 0.234568
///
/// This constant is algebraically coupled to DECAY. If DECAY changes,
/// FACTOR must be recomputed as: FACTOR = 0.9^(1/DECAY) - 1.
const FACTOR: f32 = 19.0 / 81.0;

// ── Stability Growth Parameters (w8, w9, w10) ──────────────────────

/// FSRS v4.5 base growth rate for stability increase on recall.
///
/// Controls the overall magnitude of the SInc multiplier. Appears in:
///   SInc = e^W8 * (11 - D) * S^(-W9) * (e^(W10 * (1-R)) - 1) + 1
///
/// e^1.6474 ~ 5.194. Combined with the difficulty factor (11 - D = 6.0
/// at D = 5.0), this produces a base multiplier of ~31.164.
///
/// Trained on 500M+ Anki reviews. Not user-configurable -- changing this
/// without retraining would produce miscalibrated stability growth.
const W8: f32 = 1.6474;

/// FSRS v4.5 stability damping exponent.
///
/// Creates diminishing returns as stability grows: S^(-0.1367) means
/// a memory with S = 100 days gets ~70% of the growth that a memory
/// with S = 1 day would get. This prevents runaway stability.
///
/// Appears as the exponent on S_old in the SInc formula.
const W9: f32 = 0.1367;

/// FSRS v4.5 spacing effect strength.
///
/// Controls how much more stability growth occurs when a memory is
/// accessed at low retrievability vs. high retrievability. This is the
/// mathematical heart of the spacing effect.
///
/// The term e^(W10 * (1 - R)) - 1 evaluates to:
///   - At R = 0.90 (just seen):       0.110
///   - At R = 0.70 (phase boundary):  0.365
///   - At R = 0.50 (half-forgotten):  0.688
///   - At R = 0.30 (nearly gone):     1.080
///
/// Accessing a nearly-forgotten memory (R = 0.30) strengthens it ~10x
/// more than accessing a fresh one (R = 0.90). This models the
/// empirical spacing effect (Cepeda et al. 2006).
const W10: f32 = 1.0461;

// ── Initial Stability Constants (w0-w3) ─────────────────────────────

/// FSRS v4.5 default initial stabilities for each rating grade.
///
/// In standard FSRS, the user rates their first encounter with a card
/// as Again (w0), Hard (w1), Good (w2), or Easy (w3). Each produces
/// a different initial stability.
///
/// Recalld uses only W2 (Good) as the default because storing a
/// memory is analogous to a student rating a card "Good" -- they've
/// seen and understood the content but haven't tested recall.
///
/// These constants are documented for completeness and for potential
/// future use (e.g., caller-specified initial confidence levels).
/// They are NOT used in any v1 computation except W2 as the default.
#[allow(dead_code)]
const W0_AGAIN: f32 = 0.4072;
#[allow(dead_code)]
const W1_HARD: f32 = 1.1829;
const W2_GOOD: f32 = 3.7145;
#[allow(dead_code)]
const W3_EASY: f32 = 15.6924;

/// Default initial stability for new memories, in days.
///
/// Corresponds to FSRS v4.5 "Good" rating (w2). A memory with this
/// stability drops to R = 0.90 after ~3.7 days, crosses the Phase 1
/// threshold (R = 0.70) at ~36 days, and reaches Ghost (R = 0.30)
/// at ~280 days.
///
/// Configurable per namespace via DecayConfig.initial_stability.
pub(super) const DEFAULT_INITIAL_STABILITY: f32 = W2_GOOD;

// ── Remaining FSRS v4.5 Default Parameters (w4-w19) ────────────────

/// FSRS v4.5 default parameters w4-w7 (difficulty model).
///
/// These control how difficulty is initialized and updated based on
/// user ratings. Recalld fixes D = 5.0 and does not use these,
/// but they are documented for completeness and potential future use.
///
/// w4: Initial difficulty for a "Good" rating = 5.0 (midpoint)
/// w5: Difficulty adjustment per rating step
/// w6: Difficulty reversion toward mean
/// w7: Difficulty update magnitude
#[allow(dead_code)]
const W4: f32 = 5.0;
#[allow(dead_code)]
const W5: f32 = 1.0;
#[allow(dead_code)]
const W6: f32 = 0.75;
#[allow(dead_code)]
const W7: f32 = 0.6;

/// FSRS v4.5 default parameters w11-w14.
///
/// w11-w13: Stability-after-failure formula (not used -- Recalld
///   has no failure signal; all accesses are treated as successful recall).
/// w14: Scaling factor in FSRS-5 difficulty update (not used in v4.5 mode).
#[allow(dead_code)]
const W11: f32 = 1.2;
#[allow(dead_code)]
const W12: f32 = 0.0;
#[allow(dead_code)]
const W13: f32 = 0.3261;
#[allow(dead_code)]
const W14: f32 = 0.0;

/// FSRS v4.5 default parameters w15-w19.
///
/// w15-w16: Grade-dependent SInc multipliers in FSRS-5 (not used --
///   Recalld has no grades).
/// w17: FSRS-5 failure penalty (not used).
/// w18-w19: Short-term stability parameters in FSRS-6 (not used).
#[allow(dead_code)]
const W15: f32 = 1.0;
#[allow(dead_code)]
const W16: f32 = 0.0;
#[allow(dead_code)]
const W17: f32 = 1.0;
#[allow(dead_code)]
const W18: f32 = 0.5;
#[allow(dead_code)]
const W19: f32 = 0.6;

// ── Recalld Application Constants ────────────────────────────────────

/// Fixed difficulty for all Recalld memories.
///
/// D = 5.0 is the midpoint of the FSRS 1-10 difficulty scale,
/// representing the population-average difficulty. With no user
/// ratings to adjust difficulty per-item, this produces the
/// "average case" stability growth.
///
/// Impact on SInc: the (11 - D) term evaluates to 6.0.
pub(super) const DEFAULT_DIFFICULTY: f32 = 5.0;

/// Phase 1 (Full) threshold -- R must be strictly above this for Phase 1.
///
/// When effective_R drops to 0.70 or below, the memory transitions
/// to Phase 2 (Summary) and full_text is deleted.
pub(super) const DEFAULT_PHASE_1_THRESHOLD: f32 = 0.70;

/// Phase 2 (Summary) threshold -- R must be strictly above this for Phase 2.
///
/// When effective_R drops to 0.30 or below, the memory transitions
/// to Phase 3 (Ghost) and the summary is deleted.
pub(super) const DEFAULT_PHASE_2_THRESHOLD: f32 = 0.30;

/// Phase 3 (Ghost) threshold -- R must be strictly above this for Phase 3.
///
/// When effective_R drops to 0.05 or below, the memory is eligible
/// for deletion. At this point only the embedding and relationships
/// remain.
pub(super) const DEFAULT_PHASE_3_THRESHOLD: f32 = 0.05;

/// Stability threshold for permastore status, in days.
///
/// ~4.1 years. Once stability crosses this threshold, the memory is
/// permanently exempt from decay sweeps. Derived from the observation
/// that 5 well-timed accesses produce stability of ~1,479 days
/// (Spec 02, Section 3.5). Rounded to 1,500 for a clean threshold.
///
/// Based on Bahrick (1984) permastore research: some memories, once
/// sufficiently overlearned, resist forgetting for 25+ years.
pub(super) const DEFAULT_PERMASTORE_THRESHOLD: f32 = 1500.0;

/// Maximum connection bonus from spreading activation.
///
/// The connection bonus can add at most 15 percentage points to
/// effective retrievability. This prevents highly-connected memories
/// from becoming immune to decay.
///
/// Derived from ACT-R spreading activation theory (Anderson 1993).
pub(super) const DEFAULT_MAX_CONNECTION_BONUS: f32 = 0.15;

/// Maximum associative strength between two memories.
///
/// In the ACT-R formula S_ji = S_MAX - ln(fan_j + 1), this controls
/// how much a focused (low-fan) neighbor can contribute. A neighbor
/// with fan = 1 contributes S_MAX - ln(2) = 1.307 base strength.
///
/// The effective fan cutoff is fan >= e^S_MAX - 1. With S_MAX = 2.0,
/// neighbors with 7+ connections contribute nothing (fan effect).
pub(super) const S_MAX: f32 = 2.0;

/// Partial access weight for associative (neighbor) retrievals.
///
/// When a memory is accessed as a graph neighbor rather than a direct
/// search result, the SInc multiplier is reduced:
///   SInc_partial = 1.0 + (SInc_full - 1.0) * PARTIAL_ACCESS_WEIGHT
///
/// 0.5 means neighbor access provides half the stability reinforcement
/// of direct access. This models the cognitive finding that incidental
/// exposure strengthens memories less than intentional recall.
pub(super) const DEFAULT_PARTIAL_ACCESS_WEIGHT: f32 = 0.5;

/// Stability floor -- minimum allowed stability value, in days.
///
/// Prevents division-by-zero in the forgetting curve formula and
/// models extremely volatile memories. ~14 minutes.
///
/// Note: RIF (Retrieval-Induced Forgetting) has a separate, higher
/// floor of 0.5 days to prevent catastrophic single-event decay.
/// This asymmetry is intentional -- see Spec 01, Section 1.4.
pub(super) const STABILITY_FLOOR: f32 = 0.01;

/// Stability ceiling -- maximum allowed stability value, in days.
///
/// ~100 years. Memories above the permastore threshold (~1,500 days)
/// are exempt from sweeps, but stability can continue growing to
/// reflect true long-term reinforcement.
pub(super) const STABILITY_CEILING: f32 = 36500.0;

// ── FsrsEngine ──────────────────────────────────────────────────────

/// Stateless FSRS calculator.
///
/// Holds a reference to the active [`DecayConfig`] for the namespace
/// being processed. All methods are deterministic given the same inputs.
///
/// # Thread Safety
///
/// `FsrsEngine` is `Send + Sync`. It contains no mutable state -- all
/// mutation happens on the `DecayState` passed by the caller.
pub struct FsrsEngine<'a> {
    /// Active configuration for the namespace being processed.
    pub config: &'a DecayConfig,
}

impl<'a> FsrsEngine<'a> {
    /// Create a new engine bound to the given configuration.
    pub fn new(config: &'a DecayConfig) -> Self {
        Self { config }
    }

    /// Calculate the raw retrievability of a memory.
    ///
    /// Implements the FSRS v4.5 power-law forgetting curve:
    ///
    /// ```text
    /// R(t, S) = (1 + FACTOR * t / S) ^ DECAY
    /// ```
    ///
    /// # Arguments
    ///
    /// - `time_since_access_days`: Elapsed time since last access, in
    ///   fractional days. Must be >= 0.0. A value of 0.0 returns 1.0
    ///   (just-accessed memory has perfect retrievability).
    /// - `stability`: FSRS stability S, in days. Must be > 0.0.
    ///   Represents the number of days until R drops to 0.9.
    ///
    /// # Returns
    ///
    /// Retrievability in [0.0, 1.0]. The curve is monotonically
    /// decreasing in `t` and monotonically increasing in `S`.
    pub fn retrievability(&self, time_since_access_days: f32, stability: f32) -> f32 {
        debug_assert!(
            stability > 0.0,
            "stability must be positive, got {stability}"
        );
        debug_assert!(
            time_since_access_days >= 0.0,
            "elapsed days must be non-negative, got {time_since_access_days}"
        );

        if time_since_access_days == 0.0 {
            return 1.0;
        }

        (1.0 + FACTOR * time_since_access_days / stability).powf(DECAY)
    }

    /// Calculate the stability increase factor (SInc) for an access.
    ///
    /// Implements the FSRS v4.5 recall-stability formula:
    ///
    /// ```text
    /// SInc = e^w8 * (11 - D) * S^(-w9) * (e^(w10 * (1 - R)) - 1) + 1
    /// ```
    ///
    /// The spacing effect is captured by `w10`: lower R at access time
    /// produces dramatically higher SInc. New stability is computed as
    /// `S_new = S_old * SInc`.
    ///
    /// # Arguments
    ///
    /// - `stability`: Current FSRS stability S_old, in days. Must be > 0.0.
    /// - `retrievability`: Current R at the moment of access. Range [0.0, 1.0].
    /// - `difficulty`: FSRS difficulty D. Range [1.0, 10.0].
    ///
    /// # Returns
    ///
    /// SInc >= 1.0 always. Accessing a memory never weakens it. The
    /// return value is the multiplicative factor -- NOT the new stability.
    pub fn stability_increase(&self, stability: f32, retrievability: f32, difficulty: f32) -> f32 {
        let growth_base = W8.exp(); // e^1.6474 ~ 5.194
        let difficulty_factor = 11.0 - difficulty; // 6.0 at D = 5.0
        let stability_damping = stability.powf(-W9); // S^(-0.1367)
        let spacing_bonus = (W10 * (1.0 - retrievability)).exp() - 1.0;

        let sinc = growth_base * difficulty_factor * stability_damping * spacing_bonus + 1.0;

        // SInc must be >= 1.0 -- accessing never weakens a memory.
        // This guard handles edge cases where floating-point arithmetic
        // might produce a value slightly below 1.0.
        sinc.max(1.0)
    }

    /// Apply an access to a memory, updating its decay state in place.
    ///
    /// This is the primary state-mutation entry point. It:
    /// 1. Computes the current retrievability from elapsed time
    /// 2. Calculates SInc (stability increase factor)
    /// 3. Applies partial weight for associative retrievals
    /// 4. Multiplies stability by effective SInc
    /// 5. Clamps stability to [STABILITY_FLOOR, STABILITY_CEILING]
    /// 6. Resets strength to 1.0 (just accessed = fully retrievable)
    /// 7. Updates last_accessed_at
    /// 8. Pushes an AccessEvent to the history
    /// 9. Checks for permastore threshold crossing
    ///
    /// # Arguments
    ///
    /// - `state`: Mutable reference to the memory's decay state.
    /// - `access_kind`: How the memory was accessed. `DecaySweep` is
    ///   a no-op -- sweeps do not count as accesses.
    /// - `now_millis`: Current time as milliseconds since Unix epoch.
    ///
    /// # Returns
    ///
    /// The new stability value (post-SInc). Also returns a `DecayEvent`
    /// if permastore was achieved on this access.
    ///
    /// # DecaySweep Short-Circuit
    ///
    /// If `access_kind` is `DecaySweep`, this function returns
    /// immediately with the current stability unchanged and no event.
    /// Sweep encounters are not access events -- they do not update
    /// last_accessed_at or push to the access history.
    pub fn apply_access(
        &self,
        state: &mut DecayState,
        access_kind: AccessKind,
        now_millis: i64,
    ) -> (f32, Option<DecayEvent>) {
        // Sweep encounters are not accesses
        if access_kind == AccessKind::DecaySweep {
            return (state.stability, None);
        }

        // 1. Compute elapsed time in days
        let elapsed_millis = (now_millis - state.last_accessed_at).max(0) as f64;
        let elapsed_days = (elapsed_millis / 86_400_000.0) as f32;

        // 2. Current retrievability
        let current_r = self.retrievability(elapsed_days, state.stability);

        // 3. Full SInc
        let full_sinc = self.stability_increase(state.stability, current_r, state.difficulty);

        // 4. Apply partial weight for neighbor accesses
        let effective_sinc = match access_kind {
            AccessKind::DirectRetrieval | AccessKind::ManualReinforcement => full_sinc,
            AccessKind::AssociativeRetrieval => {
                1.0 + (full_sinc - 1.0) * self.config.partial_access_weight
            }
            AccessKind::DecaySweep => unreachable!(), // handled above
        };

        // 5. Update stability with clamping
        state.stability =
            (state.stability * effective_sinc).clamp(STABILITY_FLOOR, STABILITY_CEILING);

        // 6. Reset strength -- just accessed = fully retrievable
        state.strength = 1.0;
        state.decay_strength = 1.0;

        // 7. Update last_accessed_at
        state.last_accessed_at = now_millis;

        // 8. Push access event to history
        state.push_access(AccessEvent {
            timestamp: now_millis,
            kind: access_kind,
        });

        // 9. Check for permastore promotion
        let event = if state.stability >= self.config.permastore_threshold && !state.is_permastore {
            state.is_permastore = true;
            // Caller must fill in the real MemoryId -- the engine
            // does not know which memory this state belongs to.
            Some(DecayEvent::PermastoreAchieved {
                id: MemoryId::nil(),
            })
        } else {
            None
        };

        (state.stability, event)
    }

    /// Calculate the connection bonus for a memory based on its neighbors.
    ///
    /// Implements a simplified ACT-R spreading activation model:
    ///
    /// ```text
    /// bonus = clamp(sum_j(W * max(0, S_MAX - ln(fan_j + 1)) * R_j), 0, MAX_BONUS)
    /// W = 1 / num_neighbors
    /// ```
    ///
    /// Each neighbor's contribution depends on:
    /// - **Its retrievability R_j**: Dead neighbors (low R) don't help.
    /// - **Its fan / degree**: Focused connections (low degree) help more
    ///   than diffuse hubs (fan effect, Anderson & Reder 1999).
    /// - **Attentional weight W**: Fixed activation budget split equally
    ///   across all neighbors (ACT-R convention).
    ///
    /// # Arguments
    ///
    /// - `neighbors`: Slice of (neighbor_retrievability, neighbor_degree)
    ///   tuples. The degree is the total edge count of the neighbor node,
    ///   not just edges to this memory.
    ///
    /// # Returns
    ///
    /// Connection bonus in [0.0, config.max_connection_bonus]. Zero if
    /// the neighbor slice is empty.
    pub fn connection_bonus(&self, neighbors: &[(f32, usize)]) -> f32 {
        if neighbors.is_empty() {
            return 0.0;
        }

        let w = 1.0 / neighbors.len() as f32;
        let mut bonus = 0.0_f32;

        for &(r_j, fan_j) in neighbors {
            let assoc_strength = (S_MAX - ((fan_j + 1) as f32).ln()).max(0.0);
            bonus += w * assoc_strength * r_j;
        }

        bonus.clamp(0.0, self.config.max_connection_bonus)
    }

    /// Apply the connection bonus to a base retrievability.
    ///
    /// ```text
    /// effective_R = base_R + connection_bonus * (1 - base_R)
    /// ```
    ///
    /// This formula ensures:
    /// - The bonus can only increase R, never decrease it.
    /// - The bonus has diminishing effect as R approaches 1.0.
    /// - The result stays in [0.0, 1.0]: a memory with R = 0.0 and
    ///   max bonus (0.15) gets effective_R = 0.15, not 1.0.
    pub fn effective_retrievability(&self, base_r: f32, connection_bonus: f32) -> f32 {
        (base_r + connection_bonus * (1.0 - base_r)).clamp(0.0, 1.0)
    }

    /// Determine the target decay phase for a given effective retrievability.
    ///
    /// Uses the phase thresholds from the active config. The thresholds
    /// are exclusive lower bounds -- a memory must be STRICTLY ABOVE the
    /// threshold to remain in the higher phase.
    ///
    /// # Returns
    ///
    /// - `Some(DecayPhase::Full)` if effective_r > phase_1_threshold
    /// - `Some(DecayPhase::Summary)` if effective_r > phase_2_threshold
    /// - `Some(DecayPhase::Ghost)` if effective_r > phase_3_threshold
    /// - `None` if effective_r <= phase_3_threshold (deletable)
    pub fn target_phase(&self, effective_r: f32) -> Option<DecayPhase> {
        if effective_r > self.config.phase_1_threshold {
            Some(DecayPhase::Full)
        } else if effective_r > self.config.phase_2_threshold {
            Some(DecayPhase::Summary)
        } else if effective_r > self.config.phase_3_threshold {
            Some(DecayPhase::Ghost)
        } else {
            None // Deletable
        }
    }

    /// Calculate the number of days until retrievability drops to a threshold.
    ///
    /// Inverts the forgetting curve:
    ///
    /// ```text
    /// R = (1 + FACTOR * t / S) ^ DECAY
    /// t = S / FACTOR * (R^(1/DECAY) - 1)
    /// ```
    ///
    /// Useful for estimating when a memory will transition phases.
    pub fn days_until_threshold(&self, stability: f32, target_r: f32) -> f32 {
        debug_assert!(target_r > 0.0 && target_r < 1.0);
        stability / FACTOR * (target_r.powf(1.0 / DECAY) - 1.0)
    }
}
