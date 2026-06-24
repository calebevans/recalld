//! Unified error type for all storage modules.

use thiserror::Error;
use uuid::Uuid;

use crate::model::error::DecodeError;

/// Errors from storage operations (metadata, vectors, text, edges).
#[derive(Debug, Error)]
pub enum StorageError {
    // ── redb errors (from metadata.rs and edges.rs) ──────────────────
    /// A generic redb error.
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),

    /// A redb database-level error.
    #[error("redb database error: {0}")]
    RedbDatabase(#[from] redb::DatabaseError),

    /// A redb table operation error.
    #[error("redb table error: {0}")]
    RedbTable(#[from] redb::TableError),

    /// A redb transaction error.
    #[error("redb transaction error: {0}")]
    RedbTransaction(#[from] redb::TransactionError),

    /// A redb storage-level error.
    #[error("redb storage error: {0}")]
    RedbStorage(#[from] redb::StorageError),

    /// A redb commit error.
    #[error("redb commit error: {0}")]
    RedbCommit(#[from] redb::CommitError),

    // ── Metadata errors ──────────────────────────────────────────────
    /// A record with this ID already exists.
    #[error("duplicate memory ID: {0}")]
    DuplicateId(Uuid),

    /// No record exists for the given ID.
    #[error("memory not found: {0}")]
    NotFound(Uuid),

    /// A namespace with this name already exists.
    #[error("duplicate namespace name: {0}")]
    DuplicateName(String),

    /// No namespace exists for the given ID.
    #[error("namespace not found: {0}")]
    NamespaceNotFound(u32),

    /// A secondary index is corrupt or inconsistent.
    #[error("corrupt index: {0}")]
    CorruptIndex(String),

    /// Failed to deserialize a stored value.
    #[error("deserialization error: {0}")]
    Deserialize(String),

    /// Failed to serialize a value for storage.
    #[error("serialization error: {0}")]
    Serialize(String),

    /// Failed to decode a record from its binary representation.
    #[error("decode error: {0}")]
    Decode(#[from] DecodeError),

    /// The database directory is locked by another process.
    #[error("database is locked by another process")]
    DatabaseLocked,

    /// An invalid decay phase discriminant was encountered.
    #[error("invalid decay phase: {0}")]
    InvalidPhase(u8),

    // ── I/O errors ───────────────────────────────────────────────────
    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // ── Text storage errors ──────────────────────────────────────────
    /// The on-disk entry length does not match the expected length.
    #[error("stored entry length mismatch at offset {offset}: expected {expected}, found {found}")]
    TextLengthMismatch {
        /// Byte offset in fulltext.dat.
        offset: u64,
        /// Expected payload length from meta.db.
        expected: u32,
        /// Actual length stored in the entry header.
        found: u32,
    },

    /// The CRC32 checksum of the text payload does not match.
    #[error(
        "CRC32 mismatch at offset {offset}: expected {expected:#010x}, computed {computed:#010x}"
    )]
    TextCrcMismatch {
        /// Byte offset in fulltext.dat.
        offset: u64,
        /// CRC32 stored in the entry header.
        expected: u32,
        /// CRC32 computed from the payload bytes.
        computed: u32,
    },

    /// The text payload is not valid UTF-8.
    #[error("invalid UTF-8 at offset {offset}: {source}")]
    InvalidUtf8 {
        /// Byte offset in fulltext.dat.
        offset: u64,
        /// The underlying UTF-8 conversion error.
        source: std::string::FromUtf8Error,
    },

    /// The text payload exceeds the maximum allowed size.
    #[error("text payload too large: {size} bytes, max {max}")]
    TextTooLarge {
        /// Actual payload size in bytes.
        size: usize,
        /// Maximum allowed size in bytes.
        max: usize,
    },

    /// Attempted to read using the `TextRef::NONE` sentinel.
    #[error("attempted to read TextRef::NONE sentinel")]
    InvalidTextRef,

    /// Attempted to append an empty text payload.
    #[error("attempted to append empty text")]
    EmptyText,

    /// The file header could not be parsed.
    #[error("file header could not be parsed: {file}")]
    HeaderParseError {
        /// Name of the file with the unparseable header.
        file: &'static str,
    },

    /// The file has invalid magic bytes.
    #[error("invalid magic bytes in {file}: expected {expected:?}, found {found:?}")]
    InvalidMagic {
        /// Name of the file with invalid magic.
        file: &'static str,
        /// Expected magic bytes.
        expected: [u8; 4],
        /// Actual magic bytes found.
        found: [u8; 4],
    },

    /// The file format version is not supported.
    #[error("unsupported version in {file}: expected {expected}, found {found}")]
    UnsupportedVersion {
        /// Name of the file with the unsupported version.
        file: &'static str,
        /// Expected version number.
        expected: u16,
        /// Actual version number found.
        found: u16,
    },

    /// The parent directory path is missing or invalid.
    #[error("parent directory path is missing or invalid")]
    InvalidPath,

    // ── Edge storage errors ──────────────────────────────────────────
    /// An edge key has an unexpected byte length.
    #[error("corrupt edge key: expected {expected} bytes, found {found}")]
    CorruptEdgeKey {
        /// Expected key size in bytes.
        expected: usize,
        /// Actual key size in bytes.
        found: usize,
    },

    /// An edge value has an unexpected byte length.
    #[error("corrupt edge value: expected {expected} bytes, found {found}")]
    CorruptEdgeValue {
        /// Expected value size in bytes.
        expected: usize,
        /// Actual value size in bytes.
        found: usize,
    },

    /// An invalid edge type discriminant byte was encountered.
    #[error("invalid edge type discriminant: {0}")]
    InvalidEdgeType(u8),

    // ── Vector storage errors ───────────────────────────────────────
    /// An error from the vector storage subsystem.
    #[error("vector storage error: {0}")]
    Vector(#[from] crate::storage::vectors::VectorError),
}
