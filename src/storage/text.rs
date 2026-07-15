//! Append-only text store (fulltext.dat) with CRC32C integrity and compaction.
//!
//! Stores full_text payloads for memories in Phase 1 (Full). Each entry
//! is length-prefixed with a CRC32 checksum for corruption detection.
//! See CS-08 for the full specification.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use zerocopy::byteorder::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::model::MemoryId;
use crate::storage::error::StorageError;
use crate::storage::fsync::{fsync_dir, fsync_file};

// ═══════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════

/// Maximum text entry payload: 1 MB.
/// Matches the `full_text` limit in Spec 01.
pub const MAX_ENTRY_SIZE: u32 = 1_048_576;

/// Entry header size: 4 bytes length + 4 bytes CRC32.
const ENTRY_HEADER_SIZE: u64 = 8;

/// Minimum fragmentation ratio (dead bytes / total bytes) before
/// compaction is considered worthwhile. Below this threshold,
/// `compact()` returns a no-op result to avoid unnecessary I/O.
const COMPACTION_FRAGMENTATION_THRESHOLD: f64 = 0.20;

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
}

impl TextStore {
    /// Open an existing fulltext.dat, or create a new one if the file
    /// does not exist. Validates the header and sets the write
    /// position to end-of-file.
    pub fn open(path: &Path) -> Result<TextStore, StorageError> {
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
            })
        }
    }

    /// Append a UTF-8 text entry to the log.
    ///
    /// Returns a `TextRef` containing the file offset and payload
    /// length. The caller stores this in meta.db's `DiskRecord`.
    pub fn append(&mut self, text: &str) -> Result<TextRef, StorageError> {
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

    /// Read the text entry at the given `TextRef`.
    ///
    /// Validates length match and CRC32 integrity.
    ///
    /// Uses positional reads (`pread`) so this method takes `&self`
    /// rather than `&mut self`. This avoids requiring an exclusive
    /// write lock for read-only operations, enabling concurrent
    /// readers via `RwLock`.
    pub fn read(&self, text_ref: TextRef) -> Result<String, StorageError> {
        if !text_ref.is_some() {
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

    /// Phase 1 of fulltext.dat compaction: write a new file containing
    /// only live entries, but do NOT rename or remove the marker.
    ///
    /// The caller must update meta.db with the new offsets from the
    /// returned `CompactionResult`, then call [`compact_finalize`] to
    /// atomically swap the files. This two-phase approach ensures that
    /// meta.db offsets are always consistent with the active fulltext.dat.
    ///
    /// Returns `(result, compacted)` where `compacted` is `true` if a
    /// new file was written (fragmentation above threshold) and
    /// `compact_finalize()` must be called afterwards.
    pub fn compact_write_new_file(
        &mut self,
        live_refs: &[(MemoryId, TextRef)],
    ) -> Result<(CompactionResult, bool), StorageError> {
        let parent_dir = self
            .path
            .parent()
            .ok_or(StorageError::InvalidPath)?
            .to_path_buf();
        let marker_path = parent_dir.join(".compacting");
        let new_path = parent_dir.join("fulltext.dat.new");

        let old_size = self.write_pos;
        let total_possible = self.count_entries_approx();

        // Estimate live data size to check if compaction is worthwhile.
        let live_data_size: u64 = live_refs
            .iter()
            .map(|(_, r)| ENTRY_HEADER_SIZE + r.length as u64)
            .sum::<u64>()
            + TextLogHeader::SIZE as u64;
        let dead_bytes = old_size.saturating_sub(live_data_size);
        let fragmentation = if old_size > 0 {
            dead_bytes as f64 / old_size as f64
        } else {
            0.0
        };

        if fragmentation < COMPACTION_FRAGMENTATION_THRESHOLD {
            tracing::debug!(
                fragmentation = format!("{:.1}%", fragmentation * 100.0),
                threshold = format!("{:.0}%", COMPACTION_FRAGMENTATION_THRESHOLD * 100.0),
                "Skipping fulltext.dat compaction: fragmentation below threshold"
            );
            return Ok((
                CompactionResult {
                    old_size,
                    new_size: old_size,
                    entries_removed: 0,
                    entries_kept: live_refs.len(),
                    new_refs: live_refs
                        .iter()
                        .map(|&(id, text_ref)| (id, text_ref))
                        .collect(),
                },
                false,
            ));
        }

        // Step 1: Write marker.
        fs::write(&marker_path, b"fulltext.dat compaction in progress\n")?;
        fsync_file(&marker_path)?;
        fsync_dir(&parent_dir)?;

        tracing::info!(
            live = live_refs.len(),
            old_size,
            "Starting fulltext.dat compaction"
        );

        // Step 2: Create new file with header.
        let mut new_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&new_path)?;

        let header = TextLogHeader::new();
        new_file.write_all(zerocopy::IntoBytes::as_bytes(&header))?;

        let mut new_write_pos = TextLogHeader::SIZE as u64;
        let mut new_refs: RefMapping = Vec::with_capacity(live_refs.len());

        // Step 3: Copy live entries.
        for &(memory_id, old_ref) in live_refs {
            // Read from old file (includes CRC verification).
            let text = self.read(old_ref)?;
            let data = text.as_bytes();
            let crc = compute_crc32(data);
            let length_u32 = data.len() as u32;

            // Write to new file.
            let new_offset = new_write_pos;
            new_file.write_all(&length_u32.to_le_bytes())?;
            new_file.write_all(&crc.to_le_bytes())?;
            new_file.write_all(data)?;
            new_write_pos += ENTRY_HEADER_SIZE + data.len() as u64;

            let new_ref = TextRef {
                file_offset: new_offset,
                length: length_u32,
            };
            new_refs.push((memory_id, new_ref));
        }

        // Step 4: Fsync new file.
        new_file.sync_all()?;
        drop(new_file);

        let entries_removed = total_possible.saturating_sub(live_refs.len());

        let result = CompactionResult {
            old_size,
            new_size: new_write_pos,
            entries_removed,
            entries_kept: live_refs.len(),
            new_refs,
        };

        Ok((result, true))
    }

    /// Phase 2 of fulltext.dat compaction: atomically rename the new
    /// file over the old one, remove markers, and reopen the file handle.
    ///
    /// Must only be called after `compact_write_new_file` returned
    /// `compacted = true` AND the caller has persisted updated offsets
    /// to meta.db.
    pub fn compact_finalize(&mut self) -> Result<(), StorageError> {
        let parent_dir = self
            .path
            .parent()
            .ok_or(StorageError::InvalidPath)?
            .to_path_buf();
        let marker_path = parent_dir.join(".compacting");
        let committed_marker_path = parent_dir.join(".compaction-meta-committed");
        let new_path = parent_dir.join("fulltext.dat.new");

        // Step 1: Atomic rename fulltext.dat.new -> fulltext.dat.
        fs::rename(&new_path, &self.path)?;

        // Step 2: Fsync directory to persist the rename.
        fsync_dir(&parent_dir)?;

        // Step 3: Remove markers.
        let _ = fs::remove_file(&committed_marker_path);
        let _ = fs::remove_file(&marker_path);
        fsync_dir(&parent_dir)?;

        // Step 4: Reopen file handle with updated contents.
        self.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        // Determine new write position from file size.
        self.write_pos = self.file.seek(SeekFrom::End(0))?;

        tracing::info!(new_size = self.write_pos, "Text.log compaction finalized");

        Ok(())
    }

    /// Approximate entry count by scanning forward from header.
    /// Used only for compaction stats; not authoritative.
    fn count_entries_approx(&mut self) -> usize {
        let mut count = 0usize;
        let mut pos = TextLogHeader::SIZE as u64;

        while pos < self.write_pos {
            if self.file.seek(SeekFrom::Start(pos)).is_err() {
                break;
            }
            let mut len_buf = [0u8; 4];
            if self.file.read_exact(&mut len_buf).is_err() {
                break;
            }
            let entry_len = u32::from_le_bytes(len_buf) as u64;
            pos += ENTRY_HEADER_SIZE + entry_len;
            count += 1;
        }
        count
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Crash Recovery
// ═══════════════════════════════════════════════════════════════════════

/// Recover from an interrupted fulltext.dat compaction.
///
/// Called once during startup validation, BEFORE opening the TextStore.
///
/// Handles two compaction states using marker files:
///
/// - `.compacting` alone (no `.compaction-meta-committed`):
///   meta.db has NOT been updated with new offsets yet. Delete the
///   incomplete `fulltext.dat.new` and revert to the original file.
///
/// - `.compacting` AND `.compaction-meta-committed`:
///   meta.db HAS been updated with new offsets. Complete the rename
///   of `fulltext.dat.new` -> `fulltext.dat` so the file matches
///   the committed offsets. If the rename already happened (no .new
///   file), just clean up the markers.
pub fn recover_text_compaction(db_dir: &Path) -> Result<(), StorageError> {
    let marker_path = db_dir.join(".compacting");
    let committed_marker_path = db_dir.join(".compaction-meta-committed");
    let new_file_path = db_dir.join("fulltext.dat.new");
    let fulltext_path = db_dir.join("fulltext.dat");

    if !marker_path.exists() {
        // No compaction was in progress.
        return Ok(());
    }

    tracing::warn!(
        "Compaction marker found at {} -- recovering",
        marker_path.display()
    );

    if committed_marker_path.exists() {
        // meta.db has already been updated with new offsets.
        // We must complete the rename so fulltext.dat matches.
        if new_file_path.exists() {
            tracing::warn!(
                "meta.db offsets committed; completing rename of fulltext.dat.new -> fulltext.dat"
            );
            fs::rename(&new_file_path, &fulltext_path)?;
            fsync_dir(db_dir)?;
        } else {
            // Rename already happened before crash; just clean up markers.
            tracing::info!(
                "meta.db offsets committed and rename already complete; cleaning up markers"
            );
        }
    } else {
        // meta.db has NOT been updated. Safe to delete .new and revert
        // to the original fulltext.dat with its original offsets.
        if new_file_path.exists() {
            tracing::warn!("Deleting incomplete fulltext.dat.new");
            fs::remove_file(&new_file_path)?;
        }
    }

    // Remove all markers.
    let _ = fs::remove_file(&committed_marker_path);
    let _ = fs::remove_file(&marker_path);
    fsync_dir(db_dir)?;

    tracing::info!("Compaction recovery complete");
    Ok(())
}
