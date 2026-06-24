//! Secondary index data structures for the metadata store.
//!
//! Contains `PhaseIndex` — roaring bitmap indexes tracking which vector
//! slots belong to each decay phase. Managed internally by
//! `MetadataStore`; persisted to `INDEX_TABLE` in meta.db.
//!
//! See CS-07 Section 6.

use roaring::RoaringBitmap;

use crate::model::DecayPhase;
use crate::storage::error::StorageError;

// ═══════════════════════════════════════════════════════════════════════
// PhaseIndex
// ═══════════════════════════════════════════════════════════════════════

/// In-memory bitmap index tracking which vector slots belong to each
/// decay phase. The authoritative copy lives here in RAM; it is
/// serialized to meta.db `INDEX_TABLE` periodically and rebuilt from
/// `META_TABLE` on startup if missing or corrupt.
///
/// Stores `vector_slot` (u32), not `MemoryId` (UUID is 128-bit and
/// does not fit roaring's u32 universe).
pub struct PhaseIndex {
    /// Phase 1: full text + embedding.
    full: RoaringBitmap,
    /// Phase 2: summary + embedding.
    summary: RoaringBitmap,
    /// Phase 3: embedding only.
    ghost: RoaringBitmap,
    /// Phase 4: tombstone — content stripped, graph preserved.
    /// Tombstoned memories are removed from the vector index, so this
    /// bitmap is typically empty in practice (vector slots are freed).
    /// It exists for completeness of the phase tracking.
    tombstone: RoaringBitmap,
}

impl PhaseIndex {
    /// Create an empty phase index.
    pub fn new() -> Self {
        Self {
            full: RoaringBitmap::new(),
            summary: RoaringBitmap::new(),
            ghost: RoaringBitmap::new(),
            tombstone: RoaringBitmap::new(),
        }
    }

    /// Add a new memory (always starts in Phase::Full).
    pub fn insert(&mut self, slot: u32) {
        self.full.insert(slot);
    }

    /// Move a memory between phases.
    pub fn transition(&mut self, slot: u32, from: DecayPhase, to: DecayPhase) {
        self.bitmap_mut(from).remove(slot);
        self.bitmap_mut(to).insert(slot);
    }

    /// Remove a memory entirely (after deletion).
    pub fn remove(&mut self, slot: u32, phase: DecayPhase) {
        self.bitmap_mut(phase).remove(slot);
    }

    /// Get a read-only reference to a phase's bitmap.
    pub fn bitmap(&self, phase: DecayPhase) -> &RoaringBitmap {
        match phase {
            DecayPhase::Full => &self.full,
            DecayPhase::Summary => &self.summary,
            DecayPhase::Ghost => &self.ghost,
            DecayPhase::Tombstone => &self.tombstone,
        }
    }

    /// Count of memories in a given phase.
    pub fn count(&self, phase: DecayPhase) -> u64 {
        self.bitmap(phase).len()
    }

    fn bitmap_mut(&mut self, phase: DecayPhase) -> &mut RoaringBitmap {
        match phase {
            DecayPhase::Full => &mut self.full,
            DecayPhase::Summary => &mut self.summary,
            DecayPhase::Ghost => &mut self.ghost,
            DecayPhase::Tombstone => &mut self.tombstone,
        }
    }

    /// Serialize all bitmaps into a single byte buffer.
    ///
    /// Format: `[u32 size_1][bitmap_1 bytes][u32 size_2][bitmap_2 bytes]
    ///          [u32 size_3][bitmap_3 bytes][u32 size_4][bitmap_4 bytes]`
    /// Order: full, summary, ghost, tombstone.
    ///
    /// Backward-compatible: `from_bytes` tolerates the absence of the
    /// tombstone bitmap (pre-tombstone data files).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for bitmap in [&self.full, &self.summary, &self.ghost, &self.tombstone] {
            let size = bitmap.serialized_size();
            buf.extend_from_slice(&(size as u32).to_le_bytes());
            bitmap
                .serialize_into(&mut buf)
                .expect("bitmap serialization is infallible");
        }
        buf
    }

    /// Deserialize from bytes produced by `to_bytes()`.
    ///
    /// Backward-compatible: if only 3 bitmaps are present (pre-tombstone
    /// format), the tombstone bitmap is initialized as empty.
    pub fn from_bytes(data: &[u8]) -> Result<Self, StorageError> {
        let mut offset = 0;
        let labels = ["full", "summary", "ghost", "tombstone"];
        let mut bitmaps = Vec::with_capacity(4);
        for (i, label) in labels.iter().enumerate() {
            // The tombstone bitmap is optional for backward compatibility.
            if offset >= data.len() {
                if i >= 3 {
                    // Tombstone bitmap missing — use empty.
                    bitmaps.push(RoaringBitmap::new());
                    break;
                }
                return Err(StorageError::CorruptIndex(format!(
                    "phase bitmap '{}' truncated at length prefix",
                    label
                )));
            }
            if offset + 4 > data.len() {
                if i >= 3 {
                    bitmaps.push(RoaringBitmap::new());
                    break;
                }
                return Err(StorageError::CorruptIndex(format!(
                    "phase bitmap '{}' truncated at length prefix",
                    label
                )));
            }
            let size = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + size > data.len() {
                return Err(StorageError::CorruptIndex(format!(
                    "phase bitmap '{}' truncated at payload",
                    label
                )));
            }
            let bitmap =
                RoaringBitmap::deserialize_from(&data[offset..offset + size]).map_err(|e| {
                    StorageError::CorruptIndex(format!(
                        "invalid roaring bitmap for '{}': {}",
                        label, e
                    ))
                })?;
            offset += size;
            bitmaps.push(bitmap);
        }
        // Ensure we have exactly 4 bitmaps.
        while bitmaps.len() < 4 {
            bitmaps.push(RoaringBitmap::new());
        }
        Ok(Self {
            full: bitmaps.remove(0),
            summary: bitmaps.remove(0),
            ghost: bitmaps.remove(0),
            tombstone: bitmaps.remove(0),
        })
    }

    /// Rebuild from a full META_TABLE scan. Used on startup when the
    /// persisted bitmap is missing or corrupt.
    pub fn rebuild_from_records(records: &[(u32, DecayPhase)]) -> Self {
        let mut index = Self::new();
        for &(slot, phase) in records {
            // Tombstoned records have had their vector slot freed, so
            // they should not be tracked in the bitmap. Skip them.
            if phase == DecayPhase::Tombstone {
                continue;
            }
            index.bitmap_mut(phase).insert(slot);
        }
        index
    }
}
