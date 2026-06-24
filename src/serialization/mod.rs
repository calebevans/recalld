//! Serialization for Recalld memory records.
//!
//! Two formats:
//! - **Binary**: Version-prefixed custom format for `meta.db` on-disk storage.
//!   Optimized for compact size and fast encode/decode (<500 ns).
//! - **JSON**: serde-based camelCase format for the HTTP API wire protocol.
//!   Uses `serde_json` with skip-if-none and integer-millis timestamps.

use std::fmt;

mod binary;
mod json;

pub use binary::{decode_record, encode_record};
pub use json::{
    ApiError, ApiResponse, CreateMemoryRequest, MemoryResponse, NamespaceRequest,
    NamespaceResponse, PaginatedResponse, PaginationParams, SearchHit, SearchRequest,
    SearchResponse, UpdateMemoryRequest,
};

/// Magic bytes written at the start of every binary record for validation.
/// ASCII "CH" (Cold Harbor) -- 2 bytes.
pub(crate) const RECORD_MAGIC: [u8; 2] = [0x43, 0x48];

/// Current binary schema version.
pub const CURRENT_SCHEMA_VERSION: u8 = 1;

/// Maximum supported schema version this build can decode.
pub const MAX_SUPPORTED_VERSION: u8 = 1;

/// Size of the fixed-length portion of a v1 binary record, in bytes.
/// Breakdown:
///   magic(2) + version(1) + id(16) + namespace_id(4) + created_at(8)
///   + last_accessed_at(8) + phase(1) + strength(4) + decay_strength(4)
///   + stability(4) + difficulty(4) + is_permastore(1) + vector_slot(4)
///   + edge_count(2) + text_offset(8) + text_length(4)
///   = 75 bytes
pub(crate) const V1_FIXED_SIZE: usize = 75;

// ═══════════════════════════════════════════════════════════════════════
// DecodeError
// ═══════════════════════════════════════════════════════════════════════

/// Errors that can occur when decoding a binary record from bytes.
///
/// Note: This `serialization::DecodeError` is intentionally separate from
/// CS-01's `model::DecodeError`. This one covers the full binary format
/// with magic bytes and stricter validation (e.g., `InvalidMagic`,
/// `InvalidBool`, `TooManyTags`, `NonFiniteFloat`), while CS-01's covers
/// the simpler version-only decoder in `record.rs`.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodeError {
    /// Input is shorter than the minimum record size.
    /// Contains the actual length and the minimum expected.
    Truncated { actual: usize, expected: usize },

    /// The magic bytes at the start of the record do not match `RECORD_MAGIC`.
    InvalidMagic { found: [u8; 2] },

    /// The schema version byte is higher than `MAX_SUPPORTED_VERSION`.
    UnsupportedVersion { found: u8, max_supported: u8 },

    /// The `phase` byte does not map to a valid `DecayPhase` variant.
    InvalidPhase { byte: u8 },

    /// The `is_permastore` byte is not 0 or 1.
    InvalidBool { field: &'static str, byte: u8 },

    /// A variable-length field's declared length would read past the
    /// end of the input buffer.
    FieldOverflow {
        field: &'static str,
        declared_len: usize,
        available: usize,
    },

    /// A string field contains invalid UTF-8.
    InvalidUtf8 { field: &'static str },

    /// An `AccessKind` byte does not map to a known variant.
    InvalidAccessKind { byte: u8 },

    /// The tag count prefix exceeds the maximum allowed (64).
    TooManyTags { count: u16 },

    /// The access history count prefix exceeds the maximum allowed (32).
    TooManyAccessEvents { count: u16 },

    /// A floating-point field contains NaN or infinity.
    NonFiniteFloat { field: &'static str, value: f32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { actual, expected } => {
                write!(
                    f,
                    "record truncated: {actual} bytes, need at least {expected}"
                )
            }
            Self::InvalidMagic { found } => {
                write!(
                    f,
                    "invalid magic bytes: expected [0x43, 0x48], got [{:#04x}, {:#04x}]",
                    found[0], found[1]
                )
            }
            Self::UnsupportedVersion {
                found,
                max_supported,
            } => {
                write!(
                    f,
                    "unsupported schema version {found} (max supported: {max_supported})"
                )
            }
            Self::InvalidPhase { byte } => {
                write!(f, "invalid decay phase byte: {byte}")
            }
            Self::InvalidBool { field, byte } => {
                write!(
                    f,
                    "invalid boolean for '{field}': expected 0 or 1, got {byte}"
                )
            }
            Self::FieldOverflow {
                field,
                declared_len,
                available,
            } => {
                write!(
                    f,
                    "field '{field}' declares {declared_len} bytes but only {available} remain"
                )
            }
            Self::InvalidUtf8 { field } => {
                write!(f, "field '{field}' contains invalid UTF-8")
            }
            Self::InvalidAccessKind { byte } => {
                write!(f, "invalid AccessKind byte: {byte}")
            }
            Self::TooManyTags { count } => {
                write!(f, "tag count {count} exceeds maximum of 64")
            }
            Self::TooManyAccessEvents { count } => {
                write!(f, "access event count {count} exceeds maximum of 32")
            }
            Self::NonFiniteFloat { field, value } => {
                write!(f, "non-finite float in '{field}': {value}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

// ═══════════════════════════════════════════════════════════════════════
// EncodeError
// ═══════════════════════════════════════════════════════════════════════

/// Errors that can occur when encoding a record to binary bytes.
/// Encoding errors are less common than decode errors because the
/// Rust type system already prevents many invalid states. These
/// cover the remaining edge cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// The summary field exceeds the u16 length prefix capacity (65,535 bytes).
    SummaryTooLong { len: usize },

    /// A single tag exceeds the u16 length prefix capacity.
    TagTooLong { index: usize, len: usize },

    /// Total serialized tag payload exceeds u16 capacity.
    TagPayloadTooLong { len: usize },

    /// Too many tags (> 64).
    TooManyTags { count: usize },

    /// Too many access events (> 32).
    TooManyAccessEvents { count: usize },

    /// A floating-point field contains NaN or infinity.
    NonFiniteFloat { field: &'static str },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SummaryTooLong { len } => {
                write!(f, "summary is {len} bytes, exceeds u16 max (65535)")
            }
            Self::TagTooLong { index, len } => {
                write!(f, "tag at index {index} is {len} bytes, exceeds u16 max")
            }
            Self::TagPayloadTooLong { len } => {
                write!(f, "total tag payload is {len} bytes, exceeds u16 max")
            }
            Self::TooManyTags { count } => {
                write!(f, "tag count {count} exceeds maximum of 64")
            }
            Self::TooManyAccessEvents { count } => {
                write!(f, "access event count {count} exceeds maximum of 32")
            }
            Self::NonFiniteFloat { field } => {
                write!(f, "non-finite float in field '{field}'")
            }
        }
    }
}

impl std::error::Error for EncodeError {}
