//! Memory-mapped vector storage (vectors.dat) with embedded free list.
//!
//! Each namespace gets its own vector file with independent dimensionality.
//! Vectors are read via zero-copy mmap and written via the file descriptor.
//! See CS-06 for the full specification.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use thiserror::Error;
use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::model::NamespaceId;

// ═══════════════════════════════════════════════════════════════════════
// Error Type
// ═══════════════════════════════════════════════════════════════════════

/// Errors from vector storage operations.
#[derive(Debug, Error)]
pub enum VectorError {
    /// An I/O error occurred on a vector file.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// Path to the vector file.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The vector file is corrupt or inconsistent.
    #[error("corrupt vector file {path}: {reason}")]
    Corrupt {
        /// Path to the corrupt file.
        path: PathBuf,
        /// Description of the corruption.
        reason: String,
    },

    /// The file has invalid magic bytes (expected "MEMV").
    #[error("magic mismatch in {path}: expected MEMV, got {found:?}")]
    BadMagic {
        /// Path to the file with bad magic.
        path: PathBuf,
        /// Actual magic bytes found.
        found: [u8; 4],
    },

    /// The file format version is not supported.
    #[error("version mismatch in {path}: expected 1, got {found}")]
    BadVersion {
        /// Path to the file with the wrong version.
        path: PathBuf,
        /// Actual version number found.
        found: u16,
    },

    /// The file's embedding dimensionality does not match the request.
    #[error("dimension mismatch: file has {file_dim}, caller requested {requested_dim}")]
    DimensionMismatch {
        /// Dimensionality stored in the file header.
        file_dim: u16,
        /// Dimensionality the caller expected.
        requested_dim: u16,
    },

    /// The file's stride does not match the expected value.
    #[error("stride mismatch in {path}: expected {expected}, got {found}")]
    StrideMismatch {
        /// Path to the file with the wrong stride.
        path: PathBuf,
        /// Expected stride in bytes.
        expected: u32,
        /// Actual stride found in the header.
        found: u32,
    },

    /// The header CRC32 does not match the computed value.
    #[error("header CRC mismatch in {path}: stored {stored:#010x}, computed {computed:#010x}")]
    HeaderCrcMismatch {
        /// Path to the file with the CRC mismatch.
        path: PathBuf,
        /// CRC32 stored in the header.
        stored: u32,
        /// CRC32 computed from the header bytes.
        computed: u32,
    },

    /// A vector slot index is out of bounds.
    #[error("slot {slot} out of bounds (slot_count = {slot_count})")]
    SlotOutOfBounds {
        /// The requested slot index.
        slot: u32,
        /// Total number of allocated slots.
        slot_count: u32,
    },

    /// The provided vector length does not match the index dimensions.
    #[error("vector length {got} does not match dimensions {expected}")]
    WrongVectorLength {
        /// Expected vector length (dimensions).
        expected: usize,
        /// Actual vector length provided.
        got: usize,
    },

    /// The vector file is already locked by another process.
    #[error("file already locked: {path}")]
    FileLocked {
        /// Path to the locked file.
        path: PathBuf,
    },
}

/// Convenience alias used throughout this module.
pub type Result<T> = std::result::Result<T, VectorError>;

// ═══════════════════════════════════════════════════════════════════════
// VectorFileHeader — 64 bytes
// ═══════════════════════════════════════════════════════════════════════

/// 64-byte file header for vectors.dat.
/// All multi-byte integers are little-endian.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct VectorFileHeader {
    /// Magic bytes: b"MEMV" (0x4D454D56).
    pub magic: [u8; 4],

    /// Format version. Current: 1.
    pub version: U16,

    /// Embedding dimensionality (e.g., 768, 1536, 3072).
    pub dimensions: U16,

    /// Total allocated vector slots (live + free).
    pub slot_count: U32,

    /// Number of live (non-free) vectors.
    pub live_count: U32,

    /// Bytes per slot: `dimensions * 4`. Stored explicitly so readers
    /// need not compute it and so the invariant is checked once on open.
    pub stride: U32,

    /// Head of the embedded free list. Index of first free slot, or
    /// `u32::MAX` (0xFFFFFFFF) when the free list is empty.
    pub free_list_head: U32,

    /// Number of free slots (for O(1) capacity queries).
    pub free_slot_count: U32,

    /// CRC32 of header bytes `[0..56)`. Validated on open.
    pub header_crc: U32,

    /// Reserved -- must be zeroed. Provides room for future fields
    /// without a version bump.
    pub _reserved: [u8; 32],
}

// Compile-time assertion: header is exactly 64 bytes.
const _: () = assert!(std::mem::size_of::<VectorFileHeader>() == 64);

impl VectorFileHeader {
    /// Header occupies exactly 64 bytes (one cache line).
    pub const SIZE: usize = 64;

    /// Magic bytes identifying a vector file.
    pub const MAGIC: [u8; 4] = *b"MEMV";

    /// Sentinel value indicating an empty free list.
    pub const FREE_LIST_EMPTY: u32 = u32::MAX;

    /// Byte offset of vector slot `slot` from the start of the file.
    #[inline]
    pub fn data_offset(slot: u32, stride: u32) -> u64 {
        Self::SIZE as u64 + (slot as u64) * (stride as u64)
    }

    /// Compute CRC32 over the first 56 bytes of a raw header buffer.
    pub fn compute_crc(raw: &[u8; Self::SIZE]) -> u32 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&raw[..56]);
        hasher.finalize()
    }

    /// Build a new header for a freshly created file.
    pub fn new(dimensions: u16) -> Self {
        let stride = (dimensions as u32) * 4;
        let mut header = Self {
            magic: Self::MAGIC,
            version: U16::new(1),
            dimensions: U16::new(dimensions),
            slot_count: U32::new(0),
            live_count: U32::new(0),
            stride: U32::new(stride),
            free_list_head: U32::new(Self::FREE_LIST_EMPTY),
            free_slot_count: U32::new(0),
            header_crc: U32::new(0),
            _reserved: [0u8; 32],
        };
        // Compute and set the CRC.
        let raw = zerocopy::IntoBytes::as_bytes(&header);
        let crc = Self::compute_crc(raw.try_into().unwrap());
        header.header_crc = U32::new(crc);
        header
    }

    /// Validate an on-disk header. Returns Ok(()) or a descriptive error.
    pub fn validate(&self, path: &Path, expected_dim: u16) -> Result<()> {
        if self.magic != Self::MAGIC {
            return Err(VectorError::BadMagic {
                path: path.to_path_buf(),
                found: self.magic,
            });
        }
        if self.version.get() != 1 {
            return Err(VectorError::BadVersion {
                path: path.to_path_buf(),
                found: self.version.get(),
            });
        }
        if self.dimensions.get() != expected_dim {
            return Err(VectorError::DimensionMismatch {
                file_dim: self.dimensions.get(),
                requested_dim: expected_dim,
            });
        }
        let expected_stride = (expected_dim as u32) * 4;
        if self.stride.get() != expected_stride {
            return Err(VectorError::StrideMismatch {
                path: path.to_path_buf(),
                expected: expected_stride,
                found: self.stride.get(),
            });
        }
        // CRC check — zero the crc field in a copy before hashing,
        // matching the way create and update_header compute the CRC.
        let stored_crc = self.header_crc.get();
        let raw_ref: &[u8; Self::SIZE] = zerocopy::IntoBytes::as_bytes(self).try_into().unwrap();
        let mut raw_copy: [u8; Self::SIZE] = *raw_ref;
        // Zero the header_crc field (bytes 28..32) before hashing.
        let crc_offset = std::mem::offset_of!(VectorFileHeader, header_crc);
        raw_copy[crc_offset..crc_offset + 4].fill(0);
        let computed = Self::compute_crc(&raw_copy);
        if computed != stored_crc {
            return Err(VectorError::HeaderCrcMismatch {
                path: path.to_path_buf(),
                stored: self.header_crc.get(),
                computed,
            });
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// VectorStore — one per namespace
// ═══════════════════════════════════════════════════════════════════════

/// Number of slots to pre-allocate when the file must grow.
const GROWTH_CHUNK: u32 = 1024;

/// Per-namespace vector storage. Owns the mmap (read) and file handle
/// (write). Not internally synchronized -- the caller wraps in
/// `Arc<RwLock<VectorStore>>`.
pub struct VectorStore {
    /// Read-only mapping for serving queries (zero-copy `get_vector`).
    mmap: Mmap,

    /// Open file handle for writes and file extension.
    /// Held for the process lifetime; locked exclusively via `fs2`.
    file: File,

    /// Absolute path to the vectors.dat file (for error messages).
    path: PathBuf,

    // --- Cached header fields (avoid re-reading hot path) ---
    /// Embedding dimensionality.
    dimensions: u32,

    /// Bytes per slot (`dimensions * 4`).
    stride: u32,

    /// Total allocated slots (live + free). Updated after extend/remap.
    slot_count: u32,
}

impl VectorStore {
    /// Open an existing vectors.dat or create a new one.
    ///
    /// * `dir` -- directory that will contain the file (created if absent).
    /// * `dim` -- embedding dimensionality for this namespace.
    ///
    /// On create: writes a 64-byte header and zero vector slots.
    /// On open: validates the header, acquires an exclusive file lock.
    pub fn open(dir: &Path, dim: usize) -> Result<Self> {
        let dim_u16: u16 = dim.try_into().map_err(|_| VectorError::Corrupt {
            path: dir.to_path_buf(),
            reason: format!("dimension {dim} exceeds u16::MAX"),
        })?;
        let path = dir.join("vectors.dat");
        std::fs::create_dir_all(dir).map_err(|e| VectorError::Io {
            path: path.clone(),
            source: e,
        })?;

        let exists = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| VectorError::Io {
                path: path.clone(),
                source: e,
            })?;

        // Acquire exclusive lock (advisory, prevents concurrent processes).
        use fs2::FileExt;
        file.try_lock_exclusive()
            .map_err(|_| VectorError::FileLocked { path: path.clone() })?;

        if !exists || file.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
            // New file -- write header.
            let header = VectorFileHeader::new(dim_u16);
            let header_bytes = zerocopy::IntoBytes::as_bytes(&header);
            (&file)
                .write_all(header_bytes)
                .map_err(|e| VectorError::Io {
                    path: path.clone(),
                    source: e,
                })?;
            file.sync_all().map_err(|e| VectorError::Io {
                path: path.clone(),
                source: e,
            })?;
        }

        // Memory-map the file (read-only).
        // SAFETY: file is exclusively locked; no external writer.
        let mmap = unsafe {
            Mmap::map(&file).map_err(|e| VectorError::Io {
                path: path.clone(),
                source: e,
            })?
        };

        // Read and validate the header.
        if mmap.len() < VectorFileHeader::SIZE {
            return Err(VectorError::Corrupt {
                path: path.clone(),
                reason: format!(
                    "file too small: {} bytes, need at least {}",
                    mmap.len(),
                    VectorFileHeader::SIZE
                ),
            });
        }
        let header =
            zerocopy::Ref::<_, VectorFileHeader>::from_bytes(&mmap[..VectorFileHeader::SIZE])
                .map_err(|e| VectorError::Corrupt {
                    path: path.clone(),
                    reason: format!("header parse error: {e}"),
                })?;
        header.validate(&path, dim_u16)?;

        let dimensions = header.dimensions.get() as u32;
        let stride = header.stride.get();
        let slot_count = header.slot_count.get();

        Ok(Self {
            mmap,
            file,
            path,
            dimensions,
            stride,
            slot_count,
        })
    }

    // ── Read Path ────────────────────────────────────────────────────

    /// Return vector at `slot` as a zero-copy `&[f32]` slice backed
    /// by the mmap.
    ///
    /// Returns `None` if `slot >= slot_count` or the mapped region is
    /// too short. The caller is responsible for only passing live slot
    /// indices -- free-list slots contain garbage data.
    #[inline]
    pub fn get_vector(&self, slot: u32) -> Option<&[f32]> {
        if slot >= self.slot_count {
            return None;
        }
        let offset = VectorFileHeader::SIZE + (slot as usize) * (self.stride as usize);
        let end = offset + (self.stride as usize);
        if end > self.mmap.len() {
            return None;
        }
        let bytes = &self.mmap[offset..end];
        // SAFETY: mmap base is page-aligned (>= 4096). offset is
        // header (64, divisible by 4) + slot * stride (stride is
        // dimensions*4, divisible by 4). Therefore bytes.as_ptr() is
        // 4-byte aligned, satisfying bytemuck's alignment requirement
        // for f32.
        Some(bytemuck::cast_slice(bytes))
    }

    /// Raw byte access to the entire data region after the header.
    /// Used by the flat SIMD scan in FlatVectorIndex (CS-14).
    #[inline]
    pub fn all_vectors_raw(&self) -> &[u8] {
        &self.mmap[VectorFileHeader::SIZE..]
    }

    /// Current number of allocated slots (live + free).
    #[inline]
    pub fn slot_count(&self) -> u32 {
        self.slot_count
    }

    /// Embedding dimensionality.
    #[inline]
    pub fn dimensions(&self) -> u32 {
        self.dimensions
    }

    // ── Write Path ───────────────────────────────────────────────────

    /// Allocate a slot and write `vector` into it. Returns the slot
    /// index for storage in meta.db.
    ///
    /// Prefers re-using a free-list slot. When the free list is empty,
    /// extends the file by `GROWTH_CHUNK` slots and populates the free
    /// list from the new range.
    pub fn insert_vector(&mut self, vector: &[f32]) -> Result<u32> {
        if vector.len() != self.dimensions as usize {
            return Err(VectorError::WrongVectorLength {
                expected: self.dimensions as usize,
                got: vector.len(),
            });
        }

        let slot = self.allocate_slot()?;
        let offset = VectorFileHeader::data_offset(slot, self.stride);

        // Write vector data via the file descriptor (not through mmap).
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| VectorError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        let bytes: &[u8] = bytemuck::cast_slice(vector);
        self.file.write_all(bytes).map_err(|e| VectorError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        // Note: fsync is deferred to the caller's batch sync.

        // Re-mmap so subsequent get_vector calls see the new data.
        self.remap()?;

        Ok(slot)
    }

    // ── Slot Allocation ──────────────────────────────────────────────

    /// Pop a slot from the free list, or extend the file if the free
    /// list is empty.
    fn allocate_slot(&mut self) -> Result<u32> {
        let header = self.read_header()?;

        if header.free_list_head.get() != VectorFileHeader::FREE_LIST_EMPTY {
            // --- Pop from free list ---
            let slot = header.free_list_head.get();
            let offset = VectorFileHeader::data_offset(slot, self.stride) as usize;

            // Read the chained next-pointer (first 4 bytes of the free slot).
            let next = u32::from_le_bytes(self.mmap[offset..offset + 4].try_into().unwrap());

            self.update_header(|h| {
                h.free_list_head = U32::new(next);
                h.free_slot_count = U32::new(h.free_slot_count.get() - 1);
                h.live_count = U32::new(h.live_count.get() + 1);
            })?;

            Ok(slot)
        } else {
            // --- Extend the file by GROWTH_CHUNK slots ---
            self.extend_file(1)?;
            // After extension the free list is populated; recurse once.
            self.allocate_slot()
        }
    }

    /// Grow the file by at least `min_new_slots` slots, rounding up
    /// to `GROWTH_CHUNK`. New slots are chained into the free list
    /// in ascending order (lowest index allocated first).
    fn extend_file(&mut self, min_new_slots: u32) -> Result<()> {
        let grow_by = min_new_slots.max(GROWTH_CHUNK);
        let header = self.read_header()?;
        let old_count = header.slot_count.get();
        let new_count = old_count + grow_by;
        let new_size = VectorFileHeader::SIZE as u64 + (new_count as u64) * (self.stride as u64);

        // Extend the file. The OS zero-fills the new region.
        self.file.set_len(new_size).map_err(|e| VectorError::Io {
            path: self.path.clone(),
            source: e,
        })?;

        // Chain new slots into the free list in reverse order so that
        // slot `old_count` (the lowest new index) is at the head and
        // will be allocated first.
        let mut current_head = header.free_list_head.get();
        for i in (old_count..new_count).rev() {
            let offset = VectorFileHeader::data_offset(i, self.stride);
            self.file
                .seek(SeekFrom::Start(offset))
                .map_err(|e| VectorError::Io {
                    path: self.path.clone(),
                    source: e,
                })?;
            self.file
                .write_all(&current_head.to_le_bytes())
                .map_err(|e| VectorError::Io {
                    path: self.path.clone(),
                    source: e,
                })?;
            current_head = i;
        }

        self.update_header(|h| {
            h.slot_count = U32::new(new_count);
            h.free_list_head = U32::new(current_head);
            h.free_slot_count = U32::new(h.free_slot_count.get() + grow_by);
        })?;

        self.remap()?;
        Ok(())
    }

    // ── Deletion ─────────────────────────────────────────────────────

    /// Mark `slot` as free. Zeros the slot data (preventing stale SIMD
    /// matches), then prepends the slot to the embedded free list.
    pub fn free_slot(&mut self, slot: u32) -> Result<()> {
        if slot >= self.slot_count {
            return Err(VectorError::SlotOutOfBounds {
                slot,
                slot_count: self.slot_count,
            });
        }

        let offset = VectorFileHeader::data_offset(slot, self.stride);

        // Zero the entire slot.
        let zeros = vec![0u8; self.stride as usize];
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| VectorError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        self.file.write_all(&zeros).map_err(|e| VectorError::Io {
            path: self.path.clone(),
            source: e,
        })?;

        // Write the current free_list_head as this slot's next-pointer
        // (first 4 bytes).
        let header = self.read_header()?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| VectorError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        self.file
            .write_all(&header.free_list_head.get().to_le_bytes())
            .map_err(|e| VectorError::Io {
                path: self.path.clone(),
                source: e,
            })?;

        // Update header: push slot onto free list.
        self.update_header(|h| {
            h.free_list_head = U32::new(slot);
            h.free_slot_count = U32::new(h.free_slot_count.get() + 1);
            h.live_count = U32::new(h.live_count.get() - 1);
        })?;

        self.remap()?;
        Ok(())
    }

    // ── Header I/O and Remap ─────────────────────────────────────────

    /// Read the header from the current mmap.
    fn read_header(&self) -> Result<VectorFileHeader> {
        let header_ref =
            zerocopy::Ref::<_, VectorFileHeader>::from_bytes(&self.mmap[..VectorFileHeader::SIZE])
                .map_err(|e| VectorError::Corrupt {
                    path: self.path.clone(),
                    reason: format!("header parse: {e}"),
                })?;
        Ok(*header_ref)
    }

    /// Apply a mutation to the on-disk header. Recomputes the CRC and
    /// writes the full 64-byte header via the file descriptor.
    fn update_header(&mut self, mutate: impl FnOnce(&mut VectorFileHeader)) -> Result<()> {
        let mut header = self.read_header()?;
        mutate(&mut header);

        // Recompute CRC over bytes [0..56).
        header.header_crc = U32::new(0); // zero the field before hashing
        let raw = zerocopy::IntoBytes::as_bytes(&header);
        let crc = VectorFileHeader::compute_crc(raw.try_into().unwrap());
        header.header_crc = U32::new(crc);

        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| VectorError::Io {
                path: self.path.clone(),
                source: e,
            })?;
        self.file
            .write_all(zerocopy::IntoBytes::as_bytes(&header))
            .map_err(|e| VectorError::Io {
                path: self.path.clone(),
                source: e,
            })?;

        Ok(())
    }

    /// Drop the current mmap and create a new one over the (possibly
    /// extended) file. Updates the cached `slot_count`.
    pub fn remap(&mut self) -> Result<()> {
        // SAFETY: file is exclusively locked, no external writer.
        let new_mmap = unsafe {
            Mmap::map(&self.file).map_err(|e| VectorError::Io {
                path: self.path.clone(),
                source: e,
            })?
        };
        self.mmap = new_mmap;

        // Refresh cached slot_count from the freshly mapped header.
        let header = self.read_header()?;
        self.slot_count = header.slot_count.get();

        Ok(())
    }

    /// Flush pending writes to durable storage.
    ///
    /// Calls `sync_data()` on the underlying file descriptor. This
    /// ensures all written vector data and header updates reach disk.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data().map_err(|e| VectorError::Io {
            path: self.path.clone(),
            source: e,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
// VectorManager — per-namespace HashMap
// ═══════════════════════════════════════════════════════════════════════

/// Wraps a `HashMap<NamespaceId, VectorStore>` to support per-namespace
/// vector files with independent dimensionality.
pub struct VectorManager {
    /// One VectorStore per namespace.
    stores: HashMap<NamespaceId, VectorStore>,

    /// Base directory: `recalld.db/`.
    base_path: PathBuf,
}

impl VectorManager {
    /// Create a new VectorManager rooted at `base_path`.
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            stores: HashMap::new(),
            base_path,
        }
    }

    /// Open (or create) the VectorStore for a namespace. The store is
    /// cached in the HashMap for subsequent calls.
    ///
    /// * `namespace_id`   -- interned integer from meta.db.
    /// * `namespace_name` -- human-readable name, used as the
    ///   subdirectory name (e.g., `"default"`, `"work-context"`).
    /// * `dimensions`     -- embedding dimensionality for this namespace.
    pub fn open_or_create(
        &mut self,
        namespace_id: NamespaceId,
        namespace_name: &str,
        dimensions: usize,
    ) -> Result<&mut VectorStore> {
        if !self.stores.contains_key(&namespace_id) {
            let dir = self.base_path.join(namespace_name);
            let store = VectorStore::open(&dir, dimensions)?;
            self.stores.insert(namespace_id, store);
        }
        Ok(self.stores.get_mut(&namespace_id).unwrap())
    }

    /// Get a shared reference to an already-opened store.
    /// Returns None if the namespace has not been opened.
    pub fn get(&self, namespace_id: NamespaceId) -> Option<&VectorStore> {
        self.stores.get(&namespace_id)
    }

    /// Get a mutable reference to an already-opened store.
    pub fn get_mut(&mut self, namespace_id: NamespaceId) -> Option<&mut VectorStore> {
        self.stores.get_mut(&namespace_id)
    }

    /// Iterate all open stores (for shutdown sync, diagnostics, etc.).
    pub fn iter(&self) -> impl Iterator<Item = (NamespaceId, &VectorStore)> {
        self.stores.iter().map(|(&id, store)| (id, store))
    }
}
