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

/// Errors from `Memory::validate()` and `CreateMemory` validation.
///
/// Each variant carries a machine-readable `code()` suitable for the
/// JSON error response `"error"` field, plus a human-readable message
/// via `Display`.
#[derive(Debug, Error)]
pub enum ValidationError {
    /// Summary field is empty.
    #[error("summary must not be empty")]
    SummaryEmpty,

    /// Summary exceeds the maximum byte length.
    #[error("summary is {len} bytes, max is {max}")]
    SummaryTooLong { len: usize, max: usize },

    /// Full text exceeds the maximum byte length.
    #[error("full_text is {len} bytes, max is {max}")]
    FullTextTooLong { len: usize, max: usize },

    /// Too many tags on a single memory.
    #[error("too many tags: {count}, max is {max}")]
    TooManyTags { count: usize, max: usize },

    /// A tag failed validation.
    #[error("invalid tag: {source}")]
    InvalidTag {
        #[from]
        source: TagError,
    },

    /// Embedding dimensionality does not match the namespace.
    #[error("expected {expected} embedding dimensions for namespace \
             '{namespace}', got {actual}")]
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
    #[error("namespace name must be 1-{max} characters, \
             alphanumeric/hyphens/underscores")]
    InvalidNamespaceName { max: usize },
}

impl ValidationError {
    /// Machine-readable error code for JSON API responses.
    pub fn code(&self) -> &'static str {
        match self {
            Self::SummaryEmpty => "INVALID_SUMMARY",
            Self::SummaryTooLong { .. } => "SUMMARY_TOO_LONG",
            Self::FullTextTooLong { .. } => "FULL_TEXT_TOO_LONG",
            Self::TooManyTags { .. } => "TOO_MANY_TAGS",
            Self::InvalidTag { .. } => "INVALID_TAG",
            Self::DimensionMismatch { .. } => "DIMENSION_MISMATCH",
            Self::NonFiniteEmbedding { .. } => "INVALID_EMBEDDING",
            Self::NamespaceNotFound { .. } => "NAMESPACE_NOT_FOUND",
            Self::InvalidStability { .. } => "INVALID_STABILITY",
            Self::StrengthOutOfRange(_) => "INVALID_STRENGTH",
            Self::DecayStrengthOutOfRange(_) => "INVALID_DECAY_STRENGTH",
            Self::StabilityOutOfRange { .. } => "INVALID_STABILITY",
            Self::DifficultyOutOfRange { .. } => "INVALID_DIFFICULTY",
            Self::TimestampOrdering { .. } => "INVALID_TIMESTAMPS",
            Self::InvalidNamespaceName { .. } => "INVALID_NAMESPACE_NAME",
        }
    }

    /// The field name this error pertains to, for JSON `"field"` key.
    pub fn field(&self) -> &'static str {
        match self {
            Self::SummaryEmpty | Self::SummaryTooLong { .. } => "summary",
            Self::FullTextTooLong { .. } => "full_text",
            Self::TooManyTags { .. } | Self::InvalidTag { .. } => "tags",
            Self::DimensionMismatch { .. } | Self::NonFiniteEmbedding { .. } => {
                "embedding"
            }
            Self::NamespaceNotFound { .. } | Self::InvalidNamespaceName { .. } => {
                "namespace"
            }
            Self::InvalidStability { .. } => "initial_stability",
            Self::StrengthOutOfRange(_) => "strength",
            Self::DecayStrengthOutOfRange(_) => "decay_strength",
            Self::StabilityOutOfRange { .. } => "stability",
            Self::DifficultyOutOfRange { .. } => "difficulty",
            Self::TimestampOrdering { .. } => "timestamps",
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
    #[error("record is truncated: expected at least {expected} bytes, \
             got {actual}")]
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
    #[error("variable-length field '{field}' declares {declared} bytes \
             but only {available} remain")]
    FieldOverflow {
        field: &'static str,
        declared: usize,
        available: usize,
    },
}
