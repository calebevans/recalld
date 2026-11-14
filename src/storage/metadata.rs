//! Metadata persistence backed by redb (meta.db).
//!
//! Owns the redb `Database` handle for meta.db, provides CRUD
//! operations for memory records and namespace configurations,
//! and manages secondary indexes (phase bitmaps, tag index,
//! namespace index).
//!
//! See CS-07 for the full specification.

mod lifecycle_updates;
pub(crate) mod removal_ledger;
pub(crate) mod restoration;
pub(crate) mod text_generation;

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
use crate::storage::control::{
    self, ControlState, MutationContext, MutationOutcome, MutationScope, WriteReceipt,
};
use crate::storage::error::StorageError;
use crate::storage::indexes::PhaseIndex;

// ═══════════════════════════════════════════════════════════════════════
// redb Table Definitions
// ═══════════════════════════════════════════════════════════════════════

/// Primary metadata table.
/// Key: UUID v7 bytes ([u8; 16], big-endian -- chronological sort).
/// Value: DiskRecord::to_bytes() output (variable-length, version-prefixed).
pub(crate) const META_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("memories");

/// Namespace configuration table.
/// Key: NamespaceId as u32.
/// Value: serde_json::to_vec(&NamespaceConfig) bytes.
pub(crate) const NAMESPACE_TABLE: TableDefinition<u32, &[u8]> = TableDefinition::new("namespaces");

/// Monotonic counter for namespace ID assignment.
/// Single row: key = "max_id", value = highest assigned u32.
pub(crate) const NAMESPACE_COUNTER: TableDefinition<&str, u32> =
    TableDefinition::new("namespace_counter");

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
    unavailable: std::sync::atomic::AtomicBool,
    // Lock order: engine gate -> publication -> redb -> phase_index.
    // Hold across commit and bitmap publication, including direct callers.
    publication: std::sync::RwLock<()>,
    /// Derived phase bitmap, published with canonical record commits.
    /// Legacy persisted copies are never trusted at startup.
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
        // redb's writable open can change bookkeeping. Reject historical mode
        // on a disposable copy even for callers bypassing the engine.
        if path.try_exists()? && path.metadata()?.len() > 0 {
            let (_copy, db) = super::validation::inspect_database_copy(path, true)?;
            let read = db.begin_read()?;
            if let Some(control) = control::validate_existing(&read)? {
                super::lineage::ensure_serving(&read, &control)?;
            }
        }
        let db = redb::Builder::new()
            .set_cache_size(16 * 1024 * 1024)
            .create(path)?;

        // Inspect additive canonical state before any create/adoption transaction.
        // Partial or future layouts must never be repaired into a new lineage.
        let pristine;
        {
            let read = db.begin_read()?;
            text_generation::read_selection(&read)?;
            pristine = read.list_tables()?.next().is_none()
                && read.list_multimap_tables()?.next().is_none();
            let control = control::validate_existing(&read)?;
            removal_ledger::read_export(&read, control.as_ref())?;
            if control.is_some() {
                // An adopted store must retain its canonical tables. Do not
                // recreate records, namespaces or the never-reused ID counter.
                read.open_table(META_TABLE)?;
                read.open_table(NAMESPACE_TABLE)?;
                read.open_table(NAMESPACE_COUNTER)?;
            }
        }

        // Force-create all tables by opening them in a write transaction.
        {
            let write_txn = db.begin_write()?;
            control::initialize(&write_txn)?;
            let _ = write_txn.open_table(META_TABLE)?;
            let _ = write_txn.open_table(NAMESPACE_TABLE)?;
            let _ = write_txn.open_table(NAMESPACE_COUNTER)?;
            let _ = write_txn.open_table(INDEX_TABLE)?;
            let _ = write_txn.open_multimap_table(TAG_INDEX)?;
            let _ = write_txn.open_multimap_table(NAMESPACE_INDEX)?;
            removal_ledger::initialize(&write_txn, pristine)?;
            write_txn.commit()?;
        }

        // Legacy checkpoints have no generation watermark. Always rebuild
        // from canonical records, even for direct MetadataStore callers.
        let store = Self {
            db,
            unavailable: std::sync::atomic::AtomicBool::new(false),
            publication: std::sync::RwLock::new(()),
            phase_index: std::sync::RwLock::new(PhaseIndex::new()),
        };
        store.rebuild_phase_index()?;
        Ok(store)
    }
}

impl MetadataStore {
    /// A failed publication/uncertain commit poisons this handle until reopen.
    /// Caller holds the outer storage write gate or metadata publication guard.
    pub(crate) fn mark_unavailable(&self) {
        self.unavailable
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn ensure_available(&self) -> Result<(), StorageError> {
        if self.unavailable.load(std::sync::atomic::Ordering::Acquire) {
            return Err(StorageError::RecoveryRequired(
                "store handle unavailable; reopen for recovery".into(),
            ));
        }
        Ok(())
    }

    fn commit(&self, tx: redb::WriteTransaction) -> Result<(), StorageError> {
        tx.commit().map_err(|e| {
            self.mark_unavailable();
            StorageError::RedbCommit(e)
        })
    }

    fn mutation_guard(&self) -> Result<std::sync::RwLockWriteGuard<'_, ()>, StorageError> {
        let guard = self.publication.write().unwrap_or_else(|e| e.into_inner());
        // Recheck after waiting: the preceding writer may have failed.
        self.ensure_available()?;
        let read = self.db.begin_read()?;
        let table = read.open_table(control::CONTROL)?;
        let value = table
            .get("state")?
            .ok_or_else(|| StorageError::InvalidControl("missing control".into()))?;
        let control = ControlState::decode(value.value())?;
        crate::storage::lineage::ensure_serving(&read, &control)?;
        Ok(guard)
    }
}

// ── Core Methods ────────────────────────────────────────────────────

impl MetadataStore {
    /// Generic read-modify-write helper for a single memory record.
    ///
    /// Opens a write transaction, loads the record for `id`, passes it
    /// to the closure `f` for mutation, re-serializes, writes back,
    /// commits and publishes phase changes under the publication gate.
    /// Returns the committed record.
    ///
    /// # Errors
    /// - `StorageError::NotFound` if no record exists for this ID.
    fn update_record<F>(&self, id: &MemoryId, f: F) -> Result<DiskRecord, StorageError>
    where
        F: FnOnce(&mut DiskRecord) -> Result<(), StorageError>,
    {
        let _publication = self.mutation_guard()?;
        let old_phase;
        let before;
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

            old_phase = record.phase;
            before = record.clone();
            f(&mut record)?;

            table.insert(key, record.to_bytes().as_slice())?;
            record
        };
        let state = control::advance_generation(&write_txn)?;
        removal_ledger::record_change(
            &write_txn,
            state.generation_ref(),
            &before,
            &record,
            uuid::Uuid::new_v4(),
        )?;
        self.commit(write_txn)?;

        if old_phase != record.phase {
            self.phase_index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .transition(
                    record.namespace_id,
                    record.vector_slot,
                    old_phase,
                    record.phase,
                );
        }
        Ok(record)
    }

    /// Persist a new memory record. Updates all secondary indexes
    /// (phase bitmap, tag index, namespace index) in the same
    /// write transaction.
    ///
    /// # Errors
    /// - `StorageError::DuplicateId` if a record with this ID already exists.
    pub fn insert(&self, id: MemoryId, record: &DiskRecord) -> Result<(), StorageError> {
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
        if record.id != *id.as_bytes() {
            return Err(StorageError::InvalidMutation(
                "record ID disagrees with key".into(),
            ));
        }
        let key = id.as_bytes().as_slice();
        let value = record.to_bytes();

        let write_txn = self.db.begin_write()?;
        removal_ledger::ensure_insertable(&write_txn, id)?;
        {
            // 1. Insert into primary table (reject duplicates).
            let mut meta = write_txn.open_table(META_TABLE)?;
            if meta.get(key)?.is_some() {
                return Err(StorageError::DuplicateId(id.into_inner()));
            }
            meta.insert(key, value.as_slice())?;

            // 2. Update tag index.
            let mut tags = write_txn.open_multimap_table(TAG_INDEX)?;
            if record.phase != DecayPhase::Tombstone {
                for tag in &record.tags {
                    tags.insert(tag.as_str(), key)?;
                }
                let mut ns_idx = write_txn.open_multimap_table(NAMESPACE_INDEX)?;
                ns_idx.insert(record.namespace_id, key)?;
            }
        }
        control::advance_generation(&write_txn)?;
        self.commit(write_txn)?;

        // 4. Publish the actual committed phase; tombstones own no slot.
        if record.phase != DecayPhase::Tombstone {
            let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
            pi.transition(
                record.namespace_id,
                record.vector_slot,
                DecayPhase::Full,
                record.phase,
            );
        }

        Ok(())
    }

    /// Point lookup by memory ID.
    ///
    /// Returns `None` if no record exists for this ID.
    pub fn get(&self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError> {
        self.ensure_available()?;
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
    ///
    /// # Errors
    /// `StorageError::NotFound` if the record does not exist, or if it
    /// has been tombstoned. Tombstone is terminal, and it is enforced
    /// here rather than left to callers because every caller decides
    /// from a record it read earlier: the decay sweep works from a
    /// per-phase snapshot of ids, and `reinforce_memory` reads, computes
    /// and then writes. A `forget_memory` landing in between used to
    /// have its tombstone overwritten with the caller's stale phase,
    /// which brought the record back in a live phase with its content
    /// stripped, its `NAMESPACE_INDEX` entry gone, and a `vector_slot`
    /// that had already been freed and could belong to another memory.
    pub fn update_decay_state(
        &self,
        id: MemoryId,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
        stability: f32,
        is_permastore: bool,
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.update_record(&id, |record| {
            if record.phase == DecayPhase::Tombstone {
                return Err(StorageError::NotFound(id.into_inner()));
            }
            record.phase = phase;
            record.strength = strength;
            record.decay_strength = decay_strength;
            record.stability = stability;
            record.is_permastore = if is_permastore { 1 } else { 0 };
            Ok(())
        })?;

        Ok(())
    }

    /// Zero the full_text pointer on a memory record.
    ///
    /// Called during Full -> Summary phase transitions to mark the
    /// text data in fulltext.dat as dead space reclaimable by
    /// compaction. Does not modify fulltext.dat itself.
    pub fn strip_full_text(&self, id: MemoryId) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.update_record(&id, |record| {
            record.text_offset = 0;
            record.text_length = 0;
            Ok(())
        })?;
        Ok(())
    }

    /// Clear the summary field on a memory record.
    ///
    /// Called during Summary -> Ghost phase transitions. After this,
    /// only the embedding and relationship edges remain.
    pub fn strip_summary(&self, id: MemoryId) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.update_record(&id, |record| {
            record.summary = String::new();
            Ok(())
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
        self.ensure_available()?;
        let record = self
            .get(id)?
            .ok_or(StorageError::NotFound(id.into_inner()))?;
        self.update_access_with_receipt(
            &MutationContext::unkeyed(NamespaceId::new(record.namespace_id)),
            id,
            timestamp,
            kind,
        )?;
        Ok(())
    }

    /// The current metadata control state. This is not a coherent cross-file
    /// read view or a derived readiness watermark (see `storage::control`).
    pub fn control_state(&self) -> Result<ControlState, StorageError> {
        self.ensure_available()?;
        let tx = self.db.begin_read()?;
        let table = tx.open_table(control::CONTROL)?;
        let value = table
            .get("state")?
            .ok_or_else(|| StorageError::InvalidControl("missing control state".into()))?;
        ControlState::decode(value.value())
    }

    /// Historical receipt lookup uses immutable scope, even after namespace
    /// purge. It never looks up or returns the record's removed content.
    pub fn lookup_operation(
        &self,
        store_id: uuid::Uuid,
        scope: MutationScope,
        operation_id: uuid::Uuid,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        self.ensure_available()?;
        let tx = self.db.begin_read()?;
        let table = tx.open_table(control::CONTROL)?;
        let value = table
            .get("state")?
            .ok_or_else(|| StorageError::InvalidControl("missing control state".into()))?;
        if ControlState::decode(value.value())?.store_id != store_id {
            return Err(StorageError::StoreLineageMismatch);
        }
        let table = tx.open_table(control::RECEIPTS)?;
        table
            .get(control::scoped_key(store_id, scope, operation_id.as_bytes()).as_slice())?
            .map(|v| control::validate_receipt(v.value()).map(|r| r.receipt))
            .transpose()
    }

    /// Atomic access patch + generation + receipt. `timestamp` is the sampled
    /// server clock and deliberately excluded from retry identity; replay uses
    /// the original committed timestamp. Callers with client-supplied timestamps
    /// need a distinct operation/hash contract, not this method.
    ///
    /// Success certifies ONLY the canonical metadata commit. Search/cache
    /// publication is not tracked here and must not be reported ready by callers.
    pub fn update_access_with_receipt(
        &self,
        ctx: &MutationContext,
        id: MemoryId,
        timestamp: i64,
        kind: AccessKind,
    ) -> Result<WriteReceipt, StorageError> {
        self.ensure_available()?;
        let publication = self.mutation_guard()?;
        let (tx, receipt) = self.prepare_access(&publication, ctx, id, timestamp, kind)?;
        if !receipt.replayed {
            tx.commit().map_err(|source| {
                self.mark_unavailable();
                StorageError::CommitOutcomeUnknown {
                    operation_id: receipt.operation_id,
                    source,
                }
            })?;
        }
        Ok(receipt)
    }

    // Keep preparation and commit separate so tests can abandon the actual
    // transaction at the last pre-commit boundary, without a fake backend.
    fn prepare_access(
        &self,
        _publication: &std::sync::RwLockWriteGuard<'_, ()>,
        ctx: &MutationContext,
        id: MemoryId,
        timestamp: i64,
        kind: AccessKind,
    ) -> Result<(redb::WriteTransaction, WriteReceipt), StorageError> {
        let kind_bytes = control::encode(&kind)?;
        let hash = control::request_hash(ctx, "record_access", &[id.as_bytes(), &kind_bytes]);
        let tx = self.db.begin_write()?;
        if let Some(receipt) = control::replay_or_check(&tx, ctx, hash)? {
            return Ok((tx, receipt));
        }
        {
            let mut table = tx.open_table(META_TABLE)?;
            let mut record = {
                let value = table
                    .get(id.as_bytes().as_slice())?
                    .ok_or(StorageError::NotFound(id.into_inner()))?;
                DiskRecord::from_bytes(value.value())?
            };
            if ctx.scope != MutationScope::Namespace(NamespaceId::new(record.namespace_id)) {
                return Err(StorageError::MutationNamespaceMismatch);
            }
            if record.phase == DecayPhase::Tombstone {
                return Err(StorageError::NotFound(id.into_inner()));
            }
            record.last_accessed_at = timestamp;
            if record.access_history.len() >= ACCESS_HISTORY_MAX {
                record.access_history.remove(0);
            }
            record.access_history.push(AccessEvent { timestamp, kind });
            table.insert(id.as_bytes().as_slice(), record.to_bytes().as_slice())?;
        }
        let receipt = control::record_receipt(
            &tx,
            ctx,
            hash,
            MutationOutcome::AccessRecorded {
                memory_id: id,
                accessed_at: timestamp,
            },
        )?;
        Ok((tx, receipt))
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
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
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
        let state = control::advance_generation(&write_txn)?;
        removal_ledger::record_removal(
            &write_txn,
            state.generation_ref(),
            deleted_record.namespace_id,
            id,
            Some(removal_ledger::TerminalKind::HardDelete),
            removal_ledger::ALL,
            uuid::Uuid::new_v4(),
        )?;
        self.commit(write_txn)?;

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
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
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
        let state = control::advance_generation(&write_txn)?;
        removal_ledger::record_removal(
            &write_txn,
            state.generation_ref(),
            old_namespace_id,
            id,
            Some(removal_ledger::TerminalKind::Tombstone),
            removal_ledger::ALL,
            uuid::Uuid::new_v4(),
        )?;
        self.commit(write_txn)?;

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
        self.ensure_available()?;
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        if phase == DecayPhase::Tombstone {
            return Ok(self
                .scan_all()?
                .into_iter()
                .filter(|(_, r)| r.phase == phase)
                .map(|(id, _)| id)
                .collect());
        }
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
            if record.phase == phase
                && slot_set.contains(&(record.namespace_id, record.vector_slot))
            {
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
        self.ensure_available()?;
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        if phase == DecayPhase::Tombstone {
            return Ok(self
                .scan_all()?
                .into_iter()
                .filter(|(_, r)| r.phase == phase)
                .collect());
        }
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
            if record.phase == phase
                && ns_slots.contains(&(record.namespace_id, record.vector_slot))
            {
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
        self.ensure_available()?;
        self.update_record(&id, |record| {
            record.edge_count = edge_count;
            Ok(())
        })?;

        Ok(())
    }
}

// ── Tag Index (Section 6.2) ─────────────────────────────────────────

impl MetadataStore {
    /// Find all memory IDs that carry a given tag.
    pub fn memories_with_tag(&self, tag: &str) -> Result<Vec<MemoryId>, StorageError> {
        self.ensure_available()?;
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
        self.ensure_available()?;
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
        self.ensure_available()?;
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
        self.ensure_available()?;
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
        self.ensure_available()?;
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
        self.ensure_available()?;
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
    /// Checkpoints are persisted separately; startup rebuilds from records.
    ///
    /// # Returns
    /// The number of records that were transitioned.
    pub fn decay_sweep(
        &self,
        compute_new_state: impl Fn(&DiskRecord) -> Option<DiskRecord>,
    ) -> Result<usize, StorageError> {
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
        let mut updates = Vec::new();
        for (id, record) in self.scan_all()? {
            if record.phase != DecayPhase::Tombstone {
                if let Some(updated) = compute_new_state(&record) {
                    updates.push((id, updated));
                }
            }
        }
        self.batch_update_records_locked(&updates)?;
        Ok(updates.len())
    }

    /// Batch replacement with immutable identity/slot and coherent indexes.
    pub fn batch_update_records(
        &self,
        records: &[(MemoryId, DiskRecord)],
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
        self.batch_update_records_locked(records)
    }

    // Caller holds the publication write gate.
    fn batch_update_records_locked(
        &self,
        records: &[(MemoryId, DiskRecord)],
    ) -> Result<(), StorageError> {
        if records.is_empty() {
            return Ok(());
        }
        let write_txn = self.db.begin_write()?;
        let mut transitions = Vec::new();
        let state = control::advance_generation(&write_txn)?;
        let operation_id = uuid::Uuid::new_v4();
        {
            let mut table = write_txn.open_table(META_TABLE)?;
            let mut tags = write_txn.open_multimap_table(TAG_INDEX)?;
            let mut namespaces = write_txn.open_multimap_table(NAMESPACE_INDEX)?;
            for (id, record) in records {
                let key = id.as_bytes().as_slice();
                let old = DiskRecord::from_bytes(
                    table
                        .get(key)?
                        .ok_or(StorageError::NotFound(id.into_inner()))?
                        .value(),
                )?;
                if old.phase == DecayPhase::Tombstone {
                    return Err(StorageError::NotFound(id.into_inner()));
                }
                if record.id != *id.as_bytes()
                    || record.namespace_id != old.namespace_id
                    || record.vector_slot != old.vector_slot
                {
                    return Err(StorageError::InvalidMutation(
                        "batch replacement changes identity or vector ownership".into(),
                    ));
                }
                for tag in &old.tags {
                    tags.remove(tag.as_str(), key)?;
                }
                for tag in &record.tags {
                    tags.insert(tag.as_str(), key)?;
                }
                if record.phase == DecayPhase::Tombstone {
                    namespaces.remove(record.namespace_id, key)?;
                }
                removal_ledger::record_change(
                    &write_txn,
                    state.generation_ref(),
                    &old,
                    record,
                    operation_id,
                )?;
                table.insert(key, record.to_bytes().as_slice())?;
                transitions.push((
                    record.namespace_id,
                    record.vector_slot,
                    old.phase,
                    record.phase,
                ));
            }
        }
        self.commit(write_txn)?;
        let mut pi = self.phase_index.write().unwrap_or_else(|e| e.into_inner());
        for (ns, slot, from, to) in transitions {
            pi.transition(ns, slot, from, to);
        }
        Ok(())
    }

    /// Persist the current in-memory phase bitmaps to INDEX_TABLE.
    /// Called periodically (end of decay sweep, graceful shutdown).
    pub fn persist_phase_index(&self) -> Result<(), StorageError> {
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
        let bytes = {
            let pi = self.phase_index.read().unwrap_or_else(|e| e.into_inner());
            pi.to_bytes()
        };
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(INDEX_TABLE)?;
            table.insert("phase_bitmaps", bytes.as_slice())?;
        }
        self.commit(write_txn)?;
        Ok(())
    }

    /// Rebuild the phase index by scanning all records in META_TABLE.
    /// Called during startup validation when the persisted bitmap is
    /// missing, corrupt, or when the record count does not match the
    /// bitmap cardinality.
    pub fn rebuild_phase_index(&self) -> Result<(), StorageError> {
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
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
        self.commit(write_txn)?;

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
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
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
            new_id = current_max
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidControl("namespace ID exhausted".into()))?;
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
        control::advance_generation(&write_txn)?;
        self.commit(write_txn)?;

        Ok(NamespaceId::new(new_id))
    }

    /// List all namespace configurations.
    pub fn list_namespaces(&self) -> Result<Vec<NamespaceConfig>, StorageError> {
        self.ensure_available()?;
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
        self.ensure_available()?;
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
        self.ensure_available()?;
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
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
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
        control::advance_generation(&write_txn)?;
        self.commit(write_txn)?;
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
    /// transaction. The publication gate spans the purge so phase readers
    /// cannot observe partially published chunks. This is *resumable but not
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
        let _publication = self.mutation_guard().map_err(|source| PartialPurge {
            destroyed: Vec::new(),
            source,
        })?;
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

    /// Count the live records a [`purge_namespace_records`] call would
    /// destroy.
    ///
    /// # Why not `count_memories_in_namespace`
    ///
    /// That one walks `NAMESPACE_INDEX` and confirms each hit against
    /// `META_TABLE`; the purge scans `META_TABLE` and filters on
    /// `record.namespace_id`. They read different sources of truth, so a
    /// `META_TABLE` row whose index entry is missing is invisible to the
    /// count and destroyed by the purge — which is precisely the row a
    /// non-force "is this namespace empty?" guard exists to protect. A
    /// check and an effect that disagree is not a guard.
    ///
    /// Tombstones are excluded, matching the guard's rule that a
    /// namespace holding only tombstones is empty: they hold no content
    /// the user has not already asked to forget, and they report as zero
    /// everywhere else.
    ///
    /// This is a full scan. It runs only on the non-force deletion path,
    /// immediately before a purge that scans the same table anyway.
    pub fn count_live_records_in_namespace(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<u64, StorageError> {
        self.ensure_available()?;
        let ns = namespace_id.get();
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;

        let mut count = 0u64;
        for result in table.iter()? {
            let (_, value) = result?;
            let record = DiskRecord::from_bytes(value.value())?;
            if record.namespace_id == ns && record.phase != DecayPhase::Tombstone {
                count += 1;
            }
        }
        Ok(count)
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
        let state = control::advance_generation(&write_txn)?;
        let operation_id = uuid::Uuid::new_v4();
        for (id, record) in chunk {
            removal_ledger::record_removal(
                &write_txn,
                state.generation_ref(),
                record.namespace_id,
                *id,
                Some(removal_ledger::TerminalKind::HardDelete),
                removal_ledger::ALL,
                operation_id,
            )?;
        }
        self.commit(write_txn)?;
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
        self.ensure_available()?;
        let _publication = self.mutation_guard()?;
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
        let state = control::advance_generation(&write_txn)?;
        let operation_id = uuid::Uuid::new_v4();
        for outcome in &outcomes {
            if let TombstoneOutcome::Tombstoned(record) = outcome {
                removal_ledger::record_removal(
                    &write_txn,
                    state.generation_ref(),
                    record.namespace_id,
                    MemoryId::from_bytes(record.id),
                    Some(removal_ledger::TerminalKind::Tombstone),
                    removal_ledger::ALL,
                    operation_id,
                )?;
            }
        }
        self.commit(write_txn)?;

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
        self.ensure_available()?;
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
        self.ensure_available()?;
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;
        Ok(table.len()?)
    }

    /// Count only active (non-tombstoned) memory records.
    pub fn count_active(&self) -> Result<u64, StorageError> {
        self.ensure_available()?;
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
        self.ensure_available()?;
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

    #[test]
    fn containment_publication_blocks_second_writer_and_phase_reader_after_commit() {
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};
        let (_dir, store) = open_store();
        let id = insert(&store, 1, 0, &[]);
        let store = Arc::new(store);
        // Hold the last lock in the order to pause writer 1 after its redb commit.
        let bitmap = store.phase_index.write().unwrap();
        let writer_store = store.clone();
        let writer = std::thread::spawn(move || {
            writer_store
                .update_decay_state(id, DecayPhase::Summary, 0.8, 0.8, 1., false)
                .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.get(id).unwrap().unwrap().phase != DecayPhase::Summary {
            assert!(Instant::now() < deadline, "writer did not commit");
            std::thread::yield_now();
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let second_store = store.clone();
        let second = std::thread::spawn(move || {
            second_store
                .update_record(&id, |r| {
                    entered_tx.send(()).unwrap();
                    r.phase = DecayPhase::Ghost;
                    Ok(())
                })
                .unwrap()
        });
        let (read_tx, read_rx) = mpsc::channel();
        let reader_store = store.clone();
        let reader = std::thread::spawn(move || {
            read_tx
                .send(
                    reader_store
                        .scan_phase_records(DecayPhase::Summary)
                        .unwrap(),
                )
                .unwrap();
        });
        let writer_blocked = entered_rx.recv_timeout(Duration::from_millis(100)).is_err();
        let reader_blocked = read_rx.recv_timeout(Duration::from_millis(100)).is_err();
        drop(bitmap);
        writer.join().unwrap();
        second.join().unwrap();
        reader.join().unwrap();
        assert!(writer_blocked && reader_blocked);
        assert_eq!(store.ids_in_phase(DecayPhase::Ghost).unwrap(), vec![id]);
        assert!(store.ids_in_phase(DecayPhase::Summary).unwrap().is_empty());
    }

    #[test]
    fn containment_reopen_rebuilds_corrupt_checkpoint_and_batch_publishes_phase() {
        let (dir, store) = open_store();
        let id = insert(&store, 1, 0, &["before"]);
        let mut record = store.get(id).unwrap().unwrap();
        record.phase = DecayPhase::Ghost;
        record.tags.clear();
        store.batch_update_records(&[(id, record.clone())]).unwrap();
        assert_eq!(store.ids_in_phase(DecayPhase::Ghost).unwrap(), vec![id]);
        assert!(store.memories_with_tag("before").unwrap().is_empty());
        store.tombstone(id).unwrap();
        assert!(store.batch_update_records(&[(id, record)]).is_err());
        let other = insert(&store, 1, 0, &[]);
        assert_eq!(store.ids_in_phase(DecayPhase::Full).unwrap(), vec![other]);
        let tx = store.db.begin_write().unwrap();
        {
            tx.open_table(INDEX_TABLE)
                .unwrap()
                .insert("phase_bitmaps", b"bad".as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
        drop(store);
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_eq!(store.ids_in_phase(DecayPhase::Full).unwrap(), vec![other]);
        assert_eq!(store.ids_in_phase(DecayPhase::Tombstone).unwrap(), vec![id]);
    }

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

    #[test]
    fn integration_access_receipt_cannot_be_overwritten_by_a_paused_sweep() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let (_dir, store) = open_store();
        let id = insert(&store, 1, 0, &[]);
        let ctx = MutationContext::unkeyed(NamespaceId::new(1));
        let store = Arc::new(store);
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let sweeping = store.clone();
        let sweep = std::thread::spawn(move || {
            sweeping
                .decay_sweep(|record| {
                    snapshot_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    let mut updated = record.clone();
                    updated.phase = DecayPhase::Summary;
                    Some(updated)
                })
                .unwrap()
        });
        snapshot_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let accessing = store.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let access = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let receipt = accessing
                .update_access_with_receipt(&ctx, id, 123, AccessKind::DirectRetrieval)
                .unwrap();
            done_tx.send(()).unwrap();
            receipt
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let blocked = done_rx.recv_timeout(Duration::from_millis(100)).is_err();
        resume_tx.send(()).unwrap();
        assert_eq!(sweep.join().unwrap(), 1);
        let receipt = access.join().unwrap();
        assert!(
            blocked,
            "access committed while sweep could still overwrite its record"
        );
        let record = store.get(id).unwrap().unwrap();
        assert_eq!(record.phase, DecayPhase::Summary);
        assert_eq!(record.last_accessed_at, 123);
        assert_eq!(record.access_history.len(), 1);
        assert_eq!(store.ids_in_phase(DecayPhase::Summary).unwrap(), vec![id]);
        assert_eq!(
            receipt.committed_generation,
            store.control_state().unwrap().committed_generation
        );
    }

    #[test]
    fn integration_control_partial_table_sets_never_initialize_missing_tables() {
        use redb::TableHandle;
        for mask in 1..7 {
            let (dir, store) = open_store();
            let tx = store.db.begin_write().unwrap();
            if mask & 1 == 0 {
                tx.delete_table(control::CONTROL).unwrap();
            }
            if mask & 2 == 0 {
                tx.delete_table(control::RECEIPTS).unwrap();
            }
            if mask & 4 == 0 {
                tx.delete_table(control::KEYS).unwrap();
            }
            tx.commit().unwrap();
            let before: Vec<_> = store
                .db
                .begin_read()
                .unwrap()
                .list_tables()
                .unwrap()
                .map(|t| t.name().to_owned())
                .collect();
            drop(store);
            assert!(
                MetadataStore::open(&dir.path().join("meta.db")).is_err(),
                "mask {mask}"
            );
            let db = Database::open(dir.path().join("meta.db")).unwrap();
            let after: Vec<_> = db
                .begin_read()
                .unwrap()
                .list_tables()
                .unwrap()
                .map(|t| t.name().to_owned())
                .collect();
            assert_eq!(after, before);
        }
    }

    #[test]
    fn integration_adopted_control_never_recreates_missing_canonical_tables() {
        for missing in ["memories", "namespaces", "namespace_counter"] {
            let (dir, store) = open_store();
            let tx = store.db.begin_write().unwrap();
            match missing {
                "memories" => {
                    tx.delete_table(META_TABLE).unwrap();
                }
                "namespaces" => {
                    tx.delete_table(NAMESPACE_TABLE).unwrap();
                }
                "namespace_counter" => {
                    tx.delete_table(NAMESPACE_COUNTER).unwrap();
                }
                _ => unreachable!(),
            }
            tx.commit().unwrap();
            drop(store);
            assert!(
                MetadataStore::open(&dir.path().join("meta.db")).is_err(),
                "{missing}"
            );
        }
    }

    #[test]
    fn integration_control_versions_types_and_receipt_references_fail_before_open() {
        for mode in [
            "receipt_type",
            "key_type",
            "multimap",
            "extra_state",
            "receipt_version",
            "hash_version",
            "future_generation",
            "wrong_lineage",
            "wrong_operation",
            "missing_receipt",
            "foreign_scope",
        ] {
            let (dir, store) = open_store();
            let id = insert(&store, 1, 0, &[]);
            let ctx = keyed_context(&store, 1, "original");
            let receipt = store
                .update_access_with_receipt(&ctx, id, 123, AccessKind::DirectRetrieval)
                .unwrap();
            let key =
                control::scoped_key(receipt.store_id, ctx.scope, receipt.operation_id.as_bytes());
            let tx = store.db.begin_write().unwrap();
            match mode {
                "receipt_type" => {
                    tx.delete_table(control::RECEIPTS).unwrap();
                    tx.open_table(redb::TableDefinition::<u64, u64>::new("mutation_receipts"))
                        .unwrap();
                }
                "key_type" => {
                    tx.delete_table(control::KEYS).unwrap();
                    tx.open_table(redb::TableDefinition::<u64, u64>::new("mutation_keys"))
                        .unwrap();
                }
                "multimap" => {
                    tx.delete_table(control::CONTROL).unwrap();
                    tx.open_multimap_table(redb::MultimapTableDefinition::<u64, u64>::new(
                        "canonical_control",
                    ))
                    .unwrap();
                }
                "extra_state" => {
                    tx.open_table(control::CONTROL)
                        .unwrap()
                        .insert("unexpected", b"{}".as_slice())
                        .unwrap();
                }
                "missing_receipt" => {
                    tx.open_table(control::RECEIPTS)
                        .unwrap()
                        .remove(key.as_slice())
                        .unwrap();
                }
                "foreign_scope" => {
                    let wrong = control::scoped_key(
                        receipt.store_id,
                        MutationScope::Namespace(NamespaceId::new(2)),
                        b"foreign",
                    );
                    tx.open_table(control::KEYS)
                        .unwrap()
                        .insert(wrong.as_slice(), key.as_slice())
                        .unwrap();
                }
                _ => {
                    let mut table = tx.open_table(control::RECEIPTS).unwrap();
                    let mut stored: control::StoredReceipt =
                        control::decode(table.get(key.as_slice()).unwrap().unwrap().value())
                            .unwrap();
                    match mode {
                        "receipt_version" => stored.receipt.receipt_version = 2,
                        "hash_version" => stored.request_hash_version = 2,
                        "future_generation" => {
                            stored.receipt.committed_generation = control::Generation::MAX
                        }
                        "wrong_lineage" => stored.receipt.store_id = uuid::Uuid::new_v4(),
                        "wrong_operation" => stored.receipt.operation_id = uuid::Uuid::new_v4(),
                        _ => unreachable!(),
                    }
                    table
                        .insert(key.as_slice(), control::encode(&stored).unwrap().as_slice())
                        .unwrap();
                }
            }
            tx.commit().unwrap();
            assert!(
                control::validate_existing(&store.db.begin_read().unwrap()).is_err(),
                "{mode}"
            );
            drop(store);
            assert!(
                MetadataStore::open(&dir.path().join("meta.db")).is_err(),
                "{mode}"
            );
        }
    }

    fn keyed_context(store: &MetadataStore, namespace_id: u32, key: &str) -> MutationContext {
        let state = store.control_state().unwrap();
        MutationContext {
            store_id: Some(state.store_id),
            scope: MutationScope::Namespace(NamespaceId::new(namespace_id)),
            idempotency_key: Some(key.into()),
            expected_generation: Some(state.committed_generation),
            contract_version: 1,
        }
    }

    #[test]
    fn access_receipt_aborted_transaction_and_overflow_leave_no_mutation() {
        let (dir, store) = open_store();
        let id = insert(&store, 1, 0, &[]);
        let ctx = keyed_context(&store, 1, "abort");
        let before = store.control_state().unwrap();
        // All three writes have been staged in the real redb transaction.
        let publication = store.publication.write().unwrap();
        let (tx, abandoned) = store
            .prepare_access(&publication, &ctx, id, 123, AccessKind::DirectRetrieval)
            .unwrap();
        drop(tx);
        drop(publication);
        drop(store);
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_eq!(store.control_state().unwrap(), before);
        assert!(store.get(id).unwrap().unwrap().access_history.is_empty());
        assert!(
            store
                .lookup_operation(before.store_id, ctx.scope, abandoned.operation_id)
                .unwrap()
                .is_none()
        );
        let receipt = store
            .update_access_with_receipt(&ctx, id, 456, AccessKind::DirectRetrieval)
            .unwrap();
        assert!(!receipt.replayed);
        assert_eq!(
            receipt.committed_generation.get(),
            before.committed_generation.get() + 1
        );

        let mut exhausted = store.control_state().unwrap();
        exhausted.committed_generation = control::Generation::MAX;
        let tx = store.db.begin_write().unwrap();
        tx.open_table(control::CONTROL)
            .unwrap()
            .insert("state", control::encode(&exhausted).unwrap().as_slice())
            .unwrap();
        tx.commit().unwrap();
        let ctx = keyed_context(&store, 1, "overflow");
        let before_record = store.get(id).unwrap().unwrap().to_bytes();
        assert!(matches!(
            store.update_access_with_receipt(&ctx, id, 789, AccessKind::DirectRetrieval),
            Err(StorageError::GenerationExhausted)
        ));
        assert_eq!(store.get(id).unwrap().unwrap().to_bytes(), before_record);
        assert_eq!(store.control_state().unwrap(), exhausted);
        assert_eq!(
            store
                .db
                .begin_read()
                .unwrap()
                .open_table(control::RECEIPTS)
                .unwrap()
                .len()
                .unwrap(),
            1
        );
    }

    #[test]
    fn access_receipt_replay_precedes_cas_but_never_lineage_check() {
        let (_dir, store) = open_store();
        let id = insert(&store, 1, 0, &[]);
        let ctx = keyed_context(&store, 1, "access");
        let original = store
            .update_access_with_receipt(&ctx, id, 123, AccessKind::DirectRetrieval)
            .unwrap();
        let unrelated = insert(&store, 2, 0, &[]);
        let before = store.control_state().unwrap();
        let replay = store
            .update_access_with_receipt(&ctx, id, 999, AccessKind::DirectRetrieval)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, original.operation_id);
        assert_eq!(replay.outcome, original.outcome);
        assert_eq!(replay.committed_generation, original.committed_generation);
        assert_eq!(store.control_state().unwrap(), before);
        assert_eq!(store.get(id).unwrap().unwrap().access_history.len(), 1);
        assert_eq!(store.get(id).unwrap().unwrap().last_accessed_at, 123);

        let mut stale = ctx.clone();
        stale.idempotency_key = Some("new-key".into());
        assert!(matches!(
            store.update_access_with_receipt(&stale, id, 999, AccessKind::DirectRetrieval),
            Err(StorageError::GenerationConflict { .. })
        ));
        let mut foreign = ctx.clone();
        foreign.store_id = Some(uuid::Uuid::new_v4());
        assert!(matches!(
            store.update_access_with_receipt(&foreign, id, 999, AccessKind::DirectRetrieval),
            Err(StorageError::StoreLineageMismatch)
        ));
        assert!(matches!(
            store.update_access_with_receipt(&ctx, unrelated, 999, AccessKind::DirectRetrieval),
            Err(StorageError::IdempotencyConflict)
        ));
        assert!(matches!(
            store.update_access_with_receipt(&ctx, id, 999, AccessKind::ManualReinforcement),
            Err(StorageError::IdempotencyConflict)
        ));
        let tx = store.db.begin_write().unwrap();
        assert!(matches!(
            control::replay_or_check(
                &tx,
                &ctx,
                control::request_hash(&ctx, "another_operation", &[id.as_bytes()])
            ),
            Err(StorageError::IdempotencyConflict)
        ));
        drop(tx);
        assert_eq!(store.control_state().unwrap(), before);
    }

    #[test]
    fn access_receipts_serialize_concurrent_retries_and_scope_keys_by_namespace() {
        let (_dir, store) = open_store();
        let store = std::sync::Arc::new(store);
        let id = insert(&store, 1, 0, &[]);
        let other = insert(&store, 2, 0, &[]);
        let ctx = keyed_context(&store, 1, "same-key");
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                let ctx = ctx.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    store
                        .update_access_with_receipt(&ctx, id, i, AccessKind::DirectRetrieval)
                        .unwrap()
                })
            })
            .collect();
        let receipts: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(receipts.iter().filter(|r| !r.replayed).count(), 1);
        assert!(
            receipts
                .iter()
                .all(|r| r.operation_id == receipts[0].operation_id)
        );
        assert_eq!(store.get(id).unwrap().unwrap().access_history.len(), 1);
        let other_ctx = keyed_context(&store, 2, "same-key");
        let other_receipt = store
            .update_access_with_receipt(&other_ctx, other, 456, AccessKind::DirectRetrieval)
            .unwrap();
        assert!(!other_receipt.replayed);
        assert_ne!(other_receipt.operation_id, receipts[0].operation_id);
        let wrong_scope = keyed_context(&store, 2, "wrong-scope");
        let before = store.control_state().unwrap();
        assert!(matches!(
            store.update_access_with_receipt(&wrong_scope, id, 0, AccessKind::DirectRetrieval),
            Err(StorageError::MutationNamespaceMismatch)
        ));
        assert_eq!(store.control_state().unwrap(), before);
    }

    #[test]
    fn legacy_metadata_mutations_advance_generation_but_reads_and_checkpoints_do_not() {
        let (_dir, store) = open_store();
        let mut generation = 0;
        let mut advanced = || {
            generation += 1;
            assert_eq!(
                store.control_state().unwrap().committed_generation.get(),
                generation
            );
        };
        let ns = store
            .create_namespace(&NamespaceConfig::default_namespace(0))
            .unwrap();
        advanced();
        let id = insert(&store, ns.get(), 0, &[]);
        advanced();
        store
            .update_access(id, 123, AccessKind::DirectRetrieval)
            .unwrap();
        advanced();
        store.update_edge_count(id, 3).unwrap();
        advanced();
        store
            .update_decay_state(id, DecayPhase::Summary, 0.5, 0.5, 1.0, false)
            .unwrap();
        advanced();
        store.strip_full_text(id).unwrap();
        advanced();
        store.strip_summary(id).unwrap();
        advanced();
        store
            .batch_update_records(&[(id, store.get(id).unwrap().unwrap())])
            .unwrap();
        advanced();
        store
            .decay_sweep(|r| {
                let mut r = r.clone();
                r.strength = 0.4;
                Some(r)
            })
            .unwrap();
        advanced();
        store.tombstone(id).unwrap();
        advanced();
        let terminal = store.control_state().unwrap();
        assert!(
            store
                .update_access(id, 456, AccessKind::DirectRetrieval)
                .is_err()
        );
        assert!(
            store
                .update_decay_state(id, DecayPhase::Full, 1.0, 1.0, 1.0, false)
                .is_err()
        );
        assert_eq!(store.control_state().unwrap(), terminal);
        store.delete(id).unwrap();
        advanced();
        let id = insert(&store, ns.get(), 0, &[]);
        advanced();
        store.batch_tombstone(&[id]).unwrap();
        advanced();
        store.purge_namespace_records(ns, 1).unwrap();
        advanced();
        store.delete_namespace(ns).unwrap();
        advanced();
        let before = store.control_state().unwrap();
        store.get(id).unwrap();
        store.list_namespaces().unwrap();
        store.scan_all().unwrap();
        store.persist_phase_index().unwrap();
        store.rebuild_phase_index().unwrap();
        store.batch_update_records(&[]).unwrap();
        assert_eq!(store.control_state().unwrap(), before);
    }

    #[test]
    fn control_adopts_legacy_records_without_reencoding_and_fails_closed_on_unknown_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.db");
        let id = MemoryId::new();
        let mut record = record_in(1, 0, &[]);
        record.id = *id.as_bytes();
        let bytes = record.to_bytes();
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            tx.open_table(META_TABLE)
                .unwrap()
                .insert(id.as_bytes().as_slice(), bytes.as_slice())
                .unwrap();
            tx.commit().unwrap();
        }
        let store = MetadataStore::open(&path).unwrap();
        assert_eq!(store.get(id).unwrap().unwrap().to_bytes(), bytes);
        let original = store.control_state().unwrap();
        assert_eq!(original.committed_generation.get(), 0);
        drop(store);
        let store = MetadataStore::open(&path).unwrap();
        assert_eq!(store.control_state().unwrap(), original);
        let mut future = original;
        future.control_schema_version = 2;
        let tx = store.db.begin_write().unwrap();
        tx.open_table(control::CONTROL)
            .unwrap()
            .insert("state", control::encode(&future).unwrap().as_slice())
            .unwrap();
        tx.commit().unwrap();
        drop(store);
        assert!(matches!(
            MetadataStore::open(&path),
            Err(StorageError::InvalidControl(_))
        ));
    }

    #[test]
    fn control_missing_row_or_receipt_table_never_mints_a_new_lineage() {
        for remove_row in [true, false] {
            let (dir, store) = open_store();
            let tx = store.db.begin_write().unwrap();
            if remove_row {
                tx.open_table(control::CONTROL)
                    .unwrap()
                    .remove("state")
                    .unwrap();
            } else {
                tx.delete_table(control::RECEIPTS).unwrap();
            }
            tx.commit().unwrap();
            drop(store);
            assert!(matches!(
                MetadataStore::open(&dir.path().join("meta.db")),
                Err(StorageError::InvalidControl(_))
            ));
        }
    }

    // ── the non-force emptiness guard ────────────────────────────────

    /// The check and the effect have to read the same source of truth.
    /// `count_memories_in_namespace` walks NAMESPACE_INDEX; the purge
    /// scans META_TABLE. A record whose index entry went missing — the
    /// corruption `list_memories_filtered` explicitly tolerates — was
    /// therefore invisible to the "is this namespace empty?" guard and
    /// destroyed anyway by the purge that guard was protecting it from.
    #[test]
    fn w54_the_emptiness_guard_sees_a_record_the_namespace_index_lost() {
        let (_dir, store) = open_store();
        let orphan = insert(&store, 1, 0, &[]);

        // Drop the index entry, keeping the META_TABLE row.
        {
            let write_txn = store.db.begin_write().expect("begin write");
            {
                let mut ns_idx = write_txn
                    .open_multimap_table(NAMESPACE_INDEX)
                    .expect("open namespace index");
                ns_idx
                    .remove(1, orphan.as_bytes().as_slice())
                    .expect("remove index entry");
            }
            write_txn.commit().expect("commit");
        }

        let ns = NamespaceId::new(1);
        assert_eq!(
            store
                .count_memories_in_namespace(ns)
                .expect("index-driven count"),
            0,
            "the setup did not actually orphan the record"
        );
        assert_eq!(
            store
                .count_live_records_in_namespace(ns)
                .expect("scan-driven count"),
            1,
            "the guard cannot see a record the purge will destroy"
        );
        assert_eq!(
            store.purge_namespace_records(ns, 100).expect("purge").len(),
            1,
            "the purge and the guard disagree about what is in the namespace"
        );
    }

    /// The rule the guard deliberately keeps: a namespace holding only
    /// tombstones is empty, because they report as zero everywhere else
    /// and hold no content the user has not already forgotten.
    #[test]
    fn w55_tombstones_do_not_make_a_namespace_non_empty() {
        let (_dir, store) = open_store();
        let dead = insert(&store, 1, 0, &[]);
        store.tombstone(dead).expect("tombstone");

        assert_eq!(
            store
                .count_live_records_in_namespace(NamespaceId::new(1))
                .expect("count"),
            0
        );
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

    // ── update_decay_state ───────────────────────────────────────────

    /// Tombstone is terminal, and this is the write that has to enforce
    /// it. The decay sweep and `reinforce_memory` both read a record,
    /// decide, and write back later with the phase they read. A
    /// `forget_memory` landing in between used to have its tombstone
    /// overwritten: the record came back in a live phase with no
    /// summary, no tags, no text and no `NAMESPACE_INDEX` entry, still
    /// pointing at a vector slot it had already given up.
    #[test]
    fn update_decay_state_does_not_bring_a_tombstone_back() {
        let (_dir, store) = open_store();
        let id = insert(&store, 1, 7, &["type/fact"]);
        store
            .update_decay_state(id, DecayPhase::Summary, 0.9, 0.9, 3.7, false)
            .expect("decay a live record");
        store.tombstone(id).expect("tombstone");

        let stale = store.update_decay_state(id, DecayPhase::Summary, 0.8, 0.8, 3.7, false);

        assert!(
            matches!(stale, Err(StorageError::NotFound(_))),
            "a write against a tombstone must be refused, got {stale:?}"
        );
        let record = store.get(id).expect("get").expect("the row is kept");
        assert_eq!(record.phase, DecayPhase::Tombstone);
        assert_eq!(record.strength, 0.0);
        assert_eq!(record.decay_strength, 0.0);
        assert!(
            store
                .ids_in_phase(DecayPhase::Summary)
                .expect("ids in phase")
                .is_empty(),
            "the phase bitmap claims a tombstone's freed vector slot is live"
        );
    }
}
