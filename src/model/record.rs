//! On-disk (`DiskRecord`) and in-memory cache (`CachedRecord`)
//! representations with conversions between all record formats.

use crate::model::constants::*;
use crate::model::decay::DecayPhase;
use crate::model::error::DecodeError;
use crate::model::id::{MemoryId, NamespaceId};
use crate::model::memory::{AccessEvent, AccessKind, Memory};
use crate::model::tag::Tag;

// ═══════════════════════════════════════════════════════════════════════
// DiskRecord — on-disk representation in meta.db
// ═══════════════════════════════════════════════════════════════════════

/// The on-disk record stored in `meta.db`.
///
/// Fixed-layout fields first, variable-length fields last.
/// Every record is prefixed with a version byte for forward compatibility.
/// See Spec 01, section 3.3.
///
/// This struct is an intermediate form: `to_bytes()` / `from_bytes()`
/// convert to and from the raw binary representation. It is NOT a serde
/// struct — serialization is hand-written for schema evolution control.
#[derive(Debug, Clone)]
pub struct DiskRecord {
    // ── Header ───────────────────────────────────────────────────────
    /// Schema version. Current: 1.
    pub version: u8,

    // ── Fixed-size fields ────────────────────────────────────────────
    /// UUID bytes, big-endian.
    pub id: [u8; 16],
    /// Namespace ID, little-endian on disk.
    pub namespace_id: u32,
    /// Created-at timestamp (millis since epoch), little-endian.
    pub created_at: i64,
    /// Last-accessed-at timestamp (millis since epoch), little-endian.
    pub last_accessed_at: i64,
    /// Current decay phase.
    pub phase: DecayPhase,
    /// Raw FSRS retrievability, IEEE 754 f32.
    pub strength: f32,
    /// Effective retrievability with connection bonus, IEEE 754 f32.
    pub decay_strength: f32,
    /// FSRS stability in days, IEEE 754 f32.
    pub stability: f32,
    /// FSRS difficulty, IEEE 754 f32.
    pub difficulty: f32,
    /// 0 = false, 1 = true.
    pub is_permastore: u8,
    /// Index into the namespace's `vectors.dat`.
    pub vector_slot: u32,
    /// Cached outgoing edge count.
    pub edge_count: u16,

    // ── Variable-size fields ─────────────────────────────────────────
    /// Short description of the memory.
    pub summary: String,
    /// Validated tags.
    pub tags: Vec<Tag>,
    /// Recent access history.
    pub access_history: Vec<AccessEvent>,

    // ── Text pointer ─────────────────────────────────────────────────
    /// Byte offset into `text.log`. 0 = no full_text.
    pub text_offset: u64,
    /// Byte length in `text.log`. 0 = no full_text.
    pub text_length: u32,
}

/// Size in bytes of the fixed portion of a v1 DiskRecord:
///   1 (version) + 16 (id) + 4 (namespace_id) + 8 (created_at)
///   + 8 (last_accessed_at) + 1 (phase) + 4 (strength)
///   + 4 (decay_strength) + 4 (stability) + 4 (difficulty)
///   + 1 (is_permastore) + 4 (vector_slot) + 2 (edge_count)
///   + 8 (text_offset) + 4 (text_length)
///   = 73 bytes
const V1_FIXED_SIZE: usize = 73;

impl DiskRecord {
    /// Current on-disk schema version.
    pub const CURRENT_VERSION: u8 = DISK_RECORD_VERSION;

    /// Serialize to bytes for writing to `meta.db`.
    ///
    /// Layout:
    /// ```text
    /// [version: u8]
    /// [id: [u8; 16]]
    /// [namespace_id: u32 LE]
    /// [created_at: i64 LE]
    /// [last_accessed_at: i64 LE]
    /// [phase: u8]
    /// [strength: f32 LE]
    /// [decay_strength: f32 LE]
    /// [stability: f32 LE]
    /// [difficulty: f32 LE]
    /// [is_permastore: u8]
    /// [vector_slot: u32 LE]
    /// [edge_count: u16 LE]
    /// [text_offset: u64 LE]
    /// [text_length: u32 LE]
    /// [summary_len: u16 LE] [summary_bytes: [u8; summary_len]]
    /// [tag_count: u16 LE] ( [tag_len: u16 LE] [tag_bytes: ...] ) *
    /// [access_count: u16 LE] ( [timestamp: i64 LE] [kind: u8] ) *
    /// ```
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(512);

        // Fixed fields
        buf.push(self.version);
        buf.extend_from_slice(&self.id);
        buf.extend_from_slice(&self.namespace_id.to_le_bytes());
        buf.extend_from_slice(&self.created_at.to_le_bytes());
        buf.extend_from_slice(&self.last_accessed_at.to_le_bytes());
        buf.push(self.phase.as_u8());
        buf.extend_from_slice(&self.strength.to_le_bytes());
        buf.extend_from_slice(&self.decay_strength.to_le_bytes());
        buf.extend_from_slice(&self.stability.to_le_bytes());
        buf.extend_from_slice(&self.difficulty.to_le_bytes());
        buf.push(self.is_permastore);
        buf.extend_from_slice(&self.vector_slot.to_le_bytes());
        buf.extend_from_slice(&self.edge_count.to_le_bytes());
        buf.extend_from_slice(&self.text_offset.to_le_bytes());
        buf.extend_from_slice(&self.text_length.to_le_bytes());

        // Summary (u16 length prefix)
        let summary_bytes = self.summary.as_bytes();
        buf.extend_from_slice(&(summary_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(summary_bytes);

        // Tags (u16 count, then each: u16 length + bytes)
        buf.extend_from_slice(&(self.tags.len() as u16).to_le_bytes());
        for tag in &self.tags {
            let tag_bytes = tag.as_str().as_bytes();
            buf.extend_from_slice(&(tag_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(tag_bytes);
        }

        // Access history (u16 count, then each: i64 timestamp + u8 kind)
        buf.extend_from_slice(&(self.access_history.len() as u16).to_le_bytes());
        for event in &self.access_history {
            buf.extend_from_slice(&event.timestamp.to_le_bytes());
            buf.push(access_kind_to_u8(event.kind));
        }

        buf
    }

    /// Deserialize from bytes, handling version differences.
    pub fn from_bytes(data: &[u8]) -> Result<Self, DecodeError> {
        if data.is_empty() {
            return Err(DecodeError::Truncated {
                expected: 1,
                actual: 0,
            });
        }

        let version = data[0];
        match version {
            1 => Self::decode_v1(data),
            v => Err(DecodeError::UnknownVersion(v)),
        }
    }

    /// Decode a version-1 record from raw bytes.
    fn decode_v1(data: &[u8]) -> Result<Self, DecodeError> {
        if data.len() < V1_FIXED_SIZE {
            return Err(DecodeError::Truncated {
                expected: V1_FIXED_SIZE,
                actual: data.len(),
            });
        }

        let mut pos = 1; // skip version byte
        let version = data[0];

        // ID
        let mut id = [0u8; 16];
        id.copy_from_slice(&data[pos..pos + 16]);
        pos += 16;

        // Fixed numeric fields
        let namespace_id = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let created_at = i64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let last_accessed_at = i64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let phase_byte = data[pos];
        let phase = DecayPhase::from_u8(phase_byte).ok_or(DecodeError::InvalidPhase(phase_byte))?;
        pos += 1;

        let strength = f32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let decay_strength = f32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let stability = f32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let difficulty = f32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let is_permastore = data[pos];
        pos += 1;

        let vector_slot = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let edge_count = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap());
        pos += 2;

        let text_offset = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let text_length = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        // Variable: summary
        let summary = read_length_prefixed_string(data, &mut pos, "summary")?;

        // Variable: tags
        let tag_count = read_u16(data, &mut pos, "tag_count")?;
        let mut tags = Vec::with_capacity(tag_count as usize);
        for _ in 0..tag_count {
            let tag_str = read_length_prefixed_string(data, &mut pos, "tag")?;
            // Use Tag::new for validation; corrupt tags are a fatal
            // decode error (invalid UTF-8 is caught by the string read).
            match Tag::new(tag_str) {
                Ok(tag) => tags.push(tag),
                Err(_) => {
                    // Tag validation failed but UTF-8 was valid.
                    // Skip this tag silently rather than failing the
                    // entire record — this is a recoverable corruption.
                }
            }
        }

        // Variable: access history
        let access_count = read_u16(data, &mut pos, "access_count")?;
        let mut access_history = Vec::with_capacity(access_count as usize);
        for _ in 0..access_count {
            if pos + 9 > data.len() {
                return Err(DecodeError::Truncated {
                    expected: pos + 9,
                    actual: data.len(),
                });
            }
            let timestamp = i64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let kind = access_kind_from_u8(data[pos]);
            pos += 1;
            access_history.push(AccessEvent { timestamp, kind });
        }

        Ok(DiskRecord {
            version,
            id,
            namespace_id,
            created_at,
            last_accessed_at,
            phase,
            strength,
            decay_strength,
            stability,
            difficulty,
            is_permastore,
            vector_slot,
            edge_count,
            summary,
            tags,
            access_history,
            text_offset,
            text_length,
        })
    }
}

// ── Helper functions for binary decoding ─────────────────────────────

/// Read a little-endian u16 from the buffer at the given position.
fn read_u16(data: &[u8], pos: &mut usize, field: &'static str) -> Result<u16, DecodeError> {
    if *pos + 2 > data.len() {
        return Err(DecodeError::FieldOverflow {
            field,
            declared: 2,
            available: data.len() - *pos,
        });
    }
    let val = u16::from_le_bytes(data[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    Ok(val)
}

/// Read a u16-length-prefixed UTF-8 string from the buffer.
fn read_length_prefixed_string(
    data: &[u8],
    pos: &mut usize,
    field: &'static str,
) -> Result<String, DecodeError> {
    let len = read_u16(data, pos, field)? as usize;
    if *pos + len > data.len() {
        return Err(DecodeError::FieldOverflow {
            field,
            declared: len,
            available: data.len() - *pos,
        });
    }
    let bytes = data[*pos..*pos + len].to_vec();
    *pos += len;
    String::from_utf8(bytes).map_err(|e| DecodeError::InvalidUtf8 { field, source: e })
}

/// Convert an `AccessKind` to its on-disk u8 discriminant.
fn access_kind_to_u8(kind: AccessKind) -> u8 {
    match kind {
        AccessKind::DirectRetrieval => 1,
        AccessKind::AssociativeRetrieval => 2,
        AccessKind::DecaySweep => 3,
        AccessKind::ManualReinforcement => 4,
    }
}

/// Convert a u8 discriminant to an `AccessKind`, defaulting to
/// `DirectRetrieval` for unknown values (recoverable).
fn access_kind_from_u8(val: u8) -> AccessKind {
    match val {
        1 => AccessKind::DirectRetrieval,
        2 => AccessKind::AssociativeRetrieval,
        3 => AccessKind::DecaySweep,
        4 => AccessKind::ManualReinforcement,
        // Default to DirectRetrieval for unknown values (recoverable).
        _ => AccessKind::DirectRetrieval,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CachedRecord — in-memory representation held in moka cache
// ═══════════════════════════════════════════════════════════════════════

/// The in-memory representation held in the moka cache.
///
/// Optimized for fast access, not serialization. Owns all its data
/// (no borrows from disk pages). Does NOT contain:
/// - `full_text` — too large to cache; loaded on demand from `text.log`.
/// - `embedding` — lives in the mmap'd `vectors.dat`, managed by the OS
///   page cache.
/// - `access_history` — rarely needed for queries; loaded on demand.
///
/// Estimated size per entry: ~634 bytes (see Spec 01 section 3.4).
#[derive(Debug, Clone)]
pub struct CachedRecord {
    /// Unique identifier.
    pub id: MemoryId,
    /// Namespace this memory belongs to.
    pub namespace_id: NamespaceId,
    /// Created-at timestamp (millis since epoch).
    pub created_at: i64,
    /// Last-accessed-at timestamp (millis since epoch).
    pub last_accessed_at: i64,
    /// Current decay phase.
    pub phase: DecayPhase,
    /// Raw FSRS retrievability.
    pub strength: f32,
    /// Effective retrievability with connection bonus.
    pub decay_strength: f32,
    /// FSRS stability in days.
    pub stability: f32,
    /// FSRS difficulty.
    pub difficulty: f32,
    /// Whether stability exceeds the permastore threshold.
    pub is_permastore: bool,
    /// Short description.
    pub summary: String,
    /// Validated tags.
    pub tags: Vec<Tag>,
    /// Cached outgoing edge count.
    pub edge_count: u16,
    /// Index into the namespace's vectors.dat.
    pub vector_slot: u32,
    /// Named entities extracted from the summary (derived at load time).
    pub entities: Vec<String>,
}

impl CachedRecord {
    /// Approximate size in bytes for moka cache weight estimation.
    ///
    /// This is a best-effort estimate: it accounts for the struct's
    /// fixed fields, the heap-allocated summary string, the tags
    /// vector, and the entities vector. It intentionally overestimates
    /// slightly (includes allocator overhead) to avoid cache overflows.
    pub fn estimated_size(&self) -> u32 {
        let fixed = std::mem::size_of::<Self>() as u32;
        let summary_heap = self.summary.len() as u32;
        let tags_heap: u32 = self
            .tags
            .iter()
            .map(|t| t.as_str().len() as u32 + 24) // String header
            .sum();
        let entities_heap: u32 = self
            .entities
            .iter()
            .map(|e| e.len() as u32 + 24) // String header
            .sum();
        let overhead = 64; // HashMap + allocator overhead

        fixed + summary_heap + tags_heap + entities_heap + overhead
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Conversions
// ═══════════════════════════════════════════════════════════════════════

impl From<&Memory> for DiskRecord {
    /// Convert an API Memory to a DiskRecord for persistence.
    ///
    /// The caller must set `text_offset` and `text_length` after writing
    /// full_text to `text.log`, and `vector_slot` after writing the
    /// embedding to `vectors.dat`. This conversion initializes both to
    /// zero.
    ///
    /// The `namespace` string field on Memory is not converted here —
    /// the caller must resolve it to a `namespace_id` via the namespace
    /// registry.
    fn from(m: &Memory) -> Self {
        DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: *m.id.as_bytes(),
            namespace_id: 0, // Must be set by caller from namespace registry
            created_at: m.created_at,
            last_accessed_at: m.last_accessed_at,
            phase: m.phase,
            strength: m.strength,
            decay_strength: m.decay_strength,
            stability: m.stability,
            difficulty: m.difficulty,
            is_permastore: if m.is_permastore { 1 } else { 0 },
            vector_slot: 0, // Must be set by caller after vector write
            edge_count: m.edge_count,
            summary: m.summary.clone(),
            tags: m.tags.clone(),
            access_history: m.access_history.clone().unwrap_or_default(),
            text_offset: 0, // Must be set by caller after text.log write
            text_length: 0, // ditto
        }
    }
}

impl From<&DiskRecord> for CachedRecord {
    /// Convert a DiskRecord to a CachedRecord for insertion into the
    /// moka cache. Drops full_text pointer and access_history (not
    /// cached). Parses entities from stored tags (entity/ prefix).
    fn from(d: &DiskRecord) -> Self {
        let entities = crate::model::parse_structured_tags(&d.tags).entities;
        CachedRecord {
            id: MemoryId::from_bytes(d.id),
            namespace_id: NamespaceId::new(d.namespace_id),
            created_at: d.created_at,
            last_accessed_at: d.last_accessed_at,
            phase: d.phase,
            strength: d.strength,
            decay_strength: d.decay_strength,
            stability: d.stability,
            difficulty: d.difficulty,
            is_permastore: d.is_permastore != 0,
            summary: d.summary.clone(),
            tags: d.tags.clone(),
            edge_count: d.edge_count,
            vector_slot: d.vector_slot,
            entities,
        }
    }
}

impl CachedRecord {
    /// Hydrate into an API `Memory` struct for returning to callers.
    ///
    /// The `namespace_name` must be resolved by the caller from the
    /// namespace registry. Fields not held in the cache (`full_text`,
    /// `embedding`, `access_history`) are set to `None`.
    pub fn to_memory(&self, namespace_name: String) -> Memory {
        Memory {
            id: self.id,
            namespace: namespace_name,
            created_at: self.created_at,
            last_accessed_at: self.last_accessed_at,
            summary: self.summary.clone(),
            full_text: None, // loaded on demand
            tags: self.tags.clone(),
            phase: self.phase,
            strength: self.strength,
            decay_strength: self.decay_strength,
            stability: self.stability,
            difficulty: self.difficulty,
            is_permastore: self.is_permastore,
            edge_count: self.edge_count,
            embedding: None,      // lives in mmap'd vectors.dat
            access_history: None, // loaded on demand
        }
    }
}
