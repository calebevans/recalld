//! Secondary index data structures for the metadata store.
//!
//! Contains `PhaseIndex` — roaring bitmap indexes tracking which vector
//! slots belong to each decay phase, scoped per namespace to avoid
//! cross-namespace collisions.
//!
//! Managed internally by `MetadataStore`; persisted to `INDEX_TABLE`
//! in meta.db.
//!
//! See CS-07 Section 6.

use std::collections::HashMap;

use roaring::RoaringBitmap;

use crate::model::DecayPhase;
use crate::storage::error::StorageError;

// ═══════════════════════════════════════════════════════════════════════
// NamespaceBitmaps — per-namespace set of phase bitmaps
// ═══════════════════════════════════════════════════════════════════════

/// Four roaring bitmaps (one per decay phase) for a single namespace.
///
/// Private to this module; external callers interact through `PhaseIndex`.
struct NamespaceBitmaps {
    /// Phase 1: full text + embedding.
    full: RoaringBitmap,
    /// Phase 2: summary + embedding.
    summary: RoaringBitmap,
    /// Phase 3: embedding only.
    ghost: RoaringBitmap,
    /// Phase 4: tombstone — content stripped, graph preserved.
    tombstone: RoaringBitmap,
}

impl NamespaceBitmaps {
    fn new() -> Self {
        Self {
            full: RoaringBitmap::new(),
            summary: RoaringBitmap::new(),
            ghost: RoaringBitmap::new(),
            tombstone: RoaringBitmap::new(),
        }
    }

    fn bitmap(&self, phase: DecayPhase) -> &RoaringBitmap {
        match phase {
            DecayPhase::Full => &self.full,
            DecayPhase::Summary => &self.summary,
            DecayPhase::Ghost => &self.ghost,
            DecayPhase::Tombstone => &self.tombstone,
        }
    }

    fn bitmap_mut(&mut self, phase: DecayPhase) -> &mut RoaringBitmap {
        match phase {
            DecayPhase::Full => &mut self.full,
            DecayPhase::Summary => &mut self.summary,
            DecayPhase::Ghost => &mut self.ghost,
            DecayPhase::Tombstone => &mut self.tombstone,
        }
    }

    fn is_empty(&self) -> bool {
        self.full.is_empty()
            && self.summary.is_empty()
            && self.ghost.is_empty()
            && self.tombstone.is_empty()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// PhaseIndex
// ═══════════════════════════════════════════════════════════════════════

/// In-memory bitmap index tracking which vector slots belong to each
/// decay phase, partitioned by namespace.
///
/// The authoritative copy lives here in RAM; it is serialized to
/// meta.db `INDEX_TABLE` periodically and rebuilt from `META_TABLE`
/// on startup if missing or corrupt.
///
/// Stores `(namespace_id, vector_slot)` pairs. Each namespace gets
/// its own set of four `RoaringBitmap`s (one per `DecayPhase`) to
/// avoid cross-namespace collisions — different namespaces allocate
/// vector slots independently and can share the same slot numbers.
pub struct PhaseIndex {
    /// Bitmaps keyed by namespace_id (u32).
    namespaces: HashMap<u32, NamespaceBitmaps>,
}

/// Serialization format version for namespace-aware bitmaps.
///
/// The v1 format (pre-namespace) started with a u32 bitmap size that
/// was always >= 8 (the minimum serialized size of an empty
/// `RoaringBitmap`). Version 2 uses `2u32` as a distinguishing
/// marker in the first 4 bytes, which cannot collide with v1.
const FORMAT_VERSION: u32 = 2;

impl PhaseIndex {
    /// Create an empty phase index.
    pub fn new() -> Self {
        Self {
            namespaces: HashMap::new(),
        }
    }

    /// Add a new memory (always starts in Phase::Full).
    pub fn insert(&mut self, namespace_id: u32, slot: u32) {
        self.namespaces
            .entry(namespace_id)
            .or_insert_with(NamespaceBitmaps::new)
            .bitmap_mut(DecayPhase::Full)
            .insert(slot);
    }

    /// Move a memory between phases.
    pub fn transition(
        &mut self,
        namespace_id: u32,
        slot: u32,
        from: DecayPhase,
        to: DecayPhase,
    ) {
        let ns = self
            .namespaces
            .entry(namespace_id)
            .or_insert_with(NamespaceBitmaps::new);
        ns.bitmap_mut(from).remove(slot);
        ns.bitmap_mut(to).insert(slot);
    }

    /// Remove a memory entirely (after deletion).
    pub fn remove(&mut self, namespace_id: u32, slot: u32, phase: DecayPhase) {
        if let Some(ns) = self.namespaces.get_mut(&namespace_id) {
            ns.bitmap_mut(phase).remove(slot);
        }
    }

    /// Return all `(namespace_id, vector_slot)` pairs in the given
    /// phase across all namespaces.
    pub fn all_slots_in_phase(&self, phase: DecayPhase) -> Vec<(u32, u32)> {
        let mut result = Vec::new();
        for (&ns_id, ns_bitmaps) in &self.namespaces {
            for slot in ns_bitmaps.bitmap(phase).iter() {
                result.push((ns_id, slot));
            }
        }
        result
    }

    /// Count of memories in a given phase (across all namespaces).
    pub fn count(&self, phase: DecayPhase) -> u64 {
        self.namespaces
            .values()
            .map(|ns| ns.bitmap(phase).len())
            .sum()
    }

    /// Serialize all bitmaps into a single byte buffer.
    ///
    /// Format v2 (namespace-aware):
    /// ```text
    /// [u32 version = 2]
    /// [u32 namespace_count]
    /// for each namespace (sorted by namespace_id for determinism):
    ///   [u32 namespace_id]
    ///   [u32 size_full]  [full bitmap bytes]
    ///   [u32 size_summary] [summary bitmap bytes]
    ///   [u32 size_ghost] [ghost bitmap bytes]
    ///   [u32 size_tombstone] [tombstone bitmap bytes]
    /// ```
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Version marker.
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());

        // Filter out empty namespaces to keep the serialized form compact.
        let mut sorted_ns: Vec<(&u32, &NamespaceBitmaps)> = self
            .namespaces
            .iter()
            .filter(|(_, ns)| !ns.is_empty())
            .collect();
        sorted_ns.sort_by_key(|(id, _)| **id);

        buf.extend_from_slice(&(sorted_ns.len() as u32).to_le_bytes());

        for (ns_id, ns_bitmaps) in &sorted_ns {
            buf.extend_from_slice(&ns_id.to_le_bytes());
            for bitmap in [
                &ns_bitmaps.full,
                &ns_bitmaps.summary,
                &ns_bitmaps.ghost,
                &ns_bitmaps.tombstone,
            ] {
                let size = bitmap.serialized_size();
                buf.extend_from_slice(&(size as u32).to_le_bytes());
                bitmap
                    .serialize_into(&mut buf)
                    .expect("bitmap serialization is infallible");
            }
        }

        buf
    }

    /// Deserialize from bytes produced by `to_bytes()`.
    ///
    /// Detects the format version:
    /// - Version 2 (namespace-aware): parsed in full.
    /// - Version 1 (pre-namespace, first u32 >= 8): returns an empty
    ///   `PhaseIndex`. The caller's startup validation will rebuild
    ///   from `META_TABLE` via `rebuild_from_records`.
    pub fn from_bytes(data: &[u8]) -> Result<Self, StorageError> {
        if data.len() < 4 {
            return Err(StorageError::CorruptIndex(
                "phase index data too short".into(),
            ));
        }

        let version = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if version == FORMAT_VERSION {
            Self::from_bytes_v2(data)
        } else {
            // Pre-namespace format. Return empty; rebuild_phase_index()
            // will reconstruct correctly during startup validation.
            Ok(Self::new())
        }
    }

    /// Parse the v2 namespace-aware format.
    fn from_bytes_v2(data: &[u8]) -> Result<Self, StorageError> {
        let mut offset = 4; // skip version marker

        if offset + 4 > data.len() {
            return Err(StorageError::CorruptIndex(
                "phase index v2 truncated at namespace count".into(),
            ));
        }
        let ns_count =
            u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        let mut namespaces = HashMap::with_capacity(ns_count);
        let labels = ["full", "summary", "ghost", "tombstone"];

        for _ in 0..ns_count {
            if offset + 4 > data.len() {
                return Err(StorageError::CorruptIndex(
                    "phase index v2 truncated at namespace id".into(),
                ));
            }
            let ns_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;

            let mut bitmaps = Vec::with_capacity(4);
            for label in &labels {
                if offset + 4 > data.len() {
                    return Err(StorageError::CorruptIndex(format!(
                        "phase index v2 truncated at '{}' bitmap size for ns {}",
                        label, ns_id
                    )));
                }
                let size = u32::from_le_bytes(
                    data[offset..offset + 4].try_into().unwrap(),
                ) as usize;
                offset += 4;

                if offset + size > data.len() {
                    return Err(StorageError::CorruptIndex(format!(
                        "phase index v2 truncated at '{}' bitmap payload for ns {}",
                        label, ns_id
                    )));
                }
                let bitmap = RoaringBitmap::deserialize_from(
                    &data[offset..offset + size],
                )
                .map_err(|e| {
                    StorageError::CorruptIndex(format!(
                        "invalid roaring bitmap for '{}' in ns {}: {}",
                        label, ns_id, e
                    ))
                })?;
                offset += size;
                bitmaps.push(bitmap);
            }

            namespaces.insert(
                ns_id,
                NamespaceBitmaps {
                    full: bitmaps.remove(0),
                    summary: bitmaps.remove(0),
                    ghost: bitmaps.remove(0),
                    tombstone: bitmaps.remove(0),
                },
            );
        }

        Ok(Self { namespaces })
    }

    /// Rebuild from a full META_TABLE scan. Used on startup when the
    /// persisted bitmap is missing or corrupt.
    ///
    /// Each tuple is `(namespace_id, vector_slot, phase)`.
    pub fn rebuild_from_records(records: &[(u32, u32, DecayPhase)]) -> Self {
        let mut index = Self::new();
        for &(namespace_id, slot, phase) in records {
            // Tombstoned records have had their vector slot freed, so
            // they should not be tracked in the bitmap. Skip them.
            if phase == DecayPhase::Tombstone {
                continue;
            }
            index
                .namespaces
                .entry(namespace_id)
                .or_insert_with(NamespaceBitmaps::new)
                .bitmap_mut(phase)
                .insert(slot);
        }
        index
    }
}
