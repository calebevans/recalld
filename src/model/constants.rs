//! Crate-wide numeric constants for validation, decay, and storage limits.

// ── Timestamp ────────────────────────────────────────────────────────
/// Milliseconds per day, used for FSRS time conversions.
pub const MILLIS_PER_DAY: f64 = 86_400_000.0;

// ── FSRS Forgetting Curve ────────────────────────────────────────────
/// Power-law factor: 19.0 / 81.0 ≈ 0.2346.
/// From the FSRS v4.5 formulation: R(t,S) = (1 + FACTOR * t/S)^DECAY.
pub const FSRS_FACTOR: f64 = 19.0 / 81.0;

/// Power-law exponent (negative).
pub const FSRS_DECAY: f64 = -0.5;

// ── Stability ────────────────────────────────────────────────────────
/// Global stability floor (days). Prevents division-by-zero in the
/// forgetting curve. See Spec 01 §1.4.
pub const STABILITY_FLOOR: f32 = 0.01;

/// Global stability ceiling (days). ~100 years.
pub const STABILITY_CEILING: f32 = 36_500.0;

/// Default initial stability for new memories (FSRS v4.5 w2 for
/// a "Good" first rating).
pub const DEFAULT_INITIAL_STABILITY: f32 = 3.7145;

/// Default permastore threshold (days). Memories whose stability
/// exceeds this are exempt from decay sweeps.
pub const DEFAULT_PERMASTORE_THRESHOLD: f32 = 1_500.0;

// ── Difficulty ───────────────────────────────────────────────────────
/// Minimum FSRS difficulty value.
pub const DIFFICULTY_MIN: f32 = 1.0;
/// Maximum FSRS difficulty value.
pub const DIFFICULTY_MAX: f32 = 10.0;
/// Default FSRS difficulty for new memories.
pub const DEFAULT_DIFFICULTY: f32 = 5.0;

// ── Phase Thresholds (defaults) ──────────────────────────────────────
/// Retrievability above this = Phase 1 (Full).
pub const DEFAULT_FULL_THRESHOLD: f32 = 0.7;
/// Retrievability above this (below full) = Phase 2 (Summary).
pub const DEFAULT_SUMMARY_THRESHOLD: f32 = 0.3;
/// Retrievability above this (below summary) = Phase 3 (Ghost).
/// Below this = deletion.
pub const DEFAULT_GHOST_THRESHOLD: f32 = 0.05;

// ── Content Limits ───────────────────────────────────────────────────
/// Maximum byte length of `summary` (UTF-8).
pub const SUMMARY_MAX_BYTES: usize = 2_000;
/// Maximum byte length of `full_text` (UTF-8). 1 MiB.
pub const FULL_TEXT_MAX_BYTES: usize = 1_048_576;

// ── Tag Limits ───────────────────────────────────────────────────────
/// Maximum number of tags per memory.
pub const MAX_TAGS: usize = 64;
/// Maximum byte length of a single tag (UTF-8).
pub const TAG_MAX_BYTES: usize = 128;

// ── Access History ───────────────────────────────────────────────────
/// Maximum number of `AccessEvent` entries retained per memory.
pub const ACCESS_HISTORY_MAX: usize = 32;

// ── Desired Retention ────────────────────────────────────────────────
/// Default desired retention rate for FSRS interval scheduling.
pub const DEFAULT_DESIRED_RETENTION: f32 = 0.9;

// ── Namespace ────────────────────────────────────────────────────────
/// Maximum byte length of a namespace name.
pub const NAMESPACE_NAME_MAX_BYTES: usize = 64;

/// Default embedding dimensionality (OpenAI text-embedding-3-small).
pub const DEFAULT_EMBEDDING_DIM: u32 = 1536;

// ── DiskRecord ───────────────────────────────────────────────────────
/// Current on-disk schema version.
pub const DISK_RECORD_VERSION: u8 = 1;
