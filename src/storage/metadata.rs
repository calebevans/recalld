//! Metadata persistence backed by redb (meta.db).
//!
//! Owns the redb `Database` handle for meta.db, provides CRUD
//! operations for memory records and namespace configurations,
//! and manages secondary indexes (phase bitmaps, tag index,
//! namespace index).
//!
//! See CS-07 for the full specification.

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
            let mut pi = self
                .phase_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
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
            let mut pi = self
                .phase_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
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
            let mut pi = self
                .phase_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
            pi.remove(deleted_record.namespace_id, deleted_record.vector_slot, deleted_record.phase);
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
            let mut pi = self
                .phase_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
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
            let pi = self
                .phase_index
                .read()
                .unwrap_or_else(|e| e.into_inner());
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
            let pi = self
                .phase_index
                .read()
                .unwrap_or_else(|e| e.into_inner());
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
            let mut pi = self
                .phase_index
                .write()
                .unwrap_or_else(|e| e.into_inner());
            for (_, record, old_phase) in &updates {
                if *old_phase != record.phase {
                    pi.transition(record.namespace_id, record.vector_slot, *old_phase, record.phase);
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
            let pi = self
                .phase_index
                .read()
                .unwrap_or_else(|e| e.into_inner());
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
        *self
            .phase_index
            .write()
            .unwrap_or_else(|e| e.into_inner()) = rebuilt;

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
            let ns_table = write_txn.open_table(NAMESPACE_TABLE)?;
            for result in ns_table.iter()? {
                let (_, value) = result?;
                let existing: NamespaceConfig = serde_json::from_slice(value.value())
                    .map_err(|e| StorageError::Deserialize(e.to_string()))?;
                if existing.name == config.name {
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
    pub fn get_namespace_by_name(
        &self,
        name: &str,
    ) -> Result<Option<NamespaceConfig>, StorageError> {
        let all = self.list_namespaces()?;
        Ok(all.into_iter().find(|c| c.name == name))
    }

    /// Update a namespace's mutable fields (name, thresholds, etc.).
    /// Immutable fields (id, embedding_dim, created_at) are preserved.
    pub fn update_namespace(
        &self,
        id: NamespaceId,
        updated: &NamespaceConfig,
    ) -> Result<(), StorageError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(NAMESPACE_TABLE)?;
            let existing: NamespaceConfig = {
                let existing_bytes = table
                    .get(id.get())?
                    .ok_or(StorageError::NamespaceNotFound(id.get()))?;
                serde_json::from_slice(existing_bytes.value())
                    .map_err(|e| StorageError::Deserialize(e.to_string()))?
            };

            let mut to_store = updated.clone();
            to_store.id = existing.id;
            to_store.embedding_dim = existing.embedding_dim;
            to_store.created_at = existing.created_at;

            let bytes = serde_json::to_vec(&to_store)
                .map_err(|e| StorageError::Serialize(e.to_string()))?;
            table.insert(id.get(), bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

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

    /// Drain all memories belonging to a namespace.
    ///
    /// Processes in batches of 1,000 to avoid holding a write lock
    /// for too long. Returns the list of deleted DiskRecords.
    pub fn drain_namespace_memories(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<Vec<DiskRecord>, StorageError> {
        let mut all_deleted = Vec::new();

        loop {
            let batch_ids = {
                let ids = self.memories_in_namespace(namespace_id)?;
                if ids.is_empty() {
                    break;
                }
                ids.into_iter().take(1000).collect::<Vec<_>>()
            };

            for id in batch_ids {
                if let Some(record) = self.delete(id)? {
                    all_deleted.push(record);
                }
            }
        }

        Ok(all_deleted)
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
