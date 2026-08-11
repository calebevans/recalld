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
/// Maximum length of `summary`, measured in UTF-8 **bytes**, not
/// characters. ASCII text gets 2,000 characters; text containing em
/// dashes, curly quotes, accents, or emoji gets fewer, because those
/// characters occupy 2-4 bytes each.
///
/// The limit exists because the on-disk record encodes the summary with
/// a `u16` length prefix (see `DiskRecord::to_bytes` in
/// [`crate::model::record`]); it must stay well below `u16::MAX`.
pub const SUMMARY_MAX_BYTES: usize = 2_000;
/// Maximum length of `full_text`, measured in UTF-8 **bytes**, not
/// characters. 1 MiB = 1,048,576 bytes.
///
/// This bounds the *raw* text, measured before JSON escaping. A request
/// carrying the maximum `full_text` can serialize to considerably more
/// than 1 MiB on the wire (`\n` doubles, control bytes expand up to 6x
/// as `\u00XX`), which is why the daemon's frame limit sits well above
/// it — see [`crate::daemon::protocol::MAX_MESSAGE_SIZE`].
pub const FULL_TEXT_MAX_BYTES: usize = 1_048_576;

// ── Tag Limits ───────────────────────────────────────────────────────
/// Maximum number of tags per memory.
pub const MAX_TAGS: usize = 64;
/// Maximum length of a single tag, measured in UTF-8 **bytes**, not
/// characters.
pub const TAG_MAX_BYTES: usize = 128;
/// Maximum byte length of a single entity, topic, or emotion label
/// (UTF-8). Bounds the worst-case serialized request size; see
/// [`crate::daemon::protocol`].
///
/// This is the ceiling; the effective per-field limits are
/// [`ENTITY_MAX_BYTES`], [`TOPIC_MAX_BYTES`] and [`EMOTION_MAX_BYTES`],
/// which additionally reserve room for the derived tag prefix.
pub const LABEL_MAX_BYTES: usize = 128;

// ── Derived tag prefixes ─────────────────────────────────────────────
//
// `entities`, `topics` and `emotions` are merged into `tags` as
// `<prefix><lowercased label>` *after* validation. `Tag::new` then
// rejects anything longer than `TAG_MAX_BYTES`, and the merge sites
// dropped such tags without a word. Reserving the prefix in the label
// limit is what makes a request that validates impossible to truncate
// later.

/// Prefix of the tag derived from an `entities` label.
pub const ENTITY_TAG_PREFIX: &str = "entity/";
/// Prefix of the tag derived from a `topics` label.
pub const TOPIC_TAG_PREFIX: &str = "topic/";
/// Prefix of the tag derived from an `emotions` label.
pub const EMOTION_TAG_PREFIX: &str = "emotion/";

/// The label budget left by `prefix`: never more than
/// [`LABEL_MAX_BYTES`], and never enough to push `<prefix><label>` past
/// [`TAG_MAX_BYTES`].
///
/// Lowercasing can in principle change a string's byte length, but only
/// for characters [`Tag::new`](crate::model::Tag::new) rejects outright
/// (its alphabet is `[a-z0-9/_.:-]`), so comparing the raw label's
/// length here is sound.
const fn label_max_bytes(prefix: &str) -> usize {
    let room = TAG_MAX_BYTES - prefix.len();
    if room < LABEL_MAX_BYTES {
        room
    } else {
        LABEL_MAX_BYTES
    }
}

/// Maximum byte length of a single `entities` label (UTF-8).
pub const ENTITY_MAX_BYTES: usize = label_max_bytes(ENTITY_TAG_PREFIX);
/// Maximum byte length of a single `topics` label (UTF-8).
pub const TOPIC_MAX_BYTES: usize = label_max_bytes(TOPIC_TAG_PREFIX);
/// Maximum byte length of a single `emotions` label (UTF-8).
pub const EMOTION_MAX_BYTES: usize = label_max_bytes(EMOTION_TAG_PREFIX);

// ── Structured Metadata Limits ───────────────────────────────────────
/// Maximum number of `entities` accepted on a single store request.
pub const MAX_ENTITIES: usize = 32;
/// Maximum number of `topics` accepted on a single store request.
pub const MAX_TOPICS: usize = 32;
/// Maximum number of `emotions` accepted on a single store request.
pub const MAX_EMOTIONS: usize = 32;

// ── Batch Limits ─────────────────────────────────────────────────────
/// Maximum number of memories accepted in a single batch store call.
pub const MAX_BATCH_MEMORIES: usize = 100;

// ── Near-Duplicate Detection ─────────────────────────────────────────
/// Cosine similarity at or above which two memories count as
/// near-duplicates of each other.
///
/// One number, three surfaces: the `find_similar_memories` scan mode, the
/// REST duplicate scan, and the advisory a store returns. They have to
/// agree, or "duplicate" means two different things inside one namespace.
///
/// Declared `f64`, not `f32`, and that is load-bearing: this value is
/// embedded in published JSON schemas via `json!`, and `serde_json` widens
/// an `f32` to `f64` on the way in — `0.85f32` serializes as
/// `0.8500000238418579`, silently changing the schema clients read. `f64`
/// also matches [`crate::config::GraphConfig::auto_link_threshold`] and the
/// `.as_f64()` used to parse the override. Cast to `f32` at the use sites.
pub const DUPLICATE_SIMILARITY_THRESHOLD: f64 = 0.85;

/// Maximum near-duplicate matches reported on a single store.
///
/// Three, not one: one match hides "this is the fourth restatement of the
/// same fact". Three shows a cluster forming and is still cheap.
pub const MAX_NEAR_DUPLICATE_MATCHES: usize = 3;

/// Byte budget for each reported near-duplicate's summary.
///
/// Summaries are capped at [`SUMMARY_MAX_BYTES`], so three untruncated ones
/// would be a 6 KB tax on a store response.
pub const NEAR_DUPLICATE_SUMMARY_MAX_BYTES: usize = 240;

// ── Listing Limits ───────────────────────────────────────────────────
//
// Named because two places have to agree about each one: the `default`
// and `maximum` a tool publishes in its JSON schema, and the bound the
// argument parser enforces. When those drift the schema stops describing
// the server, and a client that trusts the schema starts getting errors
// it was told could not happen. A schema test asserts the pairing.

/// Default page size of a `list_memories` call.
pub const DEFAULT_LIST_LIMIT: usize = 50;
/// Largest page size a `list_memories` call may ask for.
pub const MAX_LIST_LIMIT: usize = 200;
/// Default number of labels returned per bucket by `list_tags`.
pub const DEFAULT_TAG_LIMIT: usize = 50;
/// Largest number of labels per bucket a `list_tags` call may ask for.
pub const MAX_TAG_LIMIT: usize = 500;

// ── Access History ───────────────────────────────────────────────────
/// Maximum number of `AccessEvent` entries retained per memory.
pub const ACCESS_HISTORY_MAX: usize = 32;

// ── Desired Retention ────────────────────────────────────────────────
/// Default desired retention rate for FSRS interval scheduling.
pub const DEFAULT_DESIRED_RETENTION: f32 = 0.9;

// ── Namespace ────────────────────────────────────────────────────────
/// Maximum length of a namespace name, measured in UTF-8 **bytes**, not
/// characters. Namespace names are restricted to ASCII (they are
/// filesystem-adjacent), so for valid names bytes and characters agree.
pub const NAMESPACE_NAME_MAX_BYTES: usize = 64;

/// Default embedding dimensionality (OpenAI text-embedding-3-small).
pub const DEFAULT_EMBEDDING_DIM: u32 = 1536;

// ── DiskRecord ───────────────────────────────────────────────────────
/// Current on-disk schema version.
pub const DISK_RECORD_VERSION: u8 = 1;
