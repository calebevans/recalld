//! Custom binary serialization for `DiskRecord`.
//!
//! Implements the version-prefixed, magic-byte-validated binary format
//! described in CS-02 for on-disk `meta.db` storage. All multi-byte
//! values are little-endian. No external crates are used beyond `std`.

use crate::model::memory::{AccessEvent, AccessKind};
use crate::model::record::DiskRecord;
use crate::model::tag::Tag;

use super::{
    DecodeError, EncodeError, CURRENT_SCHEMA_VERSION, MAX_SUPPORTED_VERSION, RECORD_MAGIC,
    V1_FIXED_SIZE,
};

/// Serializes a `DiskRecord` into the Recalld binary format.
///
/// Returns a `Vec<u8>` containing the complete binary record ready
/// for writing to `meta.db`. The buffer is pre-allocated to an
/// estimated size to minimize reallocations.
///
/// # Errors
///
/// Returns `EncodeError` if:
/// - Any float field is NaN or infinity
/// - The summary exceeds 65,535 bytes
/// - Tag count exceeds 64
/// - Access history count exceeds 32
///
/// # Performance
///
/// Target: < 500 ns for a typical record (~500 bytes output).
/// The function performs one allocation (the output `Vec`) and
/// no syscalls.
pub fn encode_record(record: &DiskRecord) -> Result<Vec<u8>, EncodeError> {
    // -- Validate float fields --
    validate_finite(record.strength, "strength")?;
    validate_finite(record.decay_strength, "decay_strength")?;
    validate_finite(record.stability, "stability")?;
    validate_finite(record.difficulty, "difficulty")?;

    // -- Validate counts --
    if record.tags.len() > 64 {
        return Err(EncodeError::TooManyTags {
            count: record.tags.len(),
        });
    }
    if record.access_history.len() > 32 {
        return Err(EncodeError::TooManyAccessEvents {
            count: record.access_history.len(),
        });
    }

    let summary_bytes = record.summary.as_bytes();
    if summary_bytes.len() > u16::MAX as usize {
        return Err(EncodeError::SummaryTooLong {
            len: summary_bytes.len(),
        });
    }

    // -- Pre-calculate variable section size for capacity hint --
    let tag_payload_size: usize = record
        .tags
        .iter()
        .map(|t| 2 + t.as_str().len()) // u16 len prefix + tag bytes
        .sum();
    let access_payload_size = record.access_history.len() * 9; // i64 + u8

    let estimated_size = V1_FIXED_SIZE
        + 2
        + summary_bytes.len() // u16 prefix + summary
        + 2
        + tag_payload_size // u16 count + tags
        + 2
        + access_payload_size; // u16 count + access events

    let mut buf = Vec::with_capacity(estimated_size);

    // -- Fixed section --
    buf.extend_from_slice(&RECORD_MAGIC);
    buf.push(CURRENT_SCHEMA_VERSION);
    buf.extend_from_slice(&record.id); // 16 bytes, big-endian UUID
    buf.extend_from_slice(&record.namespace_id.to_le_bytes());
    buf.extend_from_slice(&record.created_at.to_le_bytes());
    buf.extend_from_slice(&record.last_accessed_at.to_le_bytes());
    buf.push(record.phase);
    buf.extend_from_slice(&record.strength.to_le_bytes());
    buf.extend_from_slice(&record.decay_strength.to_le_bytes());
    buf.extend_from_slice(&record.stability.to_le_bytes());
    buf.extend_from_slice(&record.difficulty.to_le_bytes());
    buf.push(record.is_permastore);
    buf.extend_from_slice(&record.vector_slot.to_le_bytes());
    buf.extend_from_slice(&record.edge_count.to_le_bytes());
    buf.extend_from_slice(&record.text_offset.to_le_bytes());
    buf.extend_from_slice(&record.text_length.to_le_bytes());

    // -- Variable section: summary --
    buf.extend_from_slice(&(summary_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(summary_bytes);

    // -- Variable section: tags --
    buf.extend_from_slice(&(record.tags.len() as u16).to_le_bytes());
    for (i, tag) in record.tags.iter().enumerate() {
        let tag_bytes = tag.as_str().as_bytes();
        if tag_bytes.len() > u16::MAX as usize {
            return Err(EncodeError::TagTooLong {
                index: i,
                len: tag_bytes.len(),
            });
        }
        buf.extend_from_slice(&(tag_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(tag_bytes);
    }

    // -- Variable section: access history --
    buf.extend_from_slice(&(record.access_history.len() as u16).to_le_bytes());
    for event in &record.access_history {
        buf.extend_from_slice(&event.timestamp.to_le_bytes());
        buf.push(encode_access_kind(event.kind));
    }

    Ok(buf)
}

/// Deserializes a `DiskRecord` from a byte slice in Recalld binary format.
///
/// Handles schema version dispatch -- currently only version 1 is
/// supported. Future versions will add match arms to this function,
/// each calling a version-specific decoder that applies defaults
/// for any fields absent in older formats.
///
/// # Errors
///
/// Returns `DecodeError` for any structural or content violation.
/// See `DecodeError` variants for the full list.
///
/// # Performance
///
/// Target: < 500 ns for a typical record (~500 bytes input).
/// The function allocates owned `String` and `Vec` values; it
/// does not borrow from the input slice.
pub fn decode_record(bytes: &[u8]) -> Result<DiskRecord, DecodeError> {
    // -- Minimum size check --
    if bytes.len() < V1_FIXED_SIZE {
        return Err(DecodeError::Truncated {
            actual: bytes.len(),
            expected: V1_FIXED_SIZE,
        });
    }

    // -- Magic validation --
    let magic = [bytes[0], bytes[1]];
    if magic != RECORD_MAGIC {
        return Err(DecodeError::InvalidMagic { found: magic });
    }

    // -- Version dispatch --
    let version = bytes[2];
    if version > MAX_SUPPORTED_VERSION {
        return Err(DecodeError::UnsupportedVersion {
            found: version,
            max_supported: MAX_SUPPORTED_VERSION,
        });
    }

    match version {
        1 => decode_v1(bytes),
        // Future: 2 => decode_v2(bytes),
        _ => Err(DecodeError::UnsupportedVersion {
            found: version,
            max_supported: MAX_SUPPORTED_VERSION,
        }),
    }
}

/// Decodes a version-1 binary record.
///
/// Uses a cursor (`pos`) to walk through the byte slice. Fixed
/// fields are read at known offsets; variable fields are read
/// sequentially with length prefixes.
fn decode_v1(bytes: &[u8]) -> Result<DiskRecord, DecodeError> {
    let mut pos: usize = 3; // skip magic + version

    // -- Fixed fields --
    let id: [u8; 16] = bytes[pos..pos + 16]
        .try_into()
        .expect("slice is exactly 16 bytes");
    pos += 16;

    let namespace_id = read_u32_le(bytes, &mut pos);
    let created_at = read_i64_le(bytes, &mut pos);
    let last_accessed_at = read_i64_le(bytes, &mut pos);

    let phase_byte = bytes[pos];
    pos += 1;
    if phase_byte < 1 || phase_byte > 3 {
        return Err(DecodeError::InvalidPhase { byte: phase_byte });
    }

    let strength = read_f32_le(bytes, &mut pos, "strength")?;
    let decay_strength = read_f32_le(bytes, &mut pos, "decay_strength")?;
    let stability = read_f32_le(bytes, &mut pos, "stability")?;
    let difficulty = read_f32_le(bytes, &mut pos, "difficulty")?;

    let is_permastore_byte = bytes[pos];
    pos += 1;
    if is_permastore_byte > 1 {
        return Err(DecodeError::InvalidBool {
            field: "is_permastore",
            byte: is_permastore_byte,
        });
    }

    let vector_slot = read_u32_le(bytes, &mut pos);
    let edge_count = read_u16_le(bytes, &mut pos);
    let text_offset = read_u64_le(bytes, &mut pos);
    let text_length = read_u32_le(bytes, &mut pos);

    // pos should now equal V1_FIXED_SIZE (75)
    debug_assert_eq!(pos, V1_FIXED_SIZE);

    // -- Variable section: summary --
    let summary = read_length_prefixed_string(bytes, &mut pos, "summary")?;

    // -- Variable section: tags --
    let tag_count = read_u16_le_checked(bytes, &mut pos, "tag_count")?;
    if tag_count > 64 {
        return Err(DecodeError::TooManyTags { count: tag_count });
    }
    let mut tags = Vec::with_capacity(tag_count as usize);
    for _ in 0..tag_count {
        let tag_str = read_length_prefixed_string(bytes, &mut pos, "tag")?;
        // Tags from disk are already validated and lowercased.
        // Wrap directly without re-validation.
        tags.push(Tag::from_trusted(tag_str));
    }

    // -- Variable section: access history --
    let access_count = read_u16_le_checked(bytes, &mut pos, "access_count")?;
    if access_count > 32 {
        return Err(DecodeError::TooManyAccessEvents {
            count: access_count,
        });
    }
    let mut access_history = Vec::with_capacity(access_count as usize);
    for _ in 0..access_count {
        if pos + 9 > bytes.len() {
            return Err(DecodeError::FieldOverflow {
                field: "access_event",
                declared_len: 9,
                available: bytes.len() - pos,
            });
        }
        let timestamp = read_i64_le(bytes, &mut pos);
        let kind_byte = bytes[pos];
        pos += 1;
        let kind = decode_access_kind(kind_byte)?;
        access_history.push(AccessEvent { timestamp, kind });
    }

    Ok(DiskRecord {
        version: 1,
        id,
        namespace_id,
        created_at,
        last_accessed_at,
        phase: phase_byte,
        strength,
        decay_strength,
        stability,
        difficulty,
        is_permastore: is_permastore_byte,
        vector_slot,
        edge_count,
        summary,
        tags,
        access_history,
        text_offset,
        text_length,
    })
}

// ═══════════════════════════════════════════════════════════════════════
// Private helpers
// ═══════════════════════════════════════════════════════════════════════

/// Validate that a float value is finite (not NaN or infinity).
fn validate_finite(value: f32, field: &'static str) -> Result<(), EncodeError> {
    if !value.is_finite() {
        return Err(EncodeError::NonFiniteFloat { field });
    }
    Ok(())
}

/// Map an `AccessKind` to its on-disk u8 discriminant.
fn encode_access_kind(kind: AccessKind) -> u8 {
    match kind {
        AccessKind::DirectRetrieval => 1,
        AccessKind::AssociativeRetrieval => 2,
        AccessKind::DecaySweep => 3,
        AccessKind::ManualReinforcement => 4,
    }
}

/// Map a u8 discriminant back to an `AccessKind`.
fn decode_access_kind(byte: u8) -> Result<AccessKind, DecodeError> {
    match byte {
        1 => Ok(AccessKind::DirectRetrieval),
        2 => Ok(AccessKind::AssociativeRetrieval),
        3 => Ok(AccessKind::DecaySweep),
        4 => Ok(AccessKind::ManualReinforcement),
        b => Err(DecodeError::InvalidAccessKind { byte: b }),
    }
}

// ── Primitive readers ───────────────────────────────────────────────
// Each advances `pos` by the number of bytes consumed.

/// Read a little-endian u16 from the buffer.
fn read_u16_le(bytes: &[u8], pos: &mut usize) -> u16 {
    let val = u16::from_le_bytes(bytes[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    val
}

/// Read a little-endian u16 with bounds checking.
fn read_u16_le_checked(
    bytes: &[u8],
    pos: &mut usize,
    field: &'static str,
) -> Result<u16, DecodeError> {
    if *pos + 2 > bytes.len() {
        return Err(DecodeError::FieldOverflow {
            field,
            declared_len: 2,
            available: bytes.len() - *pos,
        });
    }
    Ok(read_u16_le(bytes, pos))
}

/// Read a little-endian u32 from the buffer.
fn read_u32_le(bytes: &[u8], pos: &mut usize) -> u32 {
    let val = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    val
}

/// Read a little-endian u64 from the buffer.
fn read_u64_le(bytes: &[u8], pos: &mut usize) -> u64 {
    let val = u64::from_le_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    val
}

/// Read a little-endian i64 from the buffer.
fn read_i64_le(bytes: &[u8], pos: &mut usize) -> i64 {
    let val = i64::from_le_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    val
}

/// Read a little-endian f32 from the buffer, rejecting non-finite values.
fn read_f32_le(
    bytes: &[u8],
    pos: &mut usize,
    field: &'static str,
) -> Result<f32, DecodeError> {
    let val = f32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    if !val.is_finite() {
        return Err(DecodeError::NonFiniteFloat { field, value: val });
    }
    Ok(val)
}

/// Reads a u16 length prefix followed by that many bytes, interpreted as UTF-8.
fn read_length_prefixed_string(
    bytes: &[u8],
    pos: &mut usize,
    field: &'static str,
) -> Result<String, DecodeError> {
    let len = read_u16_le_checked(bytes, pos, field)? as usize;
    if *pos + len > bytes.len() {
        return Err(DecodeError::FieldOverflow {
            field,
            declared_len: len,
            available: bytes.len() - *pos,
        });
    }
    let s = std::str::from_utf8(&bytes[*pos..*pos + len])
        .map_err(|_| DecodeError::InvalidUtf8 { field })?;
    *pos += len;
    Ok(s.to_owned())
}
