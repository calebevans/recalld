//! Error types for validation, tag parsing, and binary decoding.

use thiserror::Error;

// ═══════════════════════════════════════════════════════════════════════
// TagError
// ═══════════════════════════════════════════════════════════════════════

/// Errors from `Tag::new()` validation.
#[derive(Debug, Error)]
pub enum TagError {
    /// Tag string is empty.
    #[error("tag must not be empty")]
    Empty,

    /// Tag exceeds the maximum byte length.
    #[error("tag is {len} bytes, max is {max}")]
    TooLong { len: usize, max: usize },

    /// Tag does not start with an alphanumeric character.
    #[error("tag must start with an alphanumeric character, got '{0}'")]
    InvalidStartChar(char),

    /// Tag contains a character outside the allowed set.
    #[error("tag contains disallowed character '{0}'")]
    InvalidChar(char),
}

// ═══════════════════════════════════════════════════════════════════════
// ValidationError
// ═══════════════════════════════════════════════════════════════════════

/// Returns a clarifying sentence when a value's byte length differs
/// from its character count, and an empty string otherwise.
///
/// Length limits in Recalld are measured in UTF-8 bytes. For pure ASCII
/// input that is indistinguishable from a character count, so the hint
/// would be noise; for text containing multi-byte characters it is the
/// whole explanation.
fn utf8_hint(bytes: usize, chars: usize) -> &'static str {
    if bytes == chars {
        ""
    } else {
        " Limits are measured in UTF-8 bytes, not characters: \
         non-ASCII characters such as em dashes and curly quotes \
         count as 2-4 bytes each."
    }
}

/// Errors from `Memory::validate()` and memory creation validation.
///
/// Each variant renders a human-readable message via `Display`. Variants
/// that pertain to a specific request field also expose that field's
/// camelCase wire name via [`ValidationError::field`], suitable for the
/// JSON error response `"field"` member.
#[derive(Debug, Error)]
pub enum ValidationError {
    /// Summary field is empty.
    #[error("summary must not be empty")]
    SummaryEmpty,

    /// A text field exceeds its maximum byte length.
    ///
    /// Carries both units so the message can explain the difference:
    /// the limit is on UTF-8 bytes, but callers usually think in
    /// characters.
    #[error(
        "{field} is {bytes} bytes ({chars} characters), which exceeds \
         the {max_bytes}-byte limit.{}",
        utf8_hint(*bytes, *chars)
    )]
    FieldTooLong {
        field: &'static str,
        bytes: usize,
        chars: usize,
        max_bytes: usize,
    },

    /// An array field has more items than allowed.
    #[error("{field} has {count} items, which exceeds the maximum of {max}")]
    TooManyItems {
        field: &'static str,
        count: usize,
        max: usize,
    },

    /// A tag failed validation.
    #[error("invalid tag: {source}")]
    InvalidTag {
        #[from]
        source: TagError,
    },

    /// Embedding dimensionality does not match the namespace.
    #[error(
        "expected {expected} embedding dimensions for namespace \
             '{namespace}', got {actual}"
    )]
    DimensionMismatch {
        expected: u32,
        actual: u32,
        namespace: String,
    },

    /// Embedding contains NaN or infinity.
    #[error("embedding contains non-finite value at index {index}")]
    NonFiniteEmbedding { index: usize },

    /// Namespace name not found in the registry.
    #[error("namespace '{name}' not found")]
    NamespaceNotFound { name: String },

    /// Initial stability value is outside the valid range.
    #[error("initial_stability {value} out of range [{min}, {max}]")]
    InvalidStability { value: f32, min: f32, max: f32 },

    /// Strength value is outside [0.0, 1.0].
    #[error("strength {0} out of range [0.0, 1.0]")]
    StrengthOutOfRange(f32),

    /// Decay strength value is outside [0.0, 1.0].
    #[error("decay_strength {0} out of range [0.0, 1.0]")]
    DecayStrengthOutOfRange(f32),

    /// Stability value is outside the valid range.
    #[error("stability {value} out of range [{min}, {max}]")]
    StabilityOutOfRange { value: f32, min: f32, max: f32 },

    /// Difficulty value is outside the valid range.
    #[error("difficulty {value} out of range [{min}, {max}]")]
    DifficultyOutOfRange { value: f32, min: f32, max: f32 },

    /// Created-at timestamp is after last-accessed-at timestamp.
    #[error("created_at ({created}) is after last_accessed_at ({accessed})")]
    TimestampOrdering { created: i64, accessed: i64 },

    /// Namespace name is invalid (empty, too long, or bad characters).
    #[error(
        "namespace name must be 1-{max} bytes (UTF-8) of ASCII \
             alphanumerics, hyphens, or underscores"
    )]
    InvalidNamespaceName { max: usize },

    /// Decay rate multiplier is negative.
    #[error("decay_rate_multiplier must be >= 0.0, got {value}")]
    InvalidDecayMultiplier { value: f32 },
}

impl ValidationError {
    /// The camelCase wire name of the request field this error concerns,
    /// or `None` for errors not attributable to a single input field.
    pub fn field(&self) -> Option<&'static str> {
        match self {
            ValidationError::SummaryEmpty => Some("summary"),
            ValidationError::FieldTooLong { field, .. }
            | ValidationError::TooManyItems { field, .. } => Some(field),
            ValidationError::InvalidTag { .. } => Some("tags"),
            ValidationError::DimensionMismatch { .. }
            | ValidationError::NonFiniteEmbedding { .. } => Some("embedding"),
            ValidationError::NamespaceNotFound { .. } => Some("namespace"),
            ValidationError::InvalidNamespaceName { .. } => Some("name"),
            ValidationError::InvalidStability { .. } => Some("initialStability"),
            ValidationError::InvalidDecayMultiplier { .. } => Some("decayRateMultiplier"),
            ValidationError::StrengthOutOfRange(_)
            | ValidationError::DecayStrengthOutOfRange(_)
            | ValidationError::StabilityOutOfRange { .. }
            | ValidationError::DifficultyOutOfRange { .. }
            | ValidationError::TimestampOrdering { .. } => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// DecodeError
// ═══════════════════════════════════════════════════════════════════════

/// Errors from `DiskRecord::from_bytes()` deserialization.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// Record data is shorter than the minimum expected size.
    #[error(
        "record is truncated: expected at least {expected} bytes, \
             got {actual}"
    )]
    Truncated { expected: usize, actual: usize },

    /// Schema version is not recognized.
    #[error("unknown schema version {0}")]
    UnknownVersion(u8),

    /// Decay phase discriminant is not a valid variant.
    #[error("invalid decay phase discriminant {0}")]
    InvalidPhase(u8),

    /// A string field contains invalid UTF-8.
    #[error("invalid UTF-8 in field '{field}': {source}")]
    InvalidUtf8 {
        field: &'static str,
        source: std::string::FromUtf8Error,
    },

    /// A length-prefixed field declares more bytes than remain.
    #[error(
        "variable-length field '{field}' declares {declared} bytes \
             but only {available} remain"
    )]
    FieldOverflow {
        field: &'static str,
        declared: usize,
        available: usize,
    },
}
