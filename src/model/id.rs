//! Type-safe newtype wrappers for memory and namespace identifiers.

use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════════
// MemoryId
// ═══════════════════════════════════════════════════════════════════════

/// Type-safe wrapper around a UUID v7 identifying a single memory record.
///
/// `Copy` because it is 16 bytes — cheap to pass by value.
/// `Ord` gives chronological ordering (UUID v7 bytes sort by time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryId(Uuid);

impl MemoryId {
    /// Generate a new time-ordered memory ID (UUID v7).
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID. No version check is performed — the caller
    /// is responsible for ensuring v7 semantics when that matters.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Return the inner UUID.
    pub fn into_inner(self) -> Uuid {
        self.0
    }

    /// Extract the creation timestamp embedded in the UUID v7 high bits.
    /// Returns milliseconds since Unix epoch.
    ///
    /// # Panics
    /// Panics if the inner UUID is not v7 (should never happen for IDs
    /// created via `MemoryId::new()`).
    pub fn created_at_millis(&self) -> i64 {
        let ts = self
            .0
            .get_timestamp()
            .expect("MemoryId should contain a v7 UUID with an embedded timestamp");
        let (secs, nanos) = ts.to_unix();
        (secs as i64) * 1000 + (nanos as i64) / 1_000_000
    }

    /// Return the 16-byte big-endian representation for on-disk storage.
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Reconstruct from 16 big-endian bytes (e.g., read from `meta.db`).
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }

    /// Return the nil (all-zeros) sentinel ID.
    ///
    /// Used as a placeholder when the concrete memory identity is not
    /// yet known (e.g., inside the FSRS engine when emitting a
    /// `DecayEvent::PermastoreAchieved`). The caller must replace
    /// it with the real ID before using the event externally.
    pub fn nil() -> Self {
        Self(Uuid::nil())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for MemoryId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for MemoryId {
    /// Lowercase hyphenated: `550e8400-e29b-71d4-a716-446655440000`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<Uuid> for MemoryId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl From<MemoryId> for Uuid {
    fn from(id: MemoryId) -> Self {
        id.0
    }
}

// ═══════════════════════════════════════════════════════════════════════
// NamespaceId
// ═══════════════════════════════════════════════════════════════════════

/// Type-safe wrapper around a `u32` identifying a namespace.
///
/// ID 0 is reserved as an "unset" sentinel in on-disk formats and must
/// never be assigned to a real namespace. Valid IDs start at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NamespaceId(u32);

impl NamespaceId {
    /// The reserved "unset" sentinel. Must never be used for a real
    /// namespace.
    pub const UNSET: Self = Self(0);

    /// Wrap a raw `u32`. Does not check for the reserved value — the
    /// namespace registry is responsible for never assigning ID 0.
    pub fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Return the raw `u32` value.
    pub fn get(self) -> u32 {
        self.0
    }

    /// True if this is the reserved sentinel value (0).
    pub fn is_unset(self) -> bool {
        self.0 == 0
    }
}

impl Deref for NamespaceId {
    type Target = u32;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ns:{}", self.0)
    }
}

impl From<u32> for NamespaceId {
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

impl From<NamespaceId> for u32 {
    fn from(id: NamespaceId) -> Self {
        id.0
    }
}
