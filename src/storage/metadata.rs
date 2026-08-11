//! Metadata persistence backed by redb (meta.db).
//!
//! Owns the redb `Database` handle for meta.db, provides CRUD
//! operations for memory records and namespace configurations,
//! and manages secondary indexes (phase bitmaps, tag index,
//! namespace index).
//!
//! See CS-07 for the full specification.

use std::collections::HashMap;
use std::path::Path;

use redb::{
    Database, MultimapTableDefinition, ReadableMultimapTable, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};

use crate::model::constants::ACCESS_HISTORY_MAX;
use crate::model::{
    AccessEvent, AccessKind, DecayPhase, DiskRecord, MemoryId, NamespaceConfig, NamespaceId,
};
use crate::storage::error::StorageError;
use crate::storage::indexes::PhaseIndex;

// ═══════════════════════════════════════════════════════════════════════
// redb Table Definitions
// ═══════════════════════════════════════════════════════════════════════

/// Primary metadata table.
/// Key: UUID v7 bytes ([u8; 16], big-endian -- chronological sort).
/// Value: DiskRecord::to_bytes() output (variable-length, version-prefixed).
const META_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("memories");

/// Namespace configuration table.
/// Key: NamespaceId as u32.
/// Value: serde_json::to_vec(&NamespaceConfig) bytes.
const NAMESPACE_TABLE: TableDefinition<u32, &[u8]> = TableDefinition::new("namespaces");

/// Monotonic counter for namespace ID assignment.
/// Single row: key = "max_id", value = highest assigned u32.
const NAMESPACE_COUNTER: TableDefinition<&str, u32> = TableDefinition::new("namespace_counter");

/// Serialized secondary index blobs (phase bitmaps).
/// Key: index name string. Value: serialized bytes.
const INDEX_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("indexes");

/// Tag inverted index. One tag -> many memory UUIDs.
/// Key: lowercased tag string. Value: 16-byte UUID.
const TAG_INDEX: MultimapTableDefinition<&str, &[u8]> = MultimapTableDefinition::new("tag_index");

/// Namespace inverted index. One namespace_id -> many memory UUIDs.
/// Key: NamespaceId as u32. Value: 16-byte UUID.
const NAMESPACE_INDEX: MultimapTableDefinition<u32, &[u8]> =
    MultimapTableDefinition::new("namespace_index");

// ═══════════════════════════════════════════════════════════════════════
// MetadataStore
// ═══════════════════════════════════════════════════════════════════════

/// Owns the redb Database handle for meta.db.
///
/// All methods take `&self` -- redb handles internal locking for
/// concurrent read transactions and exclusive write transactions.
pub struct MetadataStore {
    db: Database,
    /// In-memory phase bitmap index. Authoritative copy; persisted
    /// periodically to INDEX_TABLE under key "phase_bitmaps".
    phase_index: std::sync::RwLock<PhaseIndex>,
}

// ── Constructor ─────────────────────────────────────────────────────

impl MetadataStore {
    /// Open or create meta.db at the given path.
    ///
    /// - Sets a 16 MB read cache (covers top B+tree levels).
    /// - Creates all tables on first open.
    /// - Loads or rebuilds the phase index from persisted bitmaps.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let db = redb::Builder::new()
            .set_cache_size(16 * 1024 * 1024)
            .create(path)?;

        // Force-create all tables by opening them in a write transaction.
        {
            let write_txn = db.begin_write()?;
            let _ = write_txn.open_table(META_TABLE)?;
            let _ = write_txn.open_table(NAMESPACE_TABLE)?;
            let _ = write_txn.open_table(NAMESPACE_COUNTER)?;
            let _ = write_txn.open_table(INDEX_TABLE)?;
            let _ = write_txn.open_multimap_table(TAG_INDEX)?;
            let _ = write_txn.open_multimap_table(NAMESPACE_INDEX)?;
            write_txn.commit()?;
        }

        // Load persisted phase index, or create empty.
        let phase_index = Self::load_phase_index(&db)?;

        Ok(Self {
            db,
            phase_index: std::sync::RwLock::new(phase_index),
        })
    }

    /// Load phase bitmaps from INDEX_TABLE. Returns empty PhaseIndex
    /// if the key does not exist (first run or after corruption).
    fn load_phase_index(db: &Database) -> Result<PhaseIndex, StorageError> {
        let read_txn = db.begin_read()?;
        let table = read_txn.open_table(INDEX_TABLE)?;
        match table.get("phase_bitmaps")? {
            Some(value) => PhaseIndex::from_bytes(value.value()),
            None => Ok(PhaseIndex::new()),
        }
    }
}

// ── Core Methods ────────────────────────────────────────────────────

impl MetadataStore {
    /// Generic read-modify-write helper for a single memory record.
    ///
    /// Opens a write transaction, loads the record for `id`, passes it
    /// to the closure `f` for mutation, re-serializes, writes back,
    /// and commits. Returns the mutated record so callers can perform
    /// post-commit side effects (e.g., updating the phase bitmap).
    ///
    /// # Errors
    /// - `StorageError::NotFound` if no record exists for this ID.
    fn update_record<F>(&self, id: &MemoryId, f: F) -> Result<DiskRecord, StorageError>
    where
        F: FnOnce(&mut DiskRecord),
    {
        let key = id.as_bytes().as_slice();

        let write_txn = self.db.begin_write()?;
        let record = {
            let mut table = write_txn.open_table(META_TABLE)?;
            let mut record = {
                let existing = table
                    .get(key)?
                    .ok_or(StorageError::NotFound(id.into_inner()))?;
                DiskRecord::from_bytes(existing.value())?
            };

            f(&mut record);

            table.insert(key, record.to_bytes().as_slice())?;
            record
        };
        write_txn.commit()?;

        Ok(record)
    }

    /// Persist a new memory record. Updates all secondary indexes
    /// (phase bitmap, tag index, namespace index) in the same
    /// write transaction.
    ///
    /// # Errors
    /// - `StorageError::DuplicateId` if a record with this ID already exists.
    pub fn insert(&self, id: MemoryId, record: &DiskRecord) -> Result<(), StorageError> {
        let key = id.as_bytes().as_slice();
        let value = record.to_bytes();

        let write_txn = self.db.begin_write()?;
        {
            // 1. Insert into primary table (reject duplicates).
            let mut meta = write_txn.open_table(META_TABLE)?;
            if meta.get(key)?.is_some() {
                return Err(StorageError::DuplicateId(id.into_inner()));
            }
            meta.insert(key, value.as_slice())?;

            // 2. Update tag index.
            let mut tags = write_txn.open_multimap_table(TAG_INDEX)?;
            for tag in &record.tags {
                tags.insert(tag.as_str(), key)?;
            }

            // 3. Update namespace index.
            let mut ns_idx = write_txn.open_multimap_table(NAMESPACE_INDEX)?;
            ns_idx.insert(record.namespace_id, key)?;
        }
        write_txn.commit()?;

        // 4. Update in-memory phase bitmap (always Phase::Full for new
        //    records).
        {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            pi.insert(record.namespace_id, record.vector_slot);
        }

        Ok(())
    }

    /// Point lookup by memory ID.
    ///
    /// Returns `None` if no record exists for this ID.
    pub fn get(&self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        match table.get(id.as_bytes().as_slice())? {
            Some(value) => {
                let record = DiskRecord::from_bytes(value.value())?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Update the decay fields of a single memory record.
    ///
    /// Read-modify-write: loads the existing record, patches the five
    /// decay fields (phase, strength, decay_strength, stability,
    /// is_permastore), re-serializes, and writes back. If the phase
    /// changed, the phase bitmap index is updated.
    ///
    /// `strength` is the raw FSRS retrievability R(t,S).
    /// `decay_strength` is the effective retrievability including
    /// connection bonus.
    pub fn update_decay_state(
        &self,
        id: MemoryId,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
        stability: f32,
        is_permastore: bool,
    ) -> Result<(), StorageError> {
        let mut old_phase = None;

        let record = self.update_record(&id, |record| {
            old_phase = Some(record.phase);
            record.phase = phase;
            record.strength = strength;
            record.decay_strength = decay_strength;
            record.stability = stability;
            record.is_permastore = if is_permastore { 1 } else { 0 };
        })?;

        let old_phase = old_phase.expect("update_record closure always runs");

        // Update phase bitmap if phase changed.
        if old_phase != phase {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            pi.transition(record.namespace_id, record.vector_slot, old_phase, phase);
        }

        Ok(())
    }

    /// Zero the full_text pointer on a memory record.
    ///
    /// Called during Full -> Summary phase transitions to mark the
    /// text data in fulltext.dat as dead space reclaimable by
    /// compaction. Does not modify fulltext.dat itself.
    pub fn strip_full_text(&self, id: MemoryId) -> Result<(), StorageError> {
        self.update_record(&id, |record| {
            record.text_offset = 0;
            record.text_length = 0;
        })?;
        Ok(())
    }

    /// Clear the summary field on a memory record.
    ///
    /// Called during Summary -> Ghost phase transitions. After this,
    /// only the embedding and relationship edges remain.
    pub fn strip_summary(&self, id: MemoryId) -> Result<(), StorageError> {
        self.update_record(&id, |record| {
            record.summary = String::new();
        })?;
        Ok(())
    }

    /// Record a new access event on a memory.
    ///
    /// Updates `last_accessed_at` and appends to the access history
    /// ring buffer (capped at ACCESS_HISTORY_MAX entries; drops oldest if full).
    pub fn update_access(
        &self,
        id: MemoryId,
        timestamp: i64,
        kind: AccessKind,
    ) -> Result<(), StorageError> {
        self.update_record(&id, |record| {
            record.last_accessed_at = timestamp;

            let event = AccessEvent { timestamp, kind };
            if record.access_history.len() >= ACCESS_HISTORY_MAX {
                record.access_history.remove(0);
            }
            record.access_history.push(event);
        })?;

        Ok(())
    }

    /// Delete a memory record and all its secondary index entries.
    ///
    /// The caller is responsible for cleaning up vectors.dat (free list),
    /// fulltext.dat (compaction), and edges.db separately.
    ///
    /// # Returns
    /// The deleted `DiskRecord` (for the caller to know which vector
    /// slot to free, etc.), or `None` if the ID did not exist.
    pub fn delete(&self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError> {
        let key = id.as_bytes().as_slice();

        let write_txn = self.db.begin_write()?;
        let deleted_record;
        {
            // 1. Remove from primary table.
            let mut meta = write_txn.open_table(META_TABLE)?;
            match meta.remove(key)? {
                Some(value) => {
                    deleted_record = DiskRecord::from_bytes(value.value())?;
                }
                None => {
                    return Ok(None);
                }
            }

            // 2. Remove from tag index.
            let mut tags = write_txn.open_multimap_table(TAG_INDEX)?;
            for tag in &deleted_record.tags {
                tags.remove(tag.as_str(), key)?;
            }

            // 3. Remove from namespace index.
            let mut ns_idx = write_txn.open_multimap_table(NAMESPACE_INDEX)?;
            ns_idx.remove(deleted_record.namespace_id, key)?;
        }
        write_txn.commit()?;

        // 4. Remove from in-memory phase bitmap.
        {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            pi.remove(
                deleted_record.namespace_id,
                deleted_record.vector_slot,
                deleted_record.phase,
            );
        }

        Ok(Some(deleted_record))
    }

    /// Tombstone a memory: strip content fields (summary, tags,
    /// full_text pointer) and set phase to Tombstone.
    ///
    /// Unlike `delete`, this preserves the record in meta.db so that
    /// the graph node and edges remain intact. The caller is
    /// responsible for cleaning up vector, FTS, and entity indexes.
    ///
    /// The tag index entries are removed and the phase bitmap is
    /// updated (removed from old phase, NOT added to tombstone since
    /// tombstoned records don't have valid vector slots for bitmap
    /// tracking).
    pub fn tombstone(&self, id: MemoryId) -> Result<(), StorageError> {
        let key = id.as_bytes().as_slice();

        let write_txn = self.db.begin_write()?;
        let old_phase;
        let old_tags;
        let old_vector_slot;
        let old_namespace_id;
        {
            let mut meta = write_txn.open_table(META_TABLE)?;
            let mut record = {
                let existing = meta
                    .get(key)?
                    .ok_or(StorageError::NotFound(id.into_inner()))?;
                DiskRecord::from_bytes(existing.value())?
            };

            old_phase = record.phase;
            old_tags = record.tags.clone();
            old_vector_slot = record.vector_slot;
            old_namespace_id = record.namespace_id;

            // Strip content fields.
            record.summary = String::new();
            record.tags = Vec::new();
            record.text_offset = 0;
            record.text_length = 0;
            record.phase = DecayPhase::Tombstone;
            record.strength = 0.0;
            record.decay_strength = 0.0;

            meta.insert(key, record.to_bytes().as_slice())?;

            // Remove from tag index.
            let mut tags = write_txn.open_multimap_table(TAG_INDEX)?;
            for tag in &old_tags {
                tags.remove(tag.as_str(), key)?;
            }

            // Remove from namespace index (matches delete() behavior).
            let mut ns_idx = write_txn.open_multimap_table(NAMESPACE_INDEX)?;
            ns_idx.remove(record.namespace_id, key)?;
        }
        write_txn.commit()?;

        // Update phase bitmap: remove from old phase.
        // Tombstoned records are not tracked in the bitmap since their
        // vector slots are freed.
        {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            pi.remove(old_namespace_id, old_vector_slot, old_phase);
        }

        Ok(())
    }

    /// Return all memory IDs currently in the given decay phase.
    ///
    /// Uses the in-memory roaring bitmap index. Requires a META_TABLE
    /// lookup per matching slot to resolve vector_slot -> MemoryId.
    pub fn ids_in_phase(&self, phase: DecayPhase) -> Result<Vec<MemoryId>, StorageError> {
        let ns_slots: Vec<(u32, u32)> = {
            let pi = self.phase_index.read().unwrap_or_else(|e| e.into_inner());
            pi.all_slots_in_phase(phase)
        };

        if ns_slots.is_empty() {
            return Ok(Vec::new());
        }

        // Use (namespace_id, vector_slot) pairs for lookup to avoid
        // cross-namespace collisions.
        let slot_set: std::collections::HashSet<(u32, u32)> = ns_slots.into_iter().collect();
        let mut ids = Vec::new();

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        for result in table.iter()? {
            let (key_bytes, value) = result?;
            let record = DiskRecord::from_bytes(value.value())?;
            if slot_set.contains(&(record.namespace_id, record.vector_slot)) {
                let uuid = uuid::Uuid::from_slice(key_bytes.value())
                    .map_err(|_| StorageError::CorruptIndex("invalid UUID in meta table".into()))?;
                ids.push(MemoryId::from_uuid(uuid));
            }
            if ids.len() == slot_set.len() {
                break;
            }
        }

        Ok(ids)
    }

    /// Return all DiskRecords in a given phase. More efficient than
    /// `ids_in_phase()` when you need the full records.
    pub fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        // Use (namespace_id, vector_slot) pairs for lookup to avoid
        // cross-namespace collisions.
        let ns_slots: std::collections::HashSet<(u32, u32)> = {
            let pi = self.phase_index.read().unwrap_or_else(|e| e.into_inner());
            pi.all_slots_in_phase(phase).into_iter().collect()
        };

        if ns_slots.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::with_capacity(ns_slots.len());
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;

        for result in table.iter()? {
            let (key_bytes, value) = result?;
            let record = DiskRecord::from_bytes(value.value())?;
            if ns_slots.contains(&(record.namespace_id, record.vector_slot)) {
                let uuid = uuid::Uuid::from_slice(key_bytes.value())
                    .map_err(|_| StorageError::CorruptIndex("invalid UUID in meta table".into()))?;
                results.push((MemoryId::from_uuid(uuid), record));
            }
            if results.len() == ns_slots.len() {
                break;
            }
        }

        Ok(results)
    }
}

// ── Edge Count Update ──────────────────────────────────────────────

impl MetadataStore {
    /// Update the edge_count field on a memory's DiskRecord.
    ///
    /// Performs a read-modify-write in a single redb write transaction.
    /// Does not update any secondary indexes (edge_count is not indexed).
    pub fn update_edge_count(&self, id: MemoryId, edge_count: u16) -> Result<(), StorageError> {
        self.update_record(&id, |record| {
            record.edge_count = edge_count;
        })?;

        Ok(())
    }
}

// ── Tag Index (Section 6.2) ─────────────────────────────────────────

impl MetadataStore {
    /// Find all memory IDs that carry a given tag.
    pub fn memories_with_tag(&self, tag: &str) -> Result<Vec<MemoryId>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_multimap_table(TAG_INDEX)?;
        let mut ids = Vec::new();

        let values = table.get(tag)?;
        for result in values {
            let value = result?;
            let bytes: [u8; 16] = value.value().try_into().map_err(|_| {
                StorageError::CorruptIndex("invalid UUID length in tag index".into())
            })?;
            ids.push(MemoryId::from_bytes(bytes));
        }

        Ok(ids)
    }

    /// List all distinct tags in the index with their memory counts.
    pub fn list_tags(&self) -> Result<Vec<(String, u64)>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_multimap_table(TAG_INDEX)?;
        let mut tags = Vec::new();

        for result in table.iter()? {
            let (key, values) = result?;
            let tag_name = key.value().to_string();
            let count = values.count() as u64;
            tags.push((tag_name, count));
        }

        Ok(tags)
    }

    /// Tag histogram over the live memories of one namespace.
    ///
    /// `TAG_INDEX` is keyed by tag alone with no namespace dimension, so
    /// this walks `NAMESPACE_INDEX` and does one `META_TABLE` point
    /// lookup per member — the same work as one unfiltered
    /// [`list_memories_filtered`](Self::list_memories_filtered) call, not
    /// a `scan_all`.
    ///
    /// Returns `(tag -> memory count, live memory count)`. Tombstoned
    /// records are skipped; `Summary` and `Ghost` are included, because a
    /// phase transition strips text, not tags.
    pub fn tag_counts_in_namespace(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<(HashMap<String, u64>, u64), StorageError> {
        let read_txn = self.db.begin_read()?;
        let ns_table = read_txn.open_multimap_table(NAMESPACE_INDEX)?;
        let meta_table = read_txn.open_table(META_TABLE)?;

        let mut counts: HashMap<String, u64> = HashMap::new();
        let mut memories = 0u64;

        for result in ns_table.get(namespace_id.get())? {
            let value = result?;
            let bytes: [u8; 16] = value.value().try_into().map_err(|_| {
                StorageError::CorruptIndex("invalid UUID length in namespace index".into())
            })?;
            let Some(record_value) = meta_table.get(bytes.as_slice())? else {
                continue;
            };
            let record = DiskRecord::from_bytes(record_value.value())?;
            if record.phase == DecayPhase::Tombstone {
                continue;
            }
            memories += 1;
            for tag in &record.tags {
                *counts.entry(tag.as_str().to_string()).or_insert(0) += 1;
            }
        }

        Ok((counts, memories))
    }
}

// ── Namespace Index (Section 6.3) ───────────────────────────────────

impl MetadataStore {
    /// Find all memory IDs in a given namespace.
    pub fn memories_in_namespace(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<Vec<MemoryId>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_multimap_table(NAMESPACE_INDEX)?;
        let mut ids = Vec::new();

        let values = table.get(namespace_id.get())?;
        for result in values {
            let value = result?;
            let bytes: [u8; 16] = value.value().try_into().map_err(|_| {
                StorageError::CorruptIndex("invalid UUID length in namespace index".into())
            })?;
            ids.push(MemoryId::from_bytes(bytes));
        }

        Ok(ids)
    }

    /// Number of live (non-tombstoned) memories in `namespace_id`.
    ///
    /// Applies the same predicate as [`list_memories_filtered`] with no
    /// filters — walk `NAMESPACE_INDEX`, load each record, skip
    /// `Tombstone` — so `list_namespaces[].memoryCount`,
    /// `namespace_stats.memoryCount` and `list_memories.total` agree *by
    /// construction* rather than by invariant.
    ///
    /// One point lookup per member of the namespace, not a scan of the
    /// database.
    ///
    /// [`memories_in_namespace`](Self::memories_in_namespace)`.len()`
    /// would be cheaper — index cardinality only, no `META_TABLE` reads —
    /// and `tombstone` does remove from the index, so the two agree in
    /// practice. But they diverge for an index entry whose `META_TABLE`
    /// row is missing, which `list_memories_filtered` explicitly
    /// tolerates, and the whole point here is that this count agrees with
    /// what a caller can actually list.
    pub fn count_memories_in_namespace(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<u64, StorageError> {
        let read_txn = self.db.begin_read()?;
        let ns_table = read_txn.open_multimap_table(NAMESPACE_INDEX)?;
        let meta_table = read_txn.open_table(META_TABLE)?;

        let mut count = 0u64;
        for result in ns_table.get(namespace_id.get())? {
            let value = result?;
            let bytes: [u8; 16] = value.value().try_into().map_err(|_| {
                StorageError::CorruptIndex("invalid UUID length in namespace index".into())
            })?;
            let Some(record_value) = meta_table.get(bytes.as_slice())? else {
                continue;
            };
            if DiskRecord::from_bytes(record_value.value())?.phase != DecayPhase::Tombstone {
                count += 1;
            }
        }

        Ok(count)
    }

    /// List memories in a namespace with optional tag/entity/time filters,
    /// sorted by creation date (newest first) with offset/limit pagination.
    ///
    /// This is a metadata-only query path that does not require embeddings.
    /// Filters use AND semantics: a memory must match ALL provided tags
    /// and ALL provided entities to be included.
    ///
    /// Returns `(matching_records, total_matching_count)` where the records
    /// are the paginated slice and the count is the total before pagination.
    pub fn list_memories_filtered(
        &self,
        namespace_id: NamespaceId,
        require_tags: &[crate::model::Tag],
        time_range_start: Option<i64>,
        time_range_end: Option<i64>,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<(MemoryId, DiskRecord)>, u64), StorageError> {
        // Step 1: Get all memory IDs in this namespace from the index.
        let read_txn = self.db.begin_read()?;
        let ns_table = read_txn.open_multimap_table(NAMESPACE_INDEX)?;
        let meta_table = read_txn.open_table(META_TABLE)?;

        // Collect candidate IDs from namespace index.
        let ns_values = ns_table.get(namespace_id.get())?;
        let mut candidates: Vec<(MemoryId, DiskRecord)> = Vec::new();

        for result in ns_values {
            let value = result?;
            let bytes: [u8; 16] = value.value().try_into().map_err(|_| {
                StorageError::CorruptIndex("invalid UUID length in namespace index".into())
            })?;
            let mid = MemoryId::from_bytes(bytes);

            // Look up the full record.
            let Some(record_value) = meta_table.get(mid.as_bytes().as_slice())? else {
                continue;
            };
            let record = DiskRecord::from_bytes(record_value.value())?;

            // Skip tombstoned records.
            if record.phase == DecayPhase::Tombstone {
                continue;
            }

            // Apply time range filter.
            if let Some(start) = time_range_start {
                if record.created_at < start {
                    continue;
                }
            }
            if let Some(end) = time_range_end {
                if record.created_at > end {
                    continue;
                }
            }

            // Apply tag filter (AND semantics): record must have ALL required tags.
            if !require_tags.is_empty() {
                let has_all = require_tags.iter().all(|rt| record.tags.contains(rt));
                if !has_all {
                    continue;
                }
            }

            candidates.push((mid, record));
        }

        // Step 2: Sort by creation date, newest first.
        // UUID v7 keys are already chronological, but we sort by created_at
        // for correctness (handles any edge cases).
        candidates.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));

        let total = candidates.len() as u64;

        // Step 3: Apply offset/limit pagination.
        let page: Vec<(MemoryId, DiskRecord)> =
            candidates.into_iter().skip(offset).take(limit).collect();

        Ok((page, total))
    }
}

// ── Batch Operations for Decay Sweep (Section 7) ────────────────────

impl MetadataStore {
    /// Execute a decay sweep using a caller-provided transition function.
    ///
    /// 1. Takes a read snapshot of all records.
    /// 2. Calls `compute_new_state` on each record.
    /// 3. Collects records where the function returns Some.
    /// 4. Applies all updates in a single write transaction.
    /// 5. Updates the phase bitmap index.
    /// 6. Persists the updated phase bitmaps to INDEX_TABLE.
    ///
    /// # Returns
    /// The number of records that were transitioned.
    pub fn decay_sweep(
        &self,
        compute_new_state: impl Fn(&DiskRecord) -> Option<DiskRecord>,
    ) -> Result<usize, StorageError> {
        // Phase 1: Read snapshot.
        let updates: Vec<(MemoryId, DiskRecord, DecayPhase)> = {
            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(META_TABLE)?;
            let mut updates = Vec::new();

            for result in table.iter()? {
                let (key_bytes, value) = result?;
                let record = DiskRecord::from_bytes(value.value())?;
                let old_phase = record.phase;

                if let Some(new_record) = compute_new_state(&record) {
                    let uuid = uuid::Uuid::from_slice(key_bytes.value()).map_err(|_| {
                        StorageError::CorruptIndex("invalid UUID in meta table".into())
                    })?;
                    updates.push((MemoryId::from_uuid(uuid), new_record, old_phase));
                }
            }
            updates
        };

        if updates.is_empty() {
            return Ok(0);
        }

        let count = updates.len();

        // Phase 2: Batch write all updated records + persist bitmaps.
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(META_TABLE)?;
            for (id, record, _) in &updates {
                table.insert(id.as_bytes().as_slice(), record.to_bytes().as_slice())?;
            }
        }

        // Phase 3: Update in-memory phase bitmap.
        {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            for (_, record, old_phase) in &updates {
                if *old_phase != record.phase {
                    pi.transition(
                        record.namespace_id,
                        record.vector_slot,
                        *old_phase,
                        record.phase,
                    );
                }
            }

            // Persist bitmaps inside the same write transaction.
            let mut idx_table = write_txn.open_table(INDEX_TABLE)?;
            idx_table.insert("phase_bitmaps", pi.to_bytes().as_slice())?;
        }
        write_txn.commit()?;

        Ok(count)
    }

    /// Batch update multiple records in a single transaction.
    /// Does NOT update secondary indexes -- caller must manage
    /// index consistency.
    pub fn batch_update_records(
        &self,
        records: &[(MemoryId, DiskRecord)],
    ) -> Result<(), StorageError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(META_TABLE)?;
            for (id, record) in records {
                table.insert(id.as_bytes().as_slice(), record.to_bytes().as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Persist the current in-memory phase bitmaps to INDEX_TABLE.
    /// Called periodically (end of decay sweep, graceful shutdown).
    pub fn persist_phase_index(&self) -> Result<(), StorageError> {
        let bytes = {
            let pi = self.phase_index.read().unwrap_or_else(|e| e.into_inner());
            pi.to_bytes()
        };
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(INDEX_TABLE)?;
            table.insert("phase_bitmaps", bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Rebuild the phase index by scanning all records in META_TABLE.
    /// Called during startup validation when the persisted bitmap is
    /// missing, corrupt, or when the record count does not match the
    /// bitmap cardinality.
    pub fn rebuild_phase_index(&self) -> Result<(), StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        let mut slot_phases = Vec::new();

        for result in table.iter()? {
            let (_, value) = result?;
            let record = DiskRecord::from_bytes(value.value())?;
            slot_phases.push((record.namespace_id, record.vector_slot, record.phase));
        }
        drop(table);
        drop(read_txn);

        let rebuilt = PhaseIndex::rebuild_from_records(&slot_phases);

        // Persist the rebuilt index.
        let bytes = rebuilt.to_bytes();
        let write_txn = self.db.begin_write()?;
        {
            let mut idx_table = write_txn.open_table(INDEX_TABLE)?;
            idx_table.insert("phase_bitmaps", bytes.as_slice())?;
        }
        write_txn.commit()?;

        // Replace the in-memory copy.
        *self.phase_index.write().unwrap_or_else(|e| e.into_inner()) = rebuilt;

        Ok(())
    }
}

// ── Namespace CRUD (Section 8) ──────────────────────────────────────

impl MetadataStore {
    /// Create a new namespace. Assigns the next sequential NamespaceId.
    ///
    /// # Errors
    /// - `StorageError::DuplicateName` if a namespace with this name
    ///   already exists.
    pub fn create_namespace(&self, config: &NamespaceConfig) -> Result<NamespaceId, StorageError> {
        let write_txn = self.db.begin_write()?;
        let new_id;
        {
            // Check name uniqueness by scanning existing namespaces.
            //
            // Case-INSENSITIVELY, because the name becomes a directory
            // name and macOS APFS, Windows NTFS and a case-insensitive
            // ext4 volume all give "notes" and "Notes" the SAME
            // directory. Two rows sharing one directory is not a
            // cosmetic collision:
            //
            // - delete "notes" (row gone, remove_dir_all fails, warn
            //   only), create "Notes", and the new namespace adopts the
            //   old one's `vectors.dat` — somebody else's embeddings.
            // - `create_namespace("Default")` used to pass the
            //   uniqueness check, commit its row, no-op `create_dir_all`
            //   onto `default/` and only then fail on the exclusive
            //   flock. The row survived, and `delete_namespace("Default")`
            //   walked past the `!= "default"` guard and unlinked the
            //   real default's vectors.
            //
            // `validate_namespace_name` restricts names to ASCII, so
            // ASCII case folding is total here.
            let ns_table = write_txn.open_table(NAMESPACE_TABLE)?;
            for result in ns_table.iter()? {
                let (_, value) = result?;
                let existing: NamespaceConfig = serde_json::from_slice(value.value())
                    .map_err(|e| StorageError::Deserialize(e.to_string()))?;
                if existing.name.eq_ignore_ascii_case(&config.name) {
                    return Err(StorageError::DuplicateName(config.name.clone()));
                }
            }
            drop(ns_table);

            // Assign next ID.
            let mut counter_table = write_txn.open_table(NAMESPACE_COUNTER)?;
            let current_max = counter_table.get("max_id")?.map(|v| v.value()).unwrap_or(0);
            new_id = current_max + 1;
            counter_table.insert("max_id", new_id)?;
            drop(counter_table);

            // Write the namespace config with the assigned ID.
            let mut stored_config = config.clone();
            stored_config.id = NamespaceId::new(new_id);
            let bytes = serde_json::to_vec(&stored_config)
                .map_err(|e| StorageError::Serialize(e.to_string()))?;

            let mut ns_table = write_txn.open_table(NAMESPACE_TABLE)?;
            ns_table.insert(new_id, bytes.as_slice())?;
        }
        write_txn.commit()?;

        Ok(NamespaceId::new(new_id))
    }

    /// List all namespace configurations.
    pub fn list_namespaces(&self) -> Result<Vec<NamespaceConfig>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(NAMESPACE_TABLE)?;
        let mut configs = Vec::new();

        for result in table.iter()? {
            let (_, value) = result?;
            let config: NamespaceConfig = serde_json::from_slice(value.value())
                .map_err(|e| StorageError::Deserialize(e.to_string()))?;
            configs.push(config);
        }

        Ok(configs)
    }

    /// Get a single namespace configuration by ID.
    pub fn get_namespace(&self, id: NamespaceId) -> Result<Option<NamespaceConfig>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(NAMESPACE_TABLE)?;
        match table.get(id.get())? {
            Some(value) => {
                let config: NamespaceConfig = serde_json::from_slice(value.value())
                    .map_err(|e| StorageError::Deserialize(e.to_string()))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Get a namespace by name (linear scan -- acceptable for <100
    /// namespaces).
    ///
    /// Matching is case-insensitive, to agree with the uniqueness rule
    /// in [`create_namespace`](Self::create_namespace) — a name that
    /// cannot be created because it collides case-insensitively must
    /// also resolve case-insensitively, or `create` says "duplicate"
    /// while `stats` says "not found" for the same string.
    ///
    /// An exact match wins over a case-folded one. That only matters for
    /// a database written before the uniqueness rule folded case, which
    /// can already hold both `notes` and `Notes`; preferring the exact
    /// match keeps each of those rows individually addressable rather
    /// than resolving both to whichever the scan reaches first.
    pub fn get_namespace_by_name(
        &self,
        name: &str,
    ) -> Result<Option<NamespaceConfig>, StorageError> {
        let all = self.list_namespaces()?;
        if let Some(exact) = all.iter().find(|c| c.name == name) {
            return Ok(Some(exact.clone()));
        }
        Ok(all.into_iter().find(|c| c.name.eq_ignore_ascii_case(name)))
    }

    // `update_namespace` used to live here: a read-modify-write of the
    // NAMESPACE_TABLE row that preserved id, embedding_dim and
    // created_at and let everything else through — including `name`.
    // It had zero callers, and a rename is not a metadata edit. The name
    // IS the directory: renaming the row leaves the memories reading
    // `<old>/vectors.dat` until the next boot, where step 6 creates a
    // fresh empty `<new>/` and
    // `RedbStorageEngine::sweep_orphan_namespace_dirs` finds `<old>/`
    // matching no live name and removes it. A namespace rename would
    // have been a delayed, unattended shredder for every embedding in
    // it. Deleted rather than documented; if a rename is ever wanted it
    // has to move the directory in the same operation.

    /// Delete a namespace record from NAMESPACE_TABLE.
    ///
    /// Only removes the config entry. Memory cleanup is the caller's
    /// responsibility. The namespace ID is never recycled.
    pub fn delete_namespace(&self, id: NamespaceId) -> Result<NamespaceConfig, StorageError> {
        let write_txn = self.db.begin_write()?;
        let config;
        {
            let mut table = write_txn.open_table(NAMESPACE_TABLE)?;
            let value = table
                .remove(id.get())?
                .ok_or(StorageError::NamespaceNotFound(id.get()))?;
            config = serde_json::from_slice(value.value())
                .map_err(|e| StorageError::Deserialize(e.to_string()))?;
        }
        write_txn.commit()?;
        Ok(config)
    }

    /// Remove every memory record belonging to a namespace, live and
    /// tombstoned alike, together with its tag and namespace index
    /// entries.
    ///
    /// # DESTRUCTIVE AND IRREVERSIBLE
    ///
    /// There is no undo and no backup. Returns the removed
    /// `(MemoryId, DiskRecord)` pairs so the caller can free vector
    /// slots and clean the FTS, vector and entity indexes, none of
    /// which this store can reach.
    ///
    /// # Why a `META_TABLE` scan and not `NAMESPACE_INDEX`
    ///
    /// [`tombstone`](Self::tombstone) unlinks a record from
    /// `NAMESPACE_INDEX` while leaving its `META_TABLE` row in place, so
    /// an index-driven walk structurally cannot see tombstones — and
    /// tombstones are exactly the rows nothing else ever reclaims. The
    /// full scan is the only way to reach them. `record.namespace_id` is
    /// the filter.
    ///
    /// # Chunking, and what the caller owes it
    ///
    /// Removal is committed in chunks of `batch_size` rather than one
    /// transaction, so a large namespace does not hold the write lock
    /// for the whole purge. That makes the operation *resumable but not
    /// atomic*: an interrupted purge leaves a live namespace holding
    /// fewer memories. The caller must therefore delete the
    /// `NAMESPACE_TABLE` row **after** this returns, never before —
    /// deleting it first turns a crash mid-purge into orphan records
    /// belonging to a namespace that no longer exists.
    pub fn purge_namespace_records(
        &self,
        namespace_id: NamespaceId,
        batch_size: usize,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, PartialPurge> {
        // chunks(0) panics, and a zero batch would never terminate
        // anyway.
        let batch_size = batch_size.max(1);
        let ns = namespace_id.get();

        // Pass 1: read scan. Collects tombstones as well as live rows.
        // Nothing is committed yet, so a failure here destroyed nothing.
        let victims = match self.scan_namespace_records(ns) {
            Ok(v) => v,
            Err(source) => {
                return Err(PartialPurge {
                    destroyed: Vec::new(),
                    source,
                });
            }
        };

        if victims.is_empty() {
            return Ok(Vec::new());
        }

        // Pass 2: chunked removal. Each chunk that commits is gone
        // forever, so the count of committed chunks travels with the
        // error rather than being thrown away with it.
        let mut destroyed: Vec<(MemoryId, DiskRecord)> = Vec::with_capacity(victims.len());
        for chunk in victims.chunks(batch_size) {
            if let Err(source) = self.purge_one_chunk(ns, chunk) {
                // The bitmaps for what DID go are dropped slot by slot:
                // `remove_namespace` would drop the survivors' entries
                // too, and the survivors are still live records.
                {
                    let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
                    for (_, record) in &destroyed {
                        pi.remove(ns, record.vector_slot, record.phase);
                    }
                }
                return Err(PartialPurge { destroyed, source });
            }
            destroyed.extend_from_slice(chunk);
        }

        // The whole namespace is gone, so drop its bitmaps wholesale
        // rather than slot by slot.
        {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            pi.remove_namespace(ns);
        }

        Ok(destroyed)
    }

    /// Every `META_TABLE` row belonging to `ns`, tombstones included.
    fn scan_namespace_records(&self, ns: u32) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        let mut found = Vec::new();
        for result in table.iter()? {
            let (key_bytes, value) = result?;
            let record = DiskRecord::from_bytes(value.value())?;
            if record.namespace_id != ns {
                continue;
            }
            let uuid = uuid::Uuid::from_slice(key_bytes.value())
                .map_err(|_| StorageError::CorruptIndex("invalid UUID in meta table".into()))?;
            found.push((MemoryId::from_uuid(uuid), record));
        }
        Ok(found)
    }

    /// Remove one chunk of records, with their tag and namespace index
    /// entries, in a single transaction.
    fn purge_one_chunk(
        &self,
        ns: u32,
        chunk: &[(MemoryId, DiskRecord)],
    ) -> Result<(), StorageError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut meta = write_txn.open_table(META_TABLE)?;
            let mut tags = write_txn.open_multimap_table(TAG_INDEX)?;
            let mut ns_idx = write_txn.open_multimap_table(NAMESPACE_INDEX)?;

            for (id, record) in chunk {
                let key = id.as_bytes().as_slice();
                meta.remove(key)?;
                for tag in &record.tags {
                    tags.remove(tag.as_str(), key)?;
                }
                // A tombstoned record was already unlinked from
                // NAMESPACE_INDEX; removing again is a no-op.
                ns_idx.remove(ns, key)?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }
}

/// A [`MetadataStore::purge_namespace_records`] that stopped part-way.
///
/// Removal is chunked, so a failure in chunk N leaves chunks `0..N`
/// committed and permanently gone. Carrying that list out with the error
/// is the difference between a caller that can finish the job — free the
/// vector slots and clean the FTS, vector, entity and graph entries those
/// records still occupy — and one that reports a bare failure and strands
/// them in every index for the life of the database, where a retry cannot
/// find them because their `META_TABLE` rows are already gone.
#[derive(Debug)]
pub struct PartialPurge {
    /// Records already removed. Permanently destroyed; the caller still
    /// owes them index cleanup.
    pub destroyed: Vec<(MemoryId, DiskRecord)>,
    /// What stopped the purge.
    pub source: StorageError,
}

impl std::fmt::Display for PartialPurge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} records were destroyed before the purge failed: {}",
            self.destroyed.len(),
            self.source
        )
    }
}

impl std::error::Error for PartialPurge {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

// ── Batch Tombstoning ───────────────────────────────────────────────

/// What [`MetadataStore::batch_tombstone`] did to one requested id.
#[derive(Debug, Clone)]
pub enum TombstoneOutcome {
    /// The record was live and has been tombstoned. Carries the record
    /// **as it was before the strip**, so the caller still has the
    /// vector slot to free and the tags to clean out of the entity
    /// index — both of which the stripped record no longer holds.
    Tombstoned(DiskRecord),
    /// No record exists for this id.
    NotFound,
    /// The record exists but was already in the Tombstone phase.
    AlreadyTombstoned,
}

impl MetadataStore {
    /// Tombstone many memories in a single write transaction.
    ///
    /// Returns one outcome per input id, in input order. A per-id miss
    /// (`NotFound`, `AlreadyTombstoned`) is an outcome, not an error,
    /// and does not abort the batch; only a genuine storage fault
    /// aborts, in which case the transaction is dropped and *nothing*
    /// is tombstoned.
    ///
    /// # Duplicate ids are safe by construction
    ///
    /// The second occurrence of an id reads back the first occurrence's
    /// uncommitted write from within the same transaction, sees
    /// `DecayPhase::Tombstone`, and reports `AlreadyTombstoned`. Since
    /// only `Tombstoned` carries a record, a caller that frees a vector
    /// slot per `Tombstoned` outcome cannot free the same slot twice —
    /// which would build a cycle in the free list and underflow
    /// `live_count`.
    pub fn batch_tombstone(&self, ids: &[MemoryId]) -> Result<Vec<TombstoneOutcome>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut outcomes = Vec::with_capacity(ids.len());

        let write_txn = self.db.begin_write()?;
        {
            let mut meta = write_txn.open_table(META_TABLE)?;
            let mut tags = write_txn.open_multimap_table(TAG_INDEX)?;
            let mut ns_idx = write_txn.open_multimap_table(NAMESPACE_INDEX)?;

            for id in ids {
                let key = id.as_bytes().as_slice();

                let existing = match meta.get(key)? {
                    Some(value) => DiskRecord::from_bytes(value.value())?,
                    None => {
                        outcomes.push(TombstoneOutcome::NotFound);
                        continue;
                    }
                };

                if existing.phase == DecayPhase::Tombstone {
                    outcomes.push(TombstoneOutcome::AlreadyTombstoned);
                    continue;
                }

                let mut stripped = existing.clone();
                stripped.summary = String::new();
                stripped.tags = Vec::new();
                stripped.text_offset = 0;
                stripped.text_length = 0;
                stripped.phase = DecayPhase::Tombstone;
                stripped.strength = 0.0;
                stripped.decay_strength = 0.0;

                meta.insert(key, stripped.to_bytes().as_slice())?;

                for tag in &existing.tags {
                    tags.remove(tag.as_str(), key)?;
                }
                ns_idx.remove(existing.namespace_id, key)?;

                outcomes.push(TombstoneOutcome::Tombstoned(existing));
            }
        }
        write_txn.commit()?;

        // Phase bitmaps only after the commit: a bitmap update rolled
        // forward against a transaction that never landed would point
        // the decay sweep at slots that are still live.
        {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            for outcome in &outcomes {
                if let TombstoneOutcome::Tombstoned(record) = outcome {
                    pi.remove(record.namespace_id, record.vector_slot, record.phase);
                }
            }
        }

        Ok(outcomes)
    }
}

// ── Supplementary Methods (Section 9) ───────────────────────────────

impl MetadataStore {
    /// Iterate all records in creation order (UUID v7 byte order).
    pub fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        let mut records = Vec::new();

        for result in table.iter()? {
            let (key_bytes, value) = result?;
            let uuid = uuid::Uuid::from_slice(key_bytes.value())
                .map_err(|_| StorageError::CorruptIndex("invalid UUID in meta table".into()))?;
            let record = DiskRecord::from_bytes(value.value())?;
            records.push((MemoryId::from_uuid(uuid), record));
        }

        Ok(records)
    }

    /// Count the total number of memory records (including tombstones).
    ///
    /// For a count that excludes tombstoned records, use `count_active()`.
    pub fn count(&self) -> Result<u64, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        Ok(table.len()?)
    }

    /// Count only active (non-tombstoned) memory records.
    pub fn count_active(&self) -> Result<u64, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        let mut count = 0u64;
        for result in table.iter()? {
            let (_, value) = result?;
            let record = DiskRecord::from_bytes(value.value())?;
            if record.phase != DecayPhase::Tombstone {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Check if a memory ID exists without deserializing the record.
    pub fn exists(&self, id: MemoryId) -> Result<bool, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        Ok(table.get(id.as_bytes().as_slice())?.is_some())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tag;

    fn open_store() -> (tempfile::TempDir, MetadataStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = MetadataStore::open(&dir.path().join("meta.db")).expect("open meta.db");
        (dir, store)
    }

    /// A live, Full-phase record in `namespace_id` occupying `slot`.
    fn record_in(namespace_id: u32, slot: u32, tags: &[&str]) -> DiskRecord {
        DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: [0u8; 16],
            namespace_id,
            created_at: 0,
            last_accessed_at: 0,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability: 1.0,
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: slot,
            edge_count: 0,
            summary: "fixture".into(),
            tags: tags
                .iter()
                .map(|t| Tag::new(*t).expect("valid tag"))
                .collect(),
            access_history: Vec::new(),
            text_offset: 0,
            text_length: 0,
        }
    }

    fn insert(store: &MetadataStore, namespace_id: u32, slot: u32, tags: &[&str]) -> MemoryId {
        let id = MemoryId::new();
        let mut record = record_in(namespace_id, slot, tags);
        record.id = *id.as_bytes();
        store.insert(id, &record).expect("insert");
        id
    }

    // ── purge_namespace_records ──────────────────────────────────────

    /// The whole reason this scans META_TABLE instead of walking
    /// NAMESPACE_INDEX: `tombstone` unlinks the record from that index,
    /// so an index-driven purge cannot see a tombstone — and tombstones
    /// are the rows nothing else in the system ever reclaims.
    #[test]
    fn w01_purge_removes_live_and_tombstoned_rows_of_the_target_namespace_only() {
        let (_dir, store) = open_store();

        let live = insert(&store, 1, 0, &[]);
        let dead = insert(&store, 1, 1, &[]);
        store.tombstone(dead).expect("tombstone");
        let survivor = insert(&store, 2, 0, &[]);

        let purged = store
            .purge_namespace_records(NamespaceId::new(1), 100)
            .expect("purge");

        assert_eq!(purged.len(), 2, "both the live row and the tombstone");
        let purged_ids: Vec<MemoryId> = purged.iter().map(|(id, _)| *id).collect();
        assert!(purged_ids.contains(&live));
        assert!(purged_ids.contains(&dead), "the tombstone was not reached");

        assert!(store.get(live).expect("get").is_none());
        assert!(store.get(dead).expect("get").is_none());
        assert!(
            store.get(survivor).expect("get").is_some(),
            "a record in another namespace was destroyed"
        );
    }

    #[test]
    fn w02_purge_clears_the_tag_index() {
        let (_dir, store) = open_store();

        insert(&store, 1, 0, &["topic/doomed"]);
        insert(&store, 2, 0, &["topic/kept"]);

        store
            .purge_namespace_records(NamespaceId::new(1), 100)
            .expect("purge");

        assert!(
            store
                .memories_with_tag("topic/doomed")
                .expect("tag")
                .is_empty(),
            "tag index still points at purged records"
        );
        assert_eq!(store.memories_with_tag("topic/kept").expect("tag").len(), 1);
    }

    #[test]
    fn w03_purge_clears_the_namespace_index() {
        let (_dir, store) = open_store();

        insert(&store, 1, 0, &[]);
        insert(&store, 1, 1, &[]);
        insert(&store, 2, 0, &[]);

        store
            .purge_namespace_records(NamespaceId::new(1), 100)
            .expect("purge");

        assert!(
            store
                .memories_in_namespace(NamespaceId::new(1))
                .expect("ns index")
                .is_empty()
        );
        assert_eq!(
            store
                .memories_in_namespace(NamespaceId::new(2))
                .expect("ns index")
                .len(),
            1
        );
    }

    #[test]
    fn w04_purge_of_an_unknown_or_empty_namespace_is_an_empty_ok() {
        let (_dir, store) = open_store();
        insert(&store, 1, 0, &[]);

        assert!(
            store
                .purge_namespace_records(NamespaceId::new(9), 100)
                .expect("purge")
                .is_empty()
        );
        assert_eq!(store.count().expect("count"), 1, "nothing else was touched");
    }

    /// Chunking is what keeps a large purge off the write lock. It must
    /// not also be what makes a large purge incomplete.
    #[test]
    fn w05_purge_chunking_removes_every_record() {
        let (_dir, store) = open_store();
        for slot in 0..5 {
            insert(&store, 1, slot, &[]);
        }

        let purged = store
            .purge_namespace_records(NamespaceId::new(1), 2)
            .expect("purge");

        assert_eq!(purged.len(), 5);
        assert_eq!(store.count().expect("count"), 0);
    }

    // ── batch_tombstone ──────────────────────────────────────────────

    #[test]
    fn w06_batch_tombstone_reports_one_outcome_per_id_in_input_order() {
        let (_dir, store) = open_store();

        let live = insert(&store, 1, 0, &[]);
        let already = insert(&store, 1, 1, &[]);
        store.tombstone(already).expect("tombstone");
        let missing = MemoryId::new();

        let outcomes = store
            .batch_tombstone(&[missing, live, already])
            .expect("batch tombstone");

        assert!(matches!(outcomes[0], TombstoneOutcome::NotFound));
        assert!(matches!(outcomes[1], TombstoneOutcome::Tombstoned(_)));
        assert!(matches!(outcomes[2], TombstoneOutcome::AlreadyTombstoned));
    }

    /// A double free of one vector slot builds a cycle in the on-disk
    /// free list and underflows `live_count`, handing the same slot to
    /// two memories. The batch is the only place a caller can hit it, so
    /// the batch is where it has to be impossible: the second occurrence
    /// reads the first's uncommitted write and reports
    /// `AlreadyTombstoned`, which carries no record and so frees no slot.
    #[test]
    fn w07_a_duplicate_id_in_one_batch_yields_exactly_one_tombstoned_outcome() {
        let (_dir, store) = open_store();
        let id = insert(&store, 1, 7, &[]);

        let outcomes = store.batch_tombstone(&[id, id]).expect("batch tombstone");

        assert!(matches!(outcomes[0], TombstoneOutcome::Tombstoned(_)));
        assert!(matches!(outcomes[1], TombstoneOutcome::AlreadyTombstoned));
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, TombstoneOutcome::Tombstoned(_)))
                .count(),
            1,
            "a caller freeing one slot per Tombstoned outcome would double-free"
        );
    }

    /// The batch path is a second implementation of `tombstone`. It has
    /// to leave the database in the state the single-record path would.
    #[test]
    fn w08_batch_tombstone_leaves_the_same_state_as_the_single_record_path() {
        let (_dir, store) = open_store();

        let single = insert(&store, 1, 0, &["topic/alpha"]);
        let batched = insert(&store, 1, 1, &["topic/beta"]);

        store.tombstone(single).expect("tombstone");
        let outcomes = store.batch_tombstone(&[batched]).expect("batch tombstone");

        let TombstoneOutcome::Tombstoned(pre_strip) = &outcomes[0] else {
            panic!("expected Tombstoned, got {:?}", outcomes[0]);
        };
        assert_eq!(
            pre_strip.vector_slot, 1,
            "the outcome must carry the PRE-strip record, or the caller has no slot to free"
        );
        assert_eq!(pre_strip.tags.len(), 1, "pre-strip tags must survive");

        for id in [single, batched] {
            let record = store.get(id).expect("get").expect("record survives");
            assert_eq!(record.phase, DecayPhase::Tombstone);
            assert!(record.summary.is_empty());
            assert!(record.tags.is_empty());
        }

        assert!(
            store
                .memories_with_tag("topic/alpha")
                .expect("tag")
                .is_empty()
        );
        assert!(
            store
                .memories_with_tag("topic/beta")
                .expect("tag")
                .is_empty()
        );
        assert!(
            store
                .memories_in_namespace(NamespaceId::new(1))
                .expect("ns index")
                .is_empty()
        );
        assert!(
            store
                .ids_in_phase(DecayPhase::Full)
                .expect("phase")
                .is_empty(),
            "phase bitmaps still claim both slots are live"
        );
    }

    #[test]
    fn w09_batch_tombstone_of_an_empty_slice_is_an_empty_ok() {
        let (_dir, store) = open_store();
        let id = insert(&store, 1, 0, &[]);

        assert!(
            store
                .batch_tombstone(&[])
                .expect("batch tombstone")
                .is_empty()
        );
        assert_eq!(
            store.get(id).expect("get").expect("record").phase,
            DecayPhase::Full,
            "an empty batch must not open a transaction that changes anything"
        );
    }
}
