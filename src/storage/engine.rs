//! Unified StorageEngine trait and concrete RedbStorageEngine.
//!
//! The `StorageEngine` trait defines the public storage API that
//! higher layers (cache, API server, decay engine) depend on.
//! `RedbStorageEngine` is the concrete implementation that composes
//! all four storage backends (vectors, metadata, text, edges).
//!
//! See Spec 04 Section 7 for the design rationale.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::model::{
    AccessKind, DecayPhase, DiskRecord, EdgeType, MemoryId, NamespaceConfig, NamespaceId,
};
use crate::storage::edges::{EdgeStore, PersistedEdge, cleanup_orphaned_edges};
use crate::storage::error::StorageError;
use crate::storage::fsync::{fsync_dir, fsync_file};
use crate::storage::metadata::MetadataStore;
use crate::storage::text::{CompactionResult, TextRef, TextStore, recover_text_compaction};
use crate::storage::vectors::VectorManager;

// ═══════════════════════════════════════════════════════════════════════
// StorageEngine Trait
// ═══════════════════════════════════════════════════════════════════════

/// Public storage API.
///
/// Implementations manage persistence of memory records, embeddings,
/// text, decay state, namespaces, and graph edges. Higher layers depend
/// on this abstraction rather than concrete backends.
///
/// This trait supersedes the `StorageOps` and `StorageBackend` traits
/// referenced in earlier specs (Spec 02, Spec 03).
pub trait StorageEngine: Send + Sync {
    // ── Memory CRUD ─────────────────────────────────────────────────

    /// Persist a new memory record with its embedding and optional text.
    ///
    /// The caller provides the raw DiskRecord (with vector_slot and
    /// text_offset/text_length already zeroed), the embedding vector,
    /// and the optional full_text. This method:
    ///
    /// 1. Allocates a vector slot and writes the embedding.
    /// 2. Appends full_text to fulltext.dat (if provided).
    /// 3. Sets vector_slot and text pointer on the record.
    /// 4. Inserts the record into meta.db with secondary indexes.
    fn insert_memory(
        &mut self,
        id: MemoryId,
        namespace_id: NamespaceId,
        record: &mut DiskRecord,
        embedding: &[f32],
        full_text: Option<&str>,
    ) -> Result<(), StorageError>;

    /// Retrieve a memory's DiskRecord by ID.
    ///
    /// Returns `None` if no record exists for this ID.
    fn get_record(&self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError>;

    /// Read the embedding vector for a memory in the given namespace.
    ///
    /// Returns the vector as owned `Vec<f32>`, or `None` if the slot
    /// is out of bounds.
    fn get_vector(
        &self,
        namespace_id: NamespaceId,
        slot: u32,
    ) -> Result<Option<Vec<f32>>, StorageError>;

    /// Read the full text for a memory from fulltext.dat.
    ///
    /// Returns `None` if the text ref points to no data.
    fn get_text(&self, text_ref: TextRef) -> Result<Option<String>, StorageError>;

    /// Delete a memory and all associated data (meta, vector slot,
    /// edges). Text.log space is reclaimed lazily via compaction.
    ///
    /// Returns the deleted `DiskRecord` if it existed, or `None`.
    fn delete_memory(&mut self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError>;

    /// Tombstone a memory: strip content (summary, tags, full_text
    /// pointer) and set phase to Tombstone, but preserve the record
    /// in meta.db so the graph node and edges remain intact.
    ///
    /// The caller is responsible for removing the memory from vector,
    /// FTS, and entity indexes separately.
    ///
    /// Returns `Ok(())` if the memory existed, or `Err(NotFound)` if not.
    fn tombstone_memory(&self, id: MemoryId) -> Result<(), StorageError>;

    // ── Decay State ─────────────────────────────────────────────────

    /// Update the decay state of a single memory.
    ///
    /// `strength` is the raw FSRS retrievability R(t,S).
    /// `decay_strength` is the effective retrievability including
    /// connection bonus.
    fn update_decay_state(
        &self,
        id: MemoryId,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
        stability: f32,
        is_permastore: bool,
    ) -> Result<(), StorageError>;

    /// Zero the full_text pointer on a memory record.
    ///
    /// Called during Full -> Summary phase transitions to mark the
    /// text data in fulltext.dat as dead space reclaimable by
    /// compaction.
    fn strip_full_text(&self, id: MemoryId) -> Result<(), StorageError>;

    /// Clear the summary field on a memory record.
    ///
    /// Called during Summary -> Ghost phase transitions.
    fn strip_summary(&self, id: MemoryId) -> Result<(), StorageError>;

    /// Return the IDs of all memories currently in the given phase.
    fn ids_in_phase(&self, phase: DecayPhase) -> Result<Vec<MemoryId>, StorageError>;

    /// Return all (MemoryId, DiskRecord) pairs in a given phase.
    fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError>;

    /// Execute a decay sweep using the provided transition function.
    ///
    /// Returns the number of records transitioned.
    fn decay_sweep(
        &self,
        compute_new_state: &dyn Fn(&DiskRecord) -> Option<DiskRecord>,
    ) -> Result<usize, StorageError>;

    // ── Access Tracking ─────────────────────────────────────────────

    /// Record a new access event on a memory.
    fn update_access(
        &self,
        id: MemoryId,
        timestamp: i64,
        kind: AccessKind,
    ) -> Result<(), StorageError>;

    // ── Edges ───────────────────────────────────────────────────────

    /// Add a single edge.
    fn add_edge(
        &self,
        source: MemoryId,
        target: MemoryId,
        edge_type: EdgeType,
        weight: f32,
        auto_created: bool,
        created_at: u64,
    ) -> Result<(), StorageError>;

    /// Add multiple edges atomically.
    fn batch_add_edges(&self, edges: &[PersistedEdge]) -> Result<(), StorageError>;

    /// Get all outgoing edges from a source (target, edge_type).
    fn get_outgoing_edges(
        &self,
        source: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError>;

    /// Get all incoming edges to a target (source, edge_type).
    fn get_incoming_edges(
        &self,
        target: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError>;

    /// Remove all edges involving a memory (both as source and target).
    fn remove_all_edges(&self, memory_id: MemoryId) -> Result<usize, StorageError>;

    /// Load every edge from edges.db (startup graph loading).
    fn load_all_edges(&self) -> Result<Vec<PersistedEdge>, StorageError>;

    // ── Namespaces ──────────────────────────────────────────────────

    /// Create a new namespace. Returns the assigned NamespaceId.
    fn create_namespace(&mut self, config: &NamespaceConfig) -> Result<NamespaceId, StorageError>;

    /// List all namespace configurations.
    fn list_namespaces(&self) -> Result<Vec<NamespaceConfig>, StorageError>;

    /// Get a namespace configuration by ID.
    fn get_namespace(&self, id: NamespaceId) -> Result<Option<NamespaceConfig>, StorageError>;

    /// Get a namespace by name.
    fn get_namespace_by_name(&self, name: &str) -> Result<Option<NamespaceConfig>, StorageError>;

    // ── Text Compaction ─────────────────────────────────────────────

    /// Run fulltext.dat compaction, reclaiming dead space from decayed
    /// memories. Returns compaction statistics.
    fn compact_text_log(&mut self) -> Result<CompactionResult, StorageError>;

    // ── Tags ────────────────────────────────────────────────────────

    /// Find all memory IDs carrying a given tag.
    fn memories_with_tag(&self, tag: &str) -> Result<Vec<MemoryId>, StorageError>;

    /// List all distinct tags with their memory counts.
    fn list_tags(&self) -> Result<Vec<(String, u64)>, StorageError>;

    // ── Edge Count ──────────────────────────────────────────────────

    /// Update the edge_count field on a memory's DiskRecord.
    ///
    /// Performs a read-modify-write in a single redb transaction.
    fn update_edge_count(&self, id: MemoryId, edge_count: u16) -> Result<(), StorageError>;

    // ── Bulk / Diagnostic ───────────────────────────────────────────

    /// Iterate all records in creation order.
    fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError>;

    /// Count the total number of memory records (including tombstones).
    fn count(&self) -> Result<u64, StorageError>;

    /// Count only active (non-tombstoned) memory records.
    fn count_active(&self) -> Result<u64, StorageError>;

    /// Check if a memory ID exists without deserializing.
    fn exists(&self, id: MemoryId) -> Result<bool, StorageError>;

    /// Free a vector slot on disk for the given namespace.
    ///
    /// Called after tombstoning a memory to return the on-disk slot
    /// to the free list so it can be reused.
    fn free_vector_slot(
        &mut self,
        namespace_id: NamespaceId,
        slot: u32,
    ) -> Result<(), StorageError>;

    /// Flush pending writes to durable storage.
    fn sync(&mut self) -> Result<(), StorageError>;

    /// Persist the in-memory phase index to disk.
    fn persist_phase_index(&self) -> Result<(), StorageError>;
}

// ═══════════════════════════════════════════════════════════════════════
// RedbStorageEngine
// ═══════════════════════════════════════════════════════════════════════

/// Concrete redb-backed storage engine.
///
/// Composes all four storage backends:
/// - `MetadataStore` (redb) for memory records and secondary indexes
/// - `VectorManager` for per-namespace mmap'd vector files
/// - `TextStore` for the append-only text log
/// - `EdgeStore` (redb) for graph edge persistence
///
/// Thread safety: the struct itself is NOT internally synchronized.
/// The caller must wrap it in `Arc<RwLock<RedbStorageEngine>>` or
/// equivalent. Read methods take `&self`; write methods take `&mut self`.
pub struct RedbStorageEngine {
    /// Database directory path (e.g., `recalld.db/`).
    db_path: PathBuf,
    /// Metadata B-tree (redb) -- meta.db.
    meta_store: MetadataStore,
    /// Per-namespace vector files (mmap'd).
    vector_manager: VectorManager,
    /// Append-only text log -- fulltext.dat.
    text_store: TextStore,
    /// Graph edge storage (redb) -- edges.db.
    edge_store: EdgeStore,
    /// Exclusive file lock preventing multi-process access.
    _lock_file: std::fs::File,
}

impl RedbStorageEngine {
    /// Open or create a storage engine at the given directory path.
    ///
    /// Performs the full initialization sequence:
    /// 1. Acquires exclusive file lock (prevents multi-process access).
    /// 2. Opens meta.db (redb) with 16 MB read cache.
    /// 3. Opens edges.db (redb).
    /// 4. Recovers any interrupted fulltext.dat compaction.
    /// 5. Opens fulltext.dat.
    /// 6. Opens vector files for all existing namespaces.
    /// 7. Runs startup validation (rebuilds phase index, cleans orphans).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let db_path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&db_path)?;

        // Step 1: Acquire exclusive file lock.
        let lock_path = db_path.join("recalld.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;

        use fs2::FileExt;
        lock_file
            .try_lock_exclusive()
            .map_err(|_| StorageError::DatabaseLocked)?;

        // Step 2: Open meta.db.
        let meta_store = MetadataStore::open(&db_path.join("meta.db"))?;

        // Step 3: Open edges.db.
        let edge_store = EdgeStore::open(&db_path.join("edges.db"))?;

        // Step 4: Recover any interrupted fulltext.dat compaction.
        recover_text_compaction(&db_path)?;

        // Step 5: Open fulltext.dat.
        let text_store = TextStore::open(&db_path.join("fulltext.dat"))?;

        // Step 6: Open vector files for each existing namespace.
        let mut vector_manager = VectorManager::new(db_path.clone());
        let namespaces = meta_store.list_namespaces()?;
        for ns in &namespaces {
            vector_manager.open_or_create(ns.id, &ns.name, ns.embedding_dim as usize)?;
        }

        let mut engine = Self {
            db_path,
            meta_store,
            vector_manager,
            text_store,
            edge_store,
            _lock_file: lock_file,
        };

        // Step 7: Startup validation.
        engine.startup_validation()?;

        Ok(engine)
    }

    /// Run startup validation checks:
    /// 1. Rebuild phase index from meta.db.
    /// 2. Validate fulltext.dat header.
    /// 3. Clean up orphaned edges.
    fn startup_validation(&mut self) -> Result<(), StorageError> {
        tracing::info!("Running storage startup validation...");

        // 1. Rebuild the phase index from meta.db ground truth.
        self.meta_store.rebuild_phase_index()?;

        // 2. Validate fulltext.dat header.
        self.text_store.validate_header()?;

        // 3. Clean up orphaned edges.
        let all_records = self.meta_store.scan_all()?;
        let known_ids: HashSet<MemoryId> = all_records.iter().map(|(id, _)| *id).collect();
        let orphans_removed = cleanup_orphaned_edges(&self.edge_store, &known_ids)?;
        if orphans_removed > 0 {
            tracing::warn!(
                removed = orphans_removed,
                "Cleaned up orphaned edges during startup"
            );
        }

        tracing::info!("Storage startup validation complete");
        Ok(())
    }

    /// Return a reference to the underlying MetadataStore.
    /// Useful for callers that need direct access (e.g., batch
    /// operations not covered by the trait).
    pub fn meta_store(&self) -> &MetadataStore {
        &self.meta_store
    }

    /// Return a reference to the underlying EdgeStore.
    pub fn edge_store(&self) -> &EdgeStore {
        &self.edge_store
    }

    /// Return a reference to the underlying VectorManager.
    pub fn vector_manager(&self) -> &VectorManager {
        &self.vector_manager
    }

    /// Return a mutable reference to the underlying VectorManager.
    pub fn vector_manager_mut(&mut self) -> &mut VectorManager {
        &mut self.vector_manager
    }

    /// Return the database directory path.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

// ═══════════════════════════════════════════════════════════════════════
// StorageEngine Implementation
// ═══════════════════════════════════════════════════════════════════════

impl StorageEngine for RedbStorageEngine {
    // ── Memory CRUD ─────────────────────────────────────────────────

    fn insert_memory(
        &mut self,
        id: MemoryId,
        namespace_id: NamespaceId,
        record: &mut DiskRecord,
        embedding: &[f32],
        full_text: Option<&str>,
    ) -> Result<(), StorageError> {
        // 1. Allocate vector slot and write embedding.
        let vector_store = self
            .vector_manager
            .get_mut(namespace_id)
            .ok_or(StorageError::NamespaceNotFound(namespace_id.get()))?;
        let vector_slot = vector_store.insert_vector(embedding)?;
        record.vector_slot = vector_slot;

        // 2. Append full_text to fulltext.dat if present.
        // Note: fulltext.dat is append-only; if this succeeds but step 3
        // fails, the entry becomes orphaned dead space that compaction
        // will reclaim. No explicit rollback needed for text.
        if let Some(text) = full_text {
            let text_ref = self.text_store.append(text)?;
            record.text_offset = text_ref.file_offset;
            record.text_length = text_ref.length;
        }

        // 2b. Fsync vector and text data before the meta.db commit.
        // meta.db (redb) fsyncs internally on commit, so without this
        // step a crash could leave meta.db pointing to unflushed vector
        // or text data. Vectors have no integrity check, so stale/zero
        // data would be silently used for similarity search. Text has
        // CRC protection but would still fail reads until rewritten.
        if let Some(vs) = self.vector_manager.get(namespace_id) {
            vs.sync()?;
        }
        if full_text.is_some() {
            self.text_store.sync_data()?;
        }

        // 3. Insert the completed record into meta.db.
        if let Err(e) = self.meta_store.insert(id, record) {
            // Best-effort rollback: free the vector slot so it can be
            // reused. The fulltext.dat entry (if written) becomes orphaned
            // dead space that compaction will eventually reclaim.
            if let Some(vs) = self.vector_manager.get_mut(namespace_id) {
                if let Err(rollback_err) = vs.free_slot(vector_slot) {
                    tracing::warn!(
                        memory_id = %id,
                        vector_slot,
                        error = %rollback_err,
                        "Failed to roll back vector slot after meta.db insert failure; \
                         slot is leaked until next compaction or restart"
                    );
                }
            }
            if full_text.is_some() {
                tracing::warn!(
                    memory_id = %id,
                    text_offset = record.text_offset,
                    text_length = record.text_length,
                    "Orphaned fulltext.dat entry after meta.db insert failure; \
                     dead space will be reclaimed by next compaction"
                );
            }
            return Err(e);
        }

        Ok(())
    }

    fn get_record(&self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError> {
        self.meta_store.get(id)
    }

    fn get_vector(
        &self,
        namespace_id: NamespaceId,
        slot: u32,
    ) -> Result<Option<Vec<f32>>, StorageError> {
        let vector_store = self
            .vector_manager
            .get(namespace_id)
            .ok_or(StorageError::NamespaceNotFound(namespace_id.get()))?;
        Ok(vector_store.get_vector(slot).map(|v| v.to_vec()))
    }

    fn get_text(&self, text_ref: TextRef) -> Result<Option<String>, StorageError> {
        if !text_ref.is_some() {
            return Ok(None);
        }
        self.text_store.read(text_ref).map(Some)
    }

    fn delete_memory(&mut self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError> {
        // 1. Delete from meta.db (also removes secondary indexes).
        let record = match self.meta_store.delete(id)? {
            Some(r) => r,
            None => return Ok(None),
        };

        // 2. Free the vector slot.
        let namespace_id = NamespaceId::new(record.namespace_id);
        if let Some(vector_store) = self.vector_manager.get_mut(namespace_id) {
            // Ignore errors from freeing slots -- the slot may
            // reference a namespace whose vector file was already
            // removed (e.g., during namespace deletion).
            let _ = vector_store.free_slot(record.vector_slot);
        }

        // 3. Remove all edges involving this memory.
        self.edge_store.remove_all_edges(id)?;

        // 4. Text.log space is reclaimed lazily via compaction.

        Ok(Some(record))
    }

    fn tombstone_memory(&self, id: MemoryId) -> Result<(), StorageError> {
        self.meta_store.tombstone(id)
    }

    // ── Decay State ─────────────────────────────────────────────────

    fn update_decay_state(
        &self,
        id: MemoryId,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
        stability: f32,
        is_permastore: bool,
    ) -> Result<(), StorageError> {
        self.meta_store
            .update_decay_state(id, phase, strength, decay_strength, stability, is_permastore)
    }

    fn strip_full_text(&self, id: MemoryId) -> Result<(), StorageError> {
        self.meta_store.strip_full_text(id)
    }

    fn strip_summary(&self, id: MemoryId) -> Result<(), StorageError> {
        self.meta_store.strip_summary(id)
    }

    fn ids_in_phase(&self, phase: DecayPhase) -> Result<Vec<MemoryId>, StorageError> {
        self.meta_store.ids_in_phase(phase)
    }

    fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        self.meta_store.scan_phase_records(phase)
    }

    fn decay_sweep(
        &self,
        compute_new_state: &dyn Fn(&DiskRecord) -> Option<DiskRecord>,
    ) -> Result<usize, StorageError> {
        self.meta_store.decay_sweep(compute_new_state)
    }

    // ── Access Tracking ─────────────────────────────────────────────

    fn update_access(
        &self,
        id: MemoryId,
        timestamp: i64,
        kind: AccessKind,
    ) -> Result<(), StorageError> {
        self.meta_store.update_access(id, timestamp, kind)
    }

    // ── Edges ───────────────────────────────────────────────────────

    fn add_edge(
        &self,
        source: MemoryId,
        target: MemoryId,
        edge_type: EdgeType,
        weight: f32,
        auto_created: bool,
        created_at: u64,
    ) -> Result<(), StorageError> {
        self.edge_store
            .add_edge(source, target, edge_type, weight, auto_created, created_at)
    }

    fn batch_add_edges(&self, edges: &[PersistedEdge]) -> Result<(), StorageError> {
        self.edge_store.batch_add_edges(edges)
    }

    fn get_outgoing_edges(
        &self,
        source: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError> {
        self.edge_store.get_outgoing(source)
    }

    fn get_incoming_edges(
        &self,
        target: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError> {
        self.edge_store.get_incoming(target)
    }

    fn remove_all_edges(&self, memory_id: MemoryId) -> Result<usize, StorageError> {
        self.edge_store.remove_all_edges(memory_id)
    }

    fn load_all_edges(&self) -> Result<Vec<PersistedEdge>, StorageError> {
        self.edge_store.load_all_edges()
    }

    // ── Namespaces ──────────────────────────────────────────────────

    fn create_namespace(&mut self, config: &NamespaceConfig) -> Result<NamespaceId, StorageError> {
        let ns_id = self.meta_store.create_namespace(config)?;

        // Open/create the vector file for this namespace.
        self.vector_manager
            .open_or_create(ns_id, &config.name, config.embedding_dim as usize)?;

        Ok(ns_id)
    }

    fn list_namespaces(&self) -> Result<Vec<NamespaceConfig>, StorageError> {
        self.meta_store.list_namespaces()
    }

    fn get_namespace(&self, id: NamespaceId) -> Result<Option<NamespaceConfig>, StorageError> {
        self.meta_store.get_namespace(id)
    }

    fn get_namespace_by_name(&self, name: &str) -> Result<Option<NamespaceConfig>, StorageError> {
        self.meta_store.get_namespace_by_name(name)
    }

    // ── Text Compaction ─────────────────────────────────────────────

    fn compact_text_log(&mut self) -> Result<CompactionResult, StorageError> {
        // Collect all live text references from meta.db.
        let all_records = self.meta_store.scan_all()?;
        let mut live_refs: Vec<(MemoryId, TextRef)> = Vec::new();

        for (id, record) in &all_records {
            if record.text_offset > 0 && record.text_length > 0 {
                live_refs.push((
                    *id,
                    TextRef {
                        file_offset: record.text_offset,
                        length: record.text_length,
                    },
                ));
            }
        }

        // Phase 1: Write new file with only live entries.
        // This creates fulltext.dat.new and the .compacting marker,
        // but does NOT rename or remove the marker.
        let (result, compacted) = self.text_store.compact_write_new_file(&live_refs)?;

        if compacted {
            // Phase 2: Update meta.db with new text offsets BEFORE
            // renaming the file. This ensures that if we crash after
            // the meta.db commit but before the rename, recovery can
            // detect the .compaction-meta-committed marker and complete
            // the rename. Without this ordering, a crash after the
            // rename but before the meta.db update would leave stale
            // offsets pointing into a compacted file, corrupting all
            // text reads.
            let updates: Vec<(MemoryId, DiskRecord)> = result
                .new_refs
                .iter()
                .map(|(id, new_ref)| {
                    // Find the original record and update its text pointer.
                    let original = all_records
                        .iter()
                        .find(|(rid, _)| rid == id)
                        .map(|(_, r)| r)
                        .expect("compaction ref must match a record");
                    let mut updated = original.clone();
                    updated.text_offset = new_ref.file_offset;
                    updated.text_length = new_ref.length;
                    (*id, updated)
                })
                .collect();

            if !updates.is_empty() {
                self.meta_store.batch_update_records(&updates)?;
            }

            // Write a committed marker so recovery knows meta.db has
            // already been updated with new offsets. If we crash after
            // this but before the rename, recovery will complete the
            // rename instead of rolling back.
            let committed_marker = self.db_path.join(".compaction-meta-committed");
            std::fs::write(&committed_marker, b"meta.db offsets committed\n")?;
            fsync_file(&committed_marker)?;
            fsync_dir(&self.db_path)?;

            // Phase 3: Atomically swap the files and clean up markers.
            self.text_store.compact_finalize()?;

            tracing::info!(
                old_size = result.old_size,
                new_size = result.new_size,
                removed = result.entries_removed,
                kept = result.entries_kept,
                "Text.log compaction complete"
            );
        }

        Ok(result)
    }

    // ── Tags ────────────────────────────────────────────────────────

    fn memories_with_tag(&self, tag: &str) -> Result<Vec<MemoryId>, StorageError> {
        self.meta_store.memories_with_tag(tag)
    }

    fn list_tags(&self) -> Result<Vec<(String, u64)>, StorageError> {
        self.meta_store.list_tags()
    }

    // ── Edge Count ──────────────────────────────────────────────────

    fn update_edge_count(&self, id: MemoryId, edge_count: u16) -> Result<(), StorageError> {
        self.meta_store.update_edge_count(id, edge_count)
    }

    // ── Bulk / Diagnostic ───────────────────────────────────────────

    fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        self.meta_store.scan_all()
    }

    fn count(&self) -> Result<u64, StorageError> {
        self.meta_store.count()
    }

    fn count_active(&self) -> Result<u64, StorageError> {
        self.meta_store.count_active()
    }

    fn exists(&self, id: MemoryId) -> Result<bool, StorageError> {
        self.meta_store.exists(id)
    }

    fn free_vector_slot(
        &mut self,
        namespace_id: NamespaceId,
        slot: u32,
    ) -> Result<(), StorageError> {
        if let Some(vector_store) = self.vector_manager.get_mut(namespace_id) {
            vector_store.free_slot(slot)?;
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        // Sync fulltext.dat.
        self.text_store.sync_data()?;

        // Sync all open vector files.
        for (_, store) in self.vector_manager.iter() {
            store.sync()?;
        }

        Ok(())
    }

    fn persist_phase_index(&self) -> Result<(), StorageError> {
        self.meta_store.persist_phase_index()
    }
}
