//! Unified error type for all storage modules.

use thiserror::Error;
use uuid::Uuid;

use crate::model::error::DecodeError;

/// Errors from storage operations (metadata, vectors, text, edges).
#[derive(Debug, Error)]
pub enum StorageError {
    // ── redb errors (from metadata.rs and edges.rs) ──────────────────

    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),

    #[error("redb database error: {0}")]
    RedbDatabase(#[from] redb::DatabaseError),

    #[error("redb table error: {0}")]
    RedbTable(#[from] redb::TableError),

    #[error("redb transaction error: {0}")]
    RedbTransaction(#[from] redb::TransactionError),

    #[error("redb storage error: {0}")]
    RedbStorage(#[from] redb::StorageError),

    #[error("redb commit error: {0}")]
    RedbCommit(#[from] redb::CommitError),

    // ── Metadata errors ──────────────────────────────────────────────

    #[error("duplicate memory ID: {0}")]
    DuplicateId(Uuid),

    #[error("memory not found: {0}")]
    NotFound(Uuid),

    #[error("duplicate namespace name: {0}")]
    DuplicateName(String),

    #[error("namespace not found: {0}")]
    NamespaceNotFound(u32),

    #[error("corrupt index: {0}")]
    CorruptIndex(String),

    #[error("deserialization error: {0}")]
    Deserialize(String),

    #[error("serialization error: {0}")]
    Serialize(String),

    #[error("decode error: {0}")]
    Decode(#[from] DecodeError),

    #[error("database is locked by another process")]
    DatabaseLocked,

    #[error("invalid decay phase: {0}")]
    InvalidPhase(u8),

    // ── I/O errors ───────────────────────────────────────────────────

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // ── Text storage errors ──────────────────────────────────────────

    #[error("stored entry length mismatch at offset {offset}: expected {expected}, found {found}")]
    TextLengthMismatch {
        offset: u64,
        expected: u32,
        found: u32,
    },

    #[error("CRC32 mismatch at offset {offset}: expected {expected:#010x}, computed {computed:#010x}")]
    TextCrcMismatch {
        offset: u64,
        expected: u32,
        computed: u32,
    },

    #[error("invalid UTF-8 at offset {offset}: {source}")]
    InvalidUtf8 {
        offset: u64,
        source: std::string::FromUtf8Error,
    },

    #[error("text payload too large: {size} bytes, max {max}")]
    TextTooLarge { size: usize, max: usize },

    #[error("attempted to read TextRef::NONE sentinel")]
    InvalidTextRef,

    #[error("attempted to append empty text")]
    EmptyText,

    #[error("file header could not be parsed: {file}")]
    HeaderParseError { file: &'static str },

    #[error("invalid magic bytes in {file}: expected {expected:?}, found {found:?}")]
    InvalidMagic {
        file: &'static str,
        expected: [u8; 4],
        found: [u8; 4],
    },

    #[error("unsupported version in {file}: expected {expected}, found {found}")]
    UnsupportedVersion {
        file: &'static str,
        expected: u16,
        found: u16,
    },

    #[error("parent directory path is missing or invalid")]
    InvalidPath,

    // ── Edge storage errors ──────────────────────────────────────────

    #[error("corrupt edge key: expected {expected} bytes, found {found}")]
    CorruptEdgeKey { expected: usize, found: usize },

    #[error("corrupt edge value: expected {expected} bytes, found {found}")]
    CorruptEdgeValue { expected: usize, found: usize },

    #[error("invalid edge type discriminant: {0}")]
    InvalidEdgeType(u8),

    // ── Vector storage errors ───────────────────────────────────────

    #[error("vector storage error: {0}")]
    Vector(#[from] crate::storage::vectors::VectorError),
}
