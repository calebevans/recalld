//! Append-only text store (fulltext.dat) with CRC32C integrity and compaction.
//!
//! Stores full_text payloads for memories in Phase 1 (Full). Each entry
//! is length-prefixed with a CRC32 checksum for corruption detection.
//! See CS-08 for the full specification.

pub(crate) mod generation;

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::model::MemoryId;
use crate::storage::error::StorageError;

// ═══════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════

/// Maximum text entry payload: 1 MB.
/// Matches the `full_text` limit in Spec 01.
pub const MAX_ENTRY_SIZE: u32 = 1_048_576;

/// Entry header size: 4 bytes length + 4 bytes CRC32.
const ENTRY_HEADER_SIZE: u64 = 8;

/// Magic bytes identifying a fulltext.dat file.
const TEXT_LOG_MAGIC: [u8; 4] = *b"MEMT";

/// Current file format version.
const TEXT_LOG_VERSION: u16 = 1;

// ═══════════════════════════════════════════════════════════════════════
// TextLogHeader — 16 bytes
// ═══════════════════════════════════════════════════════════════════════

/// 16-byte file header for fulltext.dat.
/// All multi-byte integers are little-endian.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct TextLogHeader {
    /// Magic bytes: "MEMT" (0x4D454D54).
    pub magic: [u8; 4],
    /// Format version. Current: 1.
    pub version: U16,
    /// Reserved for future use (padding to 16 bytes).
    pub _reserved: [u8; 10],
}

impl TextLogHeader {
    pub const SIZE: usize = 16;

    /// Build a new header with current magic and version.
    pub fn new() -> Self {
        Self {
            magic: TEXT_LOG_MAGIC,
            version: U16::new(TEXT_LOG_VERSION),
            _reserved: [0u8; 10],
        }
    }

    /// Validate magic bytes and version.
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.magic != TEXT_LOG_MAGIC {
            return Err(StorageError::InvalidMagic {
                file: "fulltext.dat",
                expected: TEXT_LOG_MAGIC,
                found: self.magic,
            });
        }
        if self.version.get() != TEXT_LOG_VERSION {
            return Err(StorageError::UnsupportedVersion {
                file: "fulltext.dat",
                expected: TEXT_LOG_VERSION,
                found: self.version.get(),
            });
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CRC32 Helper
// ═══════════════════════════════════════════════════════════════════════

/// Compute CRC32 of a byte slice. Uses hardware SIMD when available.
fn compute_crc32(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

// ═══════════════════════════════════════════════════════════════════════
// Positional Read Helper
// ═══════════════════════════════════════════════════════════════════════

/// Read exactly `buf.len()` bytes from `file` at the given byte offset,
/// without mutating the file's seek cursor. Uses `pread(2)` on Unix
/// so the call is safe to issue from `&self` (no `&mut` needed).
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> Result<(), StorageError> {
    let mut pos = 0usize;
    while pos < buf.len() {
        match file.read_at(&mut buf[pos..], offset + pos as u64) {
            Ok(0) => {
                return Err(StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF during positional read",
                )));
            }
            Ok(n) => pos += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(StorageError::Io(e)),
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// TextRef
// ═══════════════════════════════════════════════════════════════════════

/// Reference to a text entry in fulltext.dat.
/// Stored in DiskRecord as (text_offset: u64, text_length: u32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextRef {
    /// Byte offset from the start of fulltext.dat to the entry header
    /// (the `length` field, NOT the data).
    pub file_offset: u64,
    /// Byte length of the text payload (excludes the 8-byte entry header).
    pub length: u32,
}

impl TextRef {
    /// The sentinel value meaning "no text stored."
    /// Safe because offset 0 is always inside the 16-byte file header.
    pub const NONE: TextRef = TextRef {
        file_offset: 0,
        length: 0,
    };

    /// Returns true if this ref points to actual text (not the sentinel).
    pub fn is_some(&self) -> bool {
        self.file_offset > 0 && self.length > 0
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactionResult
// ═══════════════════════════════════════════════════════════════════════

/// Mapping from MemoryId to its new TextRef after compaction.
pub type RefMapping = Vec<(MemoryId, TextRef)>;

/// Statistics and results from a fulltext.dat compaction run.
#[derive(Debug)]
pub struct CompactionResult {
    /// Size of the old fulltext.dat in bytes.
    pub old_size: u64,
    /// Size of the new (compacted) fulltext.dat in bytes.
    pub new_size: u64,
    /// Number of dead entries that were removed.
    pub entries_removed: usize,
    /// Number of live entries that were copied.
    pub entries_kept: usize,
    /// Mapping: for each live entry, the MemoryId and its new TextRef.
    pub new_refs: RefMapping,
}

// ═══════════════════════════════════════════════════════════════════════
// TextStore
// ═══════════════════════════════════════════════════════════════════════

/// Append-only text log with CRC32C integrity.
///
/// Thread safety: NOT internally synchronized. The caller
/// (RedbStorageEngine) must hold an appropriate lock before
/// calling any method.
pub struct TextStore {
    /// Open file handle (read + write mode).
    file: File,
    /// Absolute path to fulltext.dat on disk.
    path: PathBuf,
    /// Current write position (byte offset of the next append).
    write_pos: u64,
    header_size: u64,
}

impl TextStore {
    /// Open an existing fulltext.dat, or create a new one if the file
    /// does not exist. Validates the header and sets the write
    /// position to end-of-file.
    pub(super) fn open(path: &Path) -> Result<TextStore, StorageError> {
        if path.exists() {
            // --- Open existing ---
            let mut file = OpenOptions::new().read(true).write(true).open(path)?;

            // Read and validate header.
            let mut header_buf = [0u8; TextLogHeader::SIZE];
            file.read_exact(&mut header_buf)?;
            let header = TextLogHeader::read_from_bytes(&header_buf).map_err(|_| {
                StorageError::HeaderParseError {
                    file: "fulltext.dat",
                }
            })?;
            header.validate()?;

            // Set write position to end of file.
            let write_pos = file.seek(SeekFrom::End(0))?;

            Ok(TextStore {
                file,
                path: path.to_path_buf(),
                write_pos,
                header_size: 16,
            })
        } else {
            // --- Create new ---
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)?;

            // Write header.
            let header = TextLogHeader::new();
            file.write_all(zerocopy::IntoBytes::as_bytes(&header))?;
            file.sync_all()?;

            Ok(TextStore {
                file,
                path: path.to_path_buf(),
                write_pos: TextLogHeader::SIZE as u64,
                header_size: 16,
            })
        }
    }

    /// Append a UTF-8 text entry to the log.
    ///
    /// Returns a `TextRef` containing the file offset and payload
    /// length. The caller stores this in meta.db's `DiskRecord`.
    pub(super) fn append(&mut self, text: &str) -> Result<TextRef, StorageError> {
        let data = text.as_bytes();
        let length = data.len();

        if length > MAX_ENTRY_SIZE as usize {
            return Err(StorageError::TextTooLarge {
                size: length,
                max: MAX_ENTRY_SIZE as usize,
            });
        }
        if length == 0 {
            return Err(StorageError::EmptyText);
        }

        let length_u32 = length as u32;
        let crc = compute_crc32(data);
        let offset = self.write_pos;

        // Seek to current write position.
        self.file.seek(SeekFrom::Start(offset))?;

        // Write entry: [length: u32 LE] [crc32: u32 LE] [data]
        self.file.write_all(&length_u32.to_le_bytes())?;
        self.file.write_all(&crc.to_le_bytes())?;
        self.file.write_all(data)?;

        // Advance write position.
        self.write_pos = offset + ENTRY_HEADER_SIZE + length as u64;

        Ok(TextRef {
            file_offset: offset,
            length: length_u32,
        })
    }

    pub(crate) fn open_read_only(path: &Path) -> Result<Self, StorageError> {
        let file = File::open(path)?;
        let write_pos = file.metadata()?.len();
        let mut store = Self {
            file,
            path: path.to_path_buf(),
            write_pos,
            header_size: 16,
        };
        store.validate_header()?;
        Ok(store)
    }

    /// Read the text entry at the given `TextRef`.
    ///
    /// Validates length match and CRC32 integrity.
    ///
    /// Uses positional reads (`pread`) so this method takes `&self`
    /// rather than `&mut self`. This avoids requiring an exclusive
    /// write lock for read-only operations, enabling concurrent
    /// readers via `RwLock`.
    pub fn read(&self, text_ref: TextRef) -> Result<String, StorageError> {
        if !text_ref.is_some()
            || text_ref.length > MAX_ENTRY_SIZE
            || text_ref.file_offset < self.header_size
            || text_ref
                .file_offset
                .checked_add(ENTRY_HEADER_SIZE)
                .and_then(|offset| offset.checked_add(u64::from(text_ref.length)))
                .is_none_or(|end| end > self.write_pos)
        {
            return Err(StorageError::InvalidTextRef);
        }

        // Read entry header (8 bytes) using positional I/O.
        let mut header_buf = [0u8; 8];
        read_exact_at(&self.file, &mut header_buf, text_ref.file_offset)?;
        let stored_length = u32::from_le_bytes(header_buf[0..4].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(header_buf[4..8].try_into().unwrap());

        // Cross-check length against meta.db's expected value.
        if stored_length != text_ref.length {
            return Err(StorageError::TextLengthMismatch {
                offset: text_ref.file_offset,
                expected: text_ref.length,
                found: stored_length,
            });
        }

        // Read payload using positional I/O.
        let mut data = vec![0u8; stored_length as usize];
        let data_offset = text_ref.file_offset + ENTRY_HEADER_SIZE;
        read_exact_at(&self.file, &mut data, data_offset)?;

        // Verify CRC32.
        let computed_crc = compute_crc32(&data);
        if computed_crc != stored_crc {
            return Err(StorageError::TextCrcMismatch {
                offset: text_ref.file_offset,
                expected: stored_crc,
                computed: computed_crc,
            });
        }

        // Defense-in-depth: validate UTF-8 even though we only accept
        // valid UTF-8 at write time. Guards against on-disk corruption
        // that passes the CRC check (e.g., bit-flip in both data and CRC).
        String::from_utf8(data).map_err(|e| StorageError::InvalidUtf8 {
            offset: text_ref.file_offset,
            source: e,
        })
    }

    /// Validate the file header. Called during startup validation.
    pub fn validate_header(&mut self) -> Result<(), StorageError> {
        if self.header_size == 48 {
            return Ok(());
        } // Full identity checked by open_generation.
        self.file.seek(SeekFrom::Start(0))?;
        let mut header_buf = [0u8; TextLogHeader::SIZE];
        self.file.read_exact(&mut header_buf)?;
        let header = TextLogHeader::read_from_bytes(&header_buf).map_err(|_| {
            StorageError::HeaderParseError {
                file: "fulltext.dat",
            }
        })?;
        header.validate()
    }

    /// Flush pending writes to durable storage.
    pub fn sync_data(&self) -> Result<(), StorageError> {
        self.file.sync_data().map_err(StorageError::Io)
    }

    /// Return the current file size in bytes.
    pub fn file_size(&self) -> u64 {
        self.write_pos
    }

    /// Return the path to the fulltext.dat file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Legacy compaction is disabled until metadata selects text generations.
    pub(super) fn compact_write_new_file(
        &mut self,
        _live_refs: &[(MemoryId, TextRef)],
    ) -> Result<(CompactionResult, bool), StorageError> {
        Err(StorageError::RecoveryRequired(
            "legacy text compaction is disabled".into(),
        ))
    }

    /// Never finalize a marker-driven swap: the committed file is ambiguous.
    pub(super) fn compact_finalize(&mut self) -> Result<(), StorageError> {
        Err(StorageError::RecoveryRequired(
            "legacy text compaction is disabled".into(),
        ))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Crash Recovery
// ═══════════════════════════════════════════════════════════════════════

/// Reject interrupted legacy compaction without modifying any evidence.
/// Filesystem markers cannot prove which offsets the redb transaction committed.
pub fn recover_text_compaction(db_dir: &Path) -> Result<(), StorageError> {
    for name in [
        ".compacting",
        ".compaction-meta-committed",
        "fulltext.dat.new",
    ] {
        if db_dir.join(name).try_exists()? {
            return Err(StorageError::RecoveryRequired(format!(
                "legacy compaction artifact {name}; preserve all text files and recover from independent evidence or an intact backup"
            )));
        }
    }
    Ok(())
}
