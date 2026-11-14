//! Unified StorageEngine trait and concrete RedbStorageEngine.
//!
//! The `StorageEngine` trait defines the public storage API that
//! higher layers (cache, API server, decay engine) depend on.
//! `RedbStorageEngine` is the concrete implementation that composes
//! all four storage backends (vectors, metadata, text, edges).
//!
//! See Spec 04 Section 7 for the design rationale.

mod effects;
mod insertion;
mod text_compaction;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::model::validation::validate_namespace_name;
use crate::model::{
    AccessKind, DecayPhase, DiskRecord, EdgeType, MemoryId, NamespaceConfig, NamespaceId,
};
use crate::storage::control::{ControlState, MutationContext, MutationScope, WriteReceipt};
use crate::storage::edges::{EdgeStore, PersistedEdge, cleanup_orphaned_edges};
use crate::storage::error::StorageError;
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
    ///
    /// Returns `Err(NotFound)` if the memory does not exist or has been
    /// tombstoned; a tombstone is never rewritten.
    fn update_decay_state(
        &self,
        id: MemoryId,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
        stability: f32,
        is_permastore: bool,
    ) -> Result<(), StorageError>;

    /// Atomic v1 feedback state/history patch; false rejects a stale snapshot.
    fn apply_legacy_feedback(
        &self,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        plan: &crate::decay::lifecycle::FeedbackPlan,
    ) -> Result<bool, StorageError>;

    /// Atomic v1 strength/phase/content patch (same phase or one phase loss).
    fn apply_legacy_decay(
        &self,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
    ) -> Result<bool, StorageError>;

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
    _lock_file: crate::storage::ownership::OwnedFileLock,
    _ownership: crate::storage::ownership::StoreOwnership,
}

impl RedbStorageEngine {
    /// Fail closed after an uncertain commit or failed derived publication.
    /// Lock order for graph writers: graph -> storage -> metadata -> redb.
    pub(crate) fn ensure_available(&self) -> Result<(), StorageError> {
        self.meta_store.ensure_available()
    }

    pub(crate) fn mark_unavailable(&self) {
        self.meta_store.mark_unavailable();
    }

    /// Metadata lineage/generation foundation; not a whole-store read snapshot.
    pub fn control_state(&self) -> Result<ControlState, StorageError> {
        self.meta_store.control_state()
    }

    /// Commit an access event and its receipt in one metadata transaction.
    /// This acknowledges canonical commit only. Derived readiness is untracked;
    /// adapters must not advertise v2 ready-write semantics from this receipt.
    pub fn update_access_with_receipt(
        &self,
        ctx: &MutationContext,
        id: MemoryId,
        timestamp: i64,
        kind: AccessKind,
    ) -> Result<WriteReceipt, StorageError> {
        self.meta_store
            .update_access_with_receipt(ctx, id, timestamp, kind)
    }

    pub fn lookup_operation(
        &self,
        store_id: uuid::Uuid,
        scope: MutationScope,
        operation_id: uuid::Uuid,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        self.meta_store
            .lookup_operation(store_id, scope, operation_id)
    }

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
        let ownership = crate::storage::ownership::StoreOwnership::acquire(path.as_ref())?;
        let db_path = ownership.path.clone();
        std::fs::create_dir_all(&db_path)?;
        let lock_file = crate::storage::ownership::lock_directory(&db_path)?;

        // Existing stores must not silently initialize absent canonical files.
        recover_text_compaction(&db_path)?;
        let existing = std::fs::read_dir(&db_path)?.any(|entry| {
            entry
                .map(|e| e.file_name() != "recalld.lock")
                .unwrap_or(true)
        });
        if existing {
            for name in ["meta.db", "edges.db", "fulltext.dat"] {
                crate::storage::validation::require_file(&db_path.join(name))?;
            }
            crate::storage::validation::preflight_text(&db_path)?;
        }

        // Step 2: Open meta.db.
        let meta_store = MetadataStore::open(&db_path.join("meta.db"))?;

        // Step 3: Open edges.db.
        let edge_store = EdgeStore::open(&db_path.join("edges.db"))?;

        // Legacy compaction was checked before opening any canonical file.
        // Step 5: Open fulltext.dat.
        if !existing {
            let state = meta_store.control_state()?;
            let (prepared, id) = TextStore::create_generation(&db_path, state.store_id)?;
            let descriptor = prepared.seal(state.store_id, id, 2)?;
            crate::storage::text::generation::write_rejection_gate(&db_path, &descriptor)?;
            meta_store.select_text_generation(state.committed_generation, &descriptor, &vec![])?;
        }
        let selected_text = meta_store.text_selection_for_recovery()?;
        let text_store = match &selected_text {
            Some(selected) => TextStore::open_generation(&db_path, selected, true)?,
            None => TextStore::open(&db_path.join("fulltext.dat"))?,
        };
        // Validate canonical text before effects replay or any cleanup.
        for (_, record) in meta_store.scan_all_for_recovery()? {
            if record.text_offset != 0 || record.text_length != 0 {
                text_store.read(TextRef {
                    file_offset: record.text_offset,
                    length: record.text_length,
                })?;
            }
        }

        // Step 6: Open vector files for each existing namespace.
        let mut vector_manager = VectorManager::new(db_path.clone());
        let namespaces = meta_store.list_namespaces_for_recovery()?;
        for ns in &namespaces {
            validate_namespace_name(&ns.name).map_err(|reason| {
                StorageError::InvalidNamespaceName {
                    name: ns.name.clone(),
                    reason: reason.to_string(),
                }
            })?;
            if !std::fs::symlink_metadata(db_path.join(&ns.name))?.is_dir() {
                return Err(StorageError::InvalidPath);
            }
            crate::storage::validation::require_file(&db_path.join(&ns.name).join("vectors.dat"))?;
            vector_manager.open_or_create(ns.id, &ns.name, ns.embedding_dim as usize)?;
        }

        let mut engine = Self {
            db_path,
            meta_store,
            vector_manager,
            text_store,
            edge_store,
            _lock_file: lock_file,
            _ownership: ownership,
        };

        // Step 7: Startup validation.
        engine.recover_pending_effects()?;
        engine.startup_validation()?;
        if let Some(selected) = selected_text {
            if let Err(error) =
                crate::storage::text::generation::cleanup_orphans(&engine.db_path, &selected)
            {
                tracing::warn!(%error, "text orphan cleanup pending");
            }
        }

        Ok(engine)
    }

    /// Run startup validation checks:
    /// 1. Rebuild phase index from meta.db.
    /// 2. Validate fulltext.dat header.
    /// 3. Clean up orphaned edges.
    /// 4. Sweep namespace directories left behind by an interrupted
    ///    namespace deletion into quarantine.
    fn startup_validation(&mut self) -> Result<(), StorageError> {
        tracing::info!("Running storage startup validation...");

        // MetadataStore::open already rebuilt the phase index from records.

        // 2. Validate fulltext.dat header.
        self.text_store.validate_header()?;

        // 3. Clean up orphaned edges.
        let all_records = self.meta_store.scan_all()?;
        let mut owners = HashSet::new();
        for (id, record) in &all_records {
            if record.id != *id.as_bytes() {
                return Err(StorageError::InvalidMutation(
                    "stored record ID disagrees with key".into(),
                ));
            }
            let vectors = self
                .vector_manager
                .get(NamespaceId::new(record.namespace_id))
                .ok_or(StorageError::NamespaceNotFound(record.namespace_id))?;
            if record.phase != DecayPhase::Tombstone
                && (!owners.insert((record.namespace_id, record.vector_slot))
                    || vectors.is_free(record.vector_slot)
                    || vectors.get_vector(record.vector_slot).is_none())
            {
                return Err(StorageError::OwnershipMismatch(format!(
                    "invalid or duplicate vector owner {id}"
                )));
            }
            if record.text_offset != 0 || record.text_length != 0 {
                self.text_store.read(TextRef {
                    file_offset: record.text_offset,
                    length: record.text_length,
                })?;
            }
        }
        // Reclaim abandoned staging only after every canonical owner and payload
        // has passed validation. Never infer ownership from tombstone slots.
        let namespaces: Vec<_> = self.vector_manager.iter().map(|(id, _)| id).collect();
        for namespace in namespaces {
            let store = self
                .vector_manager
                .get_mut(namespace)
                .expect("enumerated namespace");
            let mut changed = false;
            for slot in 0..store.slot_count() {
                if !owners.contains(&(namespace.get(), slot)) && !store.is_free(slot) {
                    store.free_slot(slot)?;
                    changed = true;
                }
            }
            if changed {
                store.sync()?;
            }
        }
        let known_ids: HashSet<MemoryId> = all_records.iter().map(|(id, _)| *id).collect();
        let orphans_removed = cleanup_orphaned_edges(&self.edge_store, &known_ids)?;
        if orphans_removed > 0 {
            tracing::warn!(
                removed = orphans_removed,
                "Cleaned up orphaned edges during startup"
            );
        }

        // 4. Sweep orphaned namespace directories. Deliberately not
        //    allowed to fail startup: this is disk hygiene, and a
        //    database that refuses to open because a stray directory
        //    could not be inspected is a far worse outcome than the
        //    wasted space.
        match self.sweep_orphan_namespace_dirs() {
            Ok(removed) if removed > 0 => {
                tracing::warn!(
                    quarantined = removed,
                    path = %self.db_path.join(QUARANTINE_DIR_NAME).display(),
                    "Moved orphaned namespace directories left by an interrupted \
                     namespace deletion into quarantine. Nothing was deleted; remove \
                     the quarantine directory by hand once you are satisfied."
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Orphaned-namespace-directory sweep failed; continuing startup"
                );
            }
        }

        tracing::info!("Storage startup validation complete");
        Ok(())
    }

    /// Quarantine namespace directories that belong to no live
    /// namespace.
    ///
    /// Cleans up after a `purge_namespace` that was interrupted between
    /// deleting the `NAMESPACE_TABLE` row and removing the directory.
    /// Returns the number of directories moved.
    ///
    /// Directories are MOVED to `<db_path>/.orphaned/`, not deleted —
    /// see [`quarantine_orphan_dir`](Self::quarantine_orphan_dir) for
    /// why the delete had no lower bound.
    ///
    /// # Why this cannot delete a live namespace
    ///
    /// [`open`](Self::open) requires a real directory and existing vector
    /// file for every namespace before this sweep runs. A missing payload
    /// fails startup; it is never initialized or swept as an orphan.
    ///
    /// # Guards, and why each one is here
    ///
    /// `db_path` is user-configurable and may hold directories that have
    /// nothing to do with recalld, so "the name matches no live
    /// namespace" is not on its own a licence to recurse and delete.
    /// Every candidate must additionally:
    ///
    /// - be a real directory. Types come from `read_dir`, which does
    ///   **not** follow symlinks, so a symlink pointing outside
    ///   `db_path` is reported as a symlink and skipped rather than
    ///   followed into someone else's tree.
    /// - have a UTF-8 name (a name we cannot compare against `live` is
    ///   a name we cannot prove is dead). The comparison itself is ASCII
    ///   case-folded: on a case-insensitive filesystem the live
    ///   namespace `Notes` and the directory `notes/` are the same
    ///   directory, and an exact-string compare would call it dead.
    /// - not start with `.` — dotfile directories are conventionally
    ///   somebody else's state.
    /// - **contain `vectors.dat` and nothing else.** This is the load-
    ///   bearing guard: it is the shape this code writes, so anything
    ///   else in there is evidence the directory was not created by us.
    ///
    /// Anything that fails a guard is logged and left strictly alone.
    fn sweep_orphan_namespace_dirs(&self) -> Result<usize, StorageError> {
        // Case-folded, because the identity that matters here is the
        // one the FILESYSTEM uses and macOS APFS is case-insensitive by
        // default. An exact-string compare let a live namespace named
        // `Notes` fail to claim the directory `notes/` that it is
        // actually reading and writing — and the shape guard passes,
        // because it is genuinely a namespace directory. The sweep then
        // deleted a live namespace's vectors, unattended, at boot.
        //
        // Folding is the conservative direction: it can only ever make
        // this refuse to delete something, never delete more.
        let live: HashSet<String> = self
            .meta_store
            .list_namespaces()?
            .into_iter()
            .map(|ns| ns.name.to_ascii_lowercase())
            .collect();

        let mut removed = 0usize;

        for entry in std::fs::read_dir(&self.db_path)? {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "Could not read a data-directory entry; skipping");
                    continue;
                }
            };

            // read_dir file types do not follow symlinks.
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => {}
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(
                        path = %entry.path().display(),
                        error = %e,
                        "Could not stat a data-directory entry; skipping"
                    );
                    continue;
                }
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                tracing::warn!(
                    path = %entry.path().display(),
                    "Data-directory entry has a non-UTF-8 name; leaving it alone"
                );
                continue;
            };

            if name.starts_with('.') || live.contains(&name.to_ascii_lowercase()) {
                continue;
            }

            let dir = entry.path();
            if !Self::looks_like_a_namespace_dir(&dir) {
                tracing::warn!(
                    path = %dir.display(),
                    "Directory in the data directory matches no namespace but does not \
                     have the shape of one; leaving it alone"
                );
                continue;
            }

            match self.quarantine_orphan_dir(name, &dir) {
                Ok(moved_to) => {
                    tracing::info!(
                        namespace = name,
                        from = %dir.display(),
                        to = %moved_to.display(),
                        "Quarantined orphaned namespace directory"
                    );
                    removed += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %dir.display(),
                        error = %e,
                        "Failed to quarantine orphaned namespace directory"
                    );
                }
            }
        }

        Ok(removed)
    }

    /// Move an orphaned namespace directory into
    /// `<db_path>/.orphaned/<name>-<timestamp>/` instead of deleting it.
    /// Returns where it went.
    ///
    /// # Why a rename and not `remove_dir_all`
    ///
    /// The sweep's whole argument for being safe is "every live
    /// namespace has a row in `NAMESPACE_TABLE`, so a directory with no
    /// row is dead". That argument has no lower bound: if `meta.db` is
    /// absent or restored from an older backup, `live` is EMPTY and
    /// every namespace directory in the data directory is a candidate.
    /// The guards all pass — they are real namespace directories — and
    /// a recursive delete of all of them is unrecoverable.
    ///
    /// A rename keeps the entire hygiene benefit (the directories stop
    /// occupying the data directory and stop being reopened) with none
    /// of the blast radius: whatever the sweep got wrong is still on
    /// disk, named, timestamped, and can be moved back.
    ///
    /// Refusing to sweep when `live` is empty was the alternative. It is
    /// strictly weaker — it does nothing for the case where `live` is
    /// merely STALE rather than empty, which is the same restored-backup
    /// scenario one namespace later — and it would break the legitimate
    /// case of a fresh database with a leftover directory.
    fn quarantine_orphan_dir(&self, name: &str, dir: &Path) -> Result<PathBuf, std::io::Error> {
        let quarantine = self.db_path.join(QUARANTINE_DIR_NAME);
        std::fs::create_dir_all(&quarantine)?;

        // Same filesystem by construction — both are under db_path — so
        // this is a rename, not a copy.
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let mut dest = quarantine.join(format!("{name}-{stamp}"));
        // Two sweeps in the same second must not merge into one
        // directory, and `rename` onto an existing directory is not an
        // error on every platform.
        let mut attempt = 1u32;
        while dest.exists() {
            dest = quarantine.join(format!("{name}-{stamp}-{attempt}"));
            attempt += 1;
        }

        std::fs::rename(dir, &dest)?;
        Ok(dest)
    }

    /// Whether `dir` has exactly the shape this code creates for a
    /// namespace: one entry, named `vectors.dat`.
    ///
    /// Conservative on every uncertainty — an unreadable directory, an
    /// unreadable entry, or a non-UTF-8 name all answer `false`, which
    /// means "leave it alone".
    fn looks_like_a_namespace_dir(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };

        let mut saw_vectors = false;
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            if entry.file_name() != std::ffi::OsStr::new("vectors.dat") {
                return false;
            }
            saw_vectors = true;
        }
        saw_vectors
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
// Namespace purge
// ═══════════════════════════════════════════════════════════════════════

/// What [`RedbStorageEngine::purge_namespace`] destroyed.
#[derive(Debug)]
pub struct PurgedNamespace {
    /// The configuration of the namespace that was removed.
    pub config: NamespaceConfig,
    /// Every record removed, as it was on disk. The caller needs these
    /// to clean the indexes storage cannot reach.
    pub records: Vec<(MemoryId, DiskRecord)>,
    /// How many of `records` were live (not tombstoned).
    pub live_removed: usize,
    /// How many of `records` were already tombstones.
    pub tombstones_removed: usize,
    /// Edge rows deleted from edges.db, counting edges in which a
    /// purged memory was the target as well as the source.
    pub edges_removed: usize,
    /// Surviving memories whose outgoing `edge_count` was corrected
    /// because an edge of theirs pointed into the purged namespace, with
    /// their new count. The caller owes these an invalidation: their
    /// cached record still holds the old number.
    pub peer_edge_counts: Vec<(MemoryId, u16)>,
    /// Whether the namespace's vector directory was removed. `false`
    /// means it was already gone, or a guard refused to touch it.
    pub directory_removed: bool,
}

/// A [`RedbStorageEngine::purge_namespace`] that failed.
///
/// A purge is chunked and multi-step, so "it failed" says nothing about
/// how much survived. `destroyed` is the answer, and it is not
/// diagnostic decoration: those records are permanently gone from
/// `META_TABLE` and still occupy the FTS, vector, entity and graph
/// indexes, which only the caller can reach. Dropping the list strands
/// them there forever — a retry cannot find them, because it looks in
/// `META_TABLE`.
#[derive(Debug)]
pub struct NamespacePurgeError {
    /// Records this purge destroyed before it stopped. Empty when it
    /// failed before touching anything.
    pub destroyed: Vec<(MemoryId, DiskRecord)>,
    /// What stopped it.
    pub source: StorageError,
}

impl NamespacePurgeError {
    /// A failure that happened before any record was committed.
    fn nothing_destroyed(source: StorageError) -> Self {
        Self {
            destroyed: Vec::new(),
            source,
        }
    }

    /// A failure that happened after `destroyed` was already gone.
    fn partial(destroyed: Vec<(MemoryId, DiskRecord)>, source: StorageError) -> Self {
        Self { destroyed, source }
    }

    /// Whether anything was destroyed before the failure. A caller that
    /// reports a bare failure for this case is lying to its user.
    pub fn is_partial(&self) -> bool {
        !self.destroyed.is_empty()
    }
}

impl std::fmt::Display for NamespacePurgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_partial() {
            write!(
                f,
                "the namespace was PARTIALLY destroyed: {} memories are permanently \
                 gone and the namespace still exists. Re-run the deletion to finish \
                 it. Cause: {}",
                self.destroyed.len(),
                self.source
            )
        } else {
            write!(f, "{}", self.source)
        }
    }
}

impl std::error::Error for NamespacePurgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl RedbStorageEngine {
    /// Destroy a namespace: its records, its edges, its configuration
    /// row and its vector file.
    ///
    /// # DESTRUCTIVE AND IRREVERSIBLE
    ///
    /// Every memory in the namespace is gone when this returns. There
    /// is no undo and no backup. Callers are expected to have obtained
    /// explicit confirmation before getting here.
    ///
    /// # The caller must finish the job
    ///
    /// This method can only reach what storage owns. The returned
    /// records are the caller's input for cleaning the FTS index, the
    /// in-memory vector index, the entity index and the relationship
    /// graph — none of which this layer holds a handle to. Skip that and
    /// the namespace is gone from disk but still burning top-k slots in
    /// every search.
    ///
    /// # Ordering, and what a crash costs
    ///
    /// 1. Purge `META_TABLE` rows (chunked).
    /// 2. Purge edges.
    /// 3. Delete the `NAMESPACE_TABLE` row. **Point of no return.**
    /// 4. Drop the `VectorStore`, then remove the directory.
    ///
    /// A crash before (3) leaves a live namespace holding fewer
    /// memories: ugly, but consistent, and re-running the call finishes
    /// it. A crash after (3) leaves an orphan directory and orphan FTS
    /// rows — wasted disk, no wrong answers — and the directory is
    /// reclaimed by the boot sweep in `startup_validation`.
    ///
    /// No crash marker is needed. Namespace ids are never recycled and
    /// `NAMESPACE_TABLE` is authoritative, so "the row is gone" is
    /// itself the durable record that the namespace is dead.
    ///
    /// # A failure is not the same as "nothing happened"
    ///
    /// Step 1 commits in chunks, so an ERROR — not merely a crash — part
    /// of the way through has already destroyed everything up to that
    /// point. The error type carries those records for exactly the
    /// reason the success type does: without them the caller cannot
    /// clean the indexes, and nothing ever can, because a retry finds
    /// their `META_TABLE` rows already gone and reports an empty purge.
    pub fn purge_namespace(
        &mut self,
        namespace_id: NamespaceId,
    ) -> Result<PurgedNamespace, NamespacePurgeError> {
        let config = match self.meta_store.get_namespace(namespace_id) {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Err(NamespacePurgeError::nothing_destroyed(
                    StorageError::NamespaceNotFound(namespace_id.get()),
                ));
            }
            Err(e) => return Err(NamespacePurgeError::nothing_destroyed(e)),
        };

        tracing::info!(
            namespace = %config.name,
            namespace_id = namespace_id.get(),
            "Purging namespace (destructive, irreversible)"
        );

        let victims = self
            .meta_store
            .scan_all()
            .map_err(NamespacePurgeError::nothing_destroyed)?;
        let ids: Vec<_> = victims
            .iter()
            .filter(|(_, r)| r.namespace_id == namespace_id.get())
            .map(|(id, _)| *id)
            .collect();
        let mut records = Vec::new();
        let mut edges_removed = 0;
        let mut peer_counts = std::collections::HashMap::new();
        for chunk in ids.chunks(PURGE_CHUNK_SIZE) {
            let result = (|| {
                let guard = self.meta_store.mutation_guard()?;
                let result = self.meta_store.commit_graph_effects(
                    &guard,
                    namespace_id,
                    &self.edge_store.load_all_edges()?,
                    &[],
                    chunk,
                    true,
                    false,
                )?;
                // Preserve known committed removals even if materialization fails.
                records.extend(result.removed);
                edges_removed += result.edges_removed;
                peer_counts.extend(result.peer_counts);
                effects::apply_pending(
                    &self.meta_store,
                    &self.edge_store,
                    Some(&mut self.vector_manager),
                )
                .map_err(|source| StorageError::CommittedEffectsPending {
                    operation_id: result.receipt.operation_id,
                    source: Box::new(source),
                })
            })();
            if let Err(source) = result {
                return Err(NamespacePurgeError::partial(records, source));
            }
        }
        let peer_edge_counts = peer_counts
            .into_iter()
            .filter(|(id, _)| !ids.contains(id))
            .collect();
        let live_removed = records
            .iter()
            .filter(|(_, r)| r.phase != DecayPhase::Tombstone)
            .count();
        let tombstones_removed = records.len() - live_removed;

        // 3. Configuration row. Point of no return.
        if let Err(e) = self.meta_store.delete_namespace(namespace_id) {
            return Err(NamespacePurgeError::partial(records, e));
        }

        // 4. Vector file. The store must be dropped before the unlink:
        //    it holds an fs2 exclusive lock and a live mmap.
        let store = self.vector_manager.remove(namespace_id);
        drop(store);

        let directory_removed = self.remove_namespace_dir(&config.name);

        tracing::info!(
            namespace = %config.name,
            namespace_id = namespace_id.get(),
            live_removed,
            tombstones_removed,
            edges_removed,
            directory_removed,
            "Namespace purge complete"
        );

        Ok(PurgedNamespace {
            config,
            records,
            live_removed,
            tombstones_removed,
            edges_removed,
            peer_edge_counts,
            directory_removed,
        })
    }

    /// Remove every edge touching `ids`, and repair the outgoing
    /// `edge_count` of every SURVIVING memory that had an edge into one
    /// of them. Returns the edges removed and the `(peer, new_count)`
    /// pairs that were rewritten.
    ///
    /// # Why anything needs repairing
    ///
    /// `DiskRecord.edge_count` counts OUTGOING edges and only the
    /// source's count ever moves — see
    /// `explicit_links::bump_edge_count`. Nothing in the system
    /// decrements it, and until a memory is *destroyed* nothing needs
    /// to: `tombstone` keeps the record and its edges, so the count
    /// stays true.
    ///
    /// Destroying a memory breaks that. `survivor -> destroyed`
    /// disappears from edges.db while the survivor's record goes on
    /// counting it, permanently, and that number feeds degree
    /// centrality and cache weight. Cross-namespace survivors are the
    /// visible case — a namespace purge is the one operation that
    /// removes edges from memories it is not deleting.
    /// Resolve a namespace name to its directory under `db_path`,
    /// refusing anything that is not exactly one ordinary path
    /// component.
    ///
    /// `Path::join` does **not** normalize, so this is the whole
    /// defence between a namespace name and an arbitrary directory:
    /// `db_path.join("..")` is the parent of the data directory and
    /// `db_path.join("")` is the data directory itself. Both are
    /// `remove_dir_all` targets in [`Self::remove_namespace_dir`].
    ///
    /// Modelled on the check
    /// [`VectorManager::open_or_create`](crate::storage::vectors::VectorManager::open_or_create)
    /// already performs — the non-destructive path had no business
    /// being the better-guarded one.
    fn namespace_dir(&self, name: &str) -> Result<PathBuf, StorageError> {
        let reject = |reason: &str| {
            Err(StorageError::InvalidNamespaceName {
                name: name.to_string(),
                reason: reason.to_string(),
            })
        };

        // Separators are refused explicitly rather than left to
        // `components`: a backslash is a separator on Windows and an
        // ordinary character on Unix, and a name that means two
        // different paths on two platforms is not a name this may act
        // on.
        if name.is_empty() {
            return reject("must not be empty");
        }
        if name.contains('/') || name.contains('\\') {
            return reject("must not contain '/' or '\\'");
        }

        let mut components = Path::new(name).components();
        let first_is_the_whole_name =
            matches!(components.next(), Some(Component::Normal(c)) if c == OsStr::new(name));
        if !first_is_the_whole_name || components.next().is_some() {
            return reject(
                "must be exactly one ordinary path component (not '.', '..', or a path)",
            );
        }

        Ok(self.db_path.join(name))
    }

    /// Remove one namespace's vector directory. Returns whether it was
    /// actually removed.
    ///
    /// Unlike the boot sweep this joins a name straight onto `db_path`
    /// rather than walking `read_dir`, so it does its own guarding:
    ///
    /// - [`Self::namespace_dir`] refuses anything that is not one
    ///   ordinary path component, which is what keeps `..` and `""`
    ///   away from `remove_dir_all`.
    /// - a symlink is refused rather than followed: `remove_dir_all`
    ///   follows a symlinked *root*, and a namespace directory replaced
    ///   by a symlink would hand it an arbitrary tree to delete.
    /// - the canonicalized target must be a strict child of the
    ///   canonicalized `db_path`. The component check already implies
    ///   this; canonicalizing is what *proves* it, and proof is what a
    ///   recursive delete is worth.
    ///
    /// Never returns an error. Step 3 of the purge has already
    /// committed by the time this runs, so a failure here is wasted disk
    /// to be logged and swept at next boot, not a reason to report the
    /// deletion as failed.
    fn remove_namespace_dir(&self, name: &str) -> bool {
        let dir = match self.namespace_dir(name) {
            Ok(dir) => dir,
            Err(e) => {
                tracing::error!(
                    namespace = name,
                    error = %e,
                    "Refusing to remove a namespace directory for an unusable name"
                );
                return false;
            }
        };

        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(e) => {
                tracing::warn!(
                    path = %dir.display(),
                    error = %e,
                    "Could not stat namespace directory; leaving it on disk"
                );
                return false;
            }
        };

        if metadata.file_type().is_symlink() {
            tracing::warn!(
                path = %dir.display(),
                "Namespace directory is a symlink; refusing to follow it. Remove the \
                 link target by hand if it really is namespace data."
            );
            return false;
        }

        if !metadata.is_dir() {
            tracing::warn!(
                path = %dir.display(),
                "Namespace path is not a directory; leaving it alone"
            );
            return false;
        }

        // The last word before the recursive delete: resolve both ends
        // and require a strict parent/child relationship.
        let canonical_base = match self.db_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    path = %self.db_path.display(),
                    error = %e,
                    "Could not canonicalize the data directory; leaving the namespace \
                     directory on disk"
                );
                return false;
            }
        };
        let canonical_dir = match dir.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    path = %dir.display(),
                    error = %e,
                    "Could not canonicalize the namespace directory; leaving it on disk"
                );
                return false;
            }
        };
        if canonical_dir.parent() != Some(canonical_base.as_path()) {
            tracing::error!(
                namespace = name,
                path = %canonical_dir.display(),
                base = %canonical_base.display(),
                "Namespace directory does not resolve to a direct child of the data \
                 directory; refusing to remove it"
            );
            return false;
        }

        match std::fs::remove_dir_all(&dir) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    path = %dir.display(),
                    error = %e,
                    "Failed to remove namespace directory; it will be swept at next startup"
                );
                false
            }
        }
    }
}

/// Where the boot sweep moves orphaned namespace directories.
///
/// Leading dot on purpose: the sweep skips dotfile directories, so its
/// own quarantine can never become a candidate for the next sweep.
const QUARANTINE_DIR_NAME: &str = ".orphaned";

/// How many records one `purge_namespace_records` transaction commits.
///
/// Bounds how long a purge holds the redb write lock. Small enough that
/// a concurrent reader is not stalled for seconds; large enough that a
/// hundred-thousand-record namespace is not a hundred thousand fsyncs.
const PURGE_CHUNK_SIZE: usize = 1_000;

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
        self.ensure_available()?;
        if record.version != 1 || record.schema.is_some() {
            return Err(StorageError::RecoveryRequired(
                "schema-v2 records require participating writers".into(),
            ));
        }
        record.encode()?;
        // Reject identity conflicts before allocating or appending payloads.
        if record.id != *id.as_bytes() || record.namespace_id != namespace_id.get() {
            return Err(StorageError::InvalidMutation(
                "record ID/namespace disagrees with insert arguments".into(),
            ));
        }
        if record.phase == DecayPhase::Tombstone {
            return Err(StorageError::InvalidMutation(
                "cannot allocate a vector for a tombstone".into(),
            ));
        }
        if self.meta_store.exists(id)? {
            return Err(StorageError::DuplicateId(id.into_inner()));
        }
        self.meta_store
            .get_namespace(namespace_id)?
            .ok_or(StorageError::NamespaceNotFound(namespace_id.get()))?;
        self.meta_store.ensure_uuid_not_terminal(id)?;
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

        // A commit I/O error can have an uncertain outcome. Never reclaim
        // bytes that metadata may already own; recovery must decide.
        self.meta_store.insert(id, record)?;

        Ok(())
    }

    fn get_record(&self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError> {
        self.ensure_available()?;
        self.meta_store.get(id)
    }

    fn get_vector(
        &self,
        namespace_id: NamespaceId,
        slot: u32,
    ) -> Result<Option<Vec<f32>>, StorageError> {
        self.ensure_available()?;
        let vector_store = self
            .vector_manager
            .get(namespace_id)
            .ok_or(StorageError::NamespaceNotFound(namespace_id.get()))?;
        Ok(vector_store.get_vector(slot).map(|v| v.to_vec()))
    }

    fn get_text(&self, text_ref: TextRef) -> Result<Option<String>, StorageError> {
        self.ensure_available()?;
        if !text_ref.is_some() {
            return Ok(None);
        }
        self.text_store.read(text_ref).map(Some)
    }

    fn delete_memory(&mut self, id: MemoryId) -> Result<Option<DiskRecord>, StorageError> {
        Ok(self
            .delete_memory_with_edges(id, &[])?
            .and_then(|r| r.removed.into_iter().next().map(|(_, r)| r)))
    }

    fn tombstone_memory(&self, id: MemoryId) -> Result<(), StorageError> {
        self.ensure_available()?;
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
        self.ensure_available()?;
        self.meta_store.update_decay_state(
            id,
            phase,
            strength,
            decay_strength,
            stability,
            is_permastore,
        )
    }

    fn apply_legacy_feedback(
        &self,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        plan: &crate::decay::lifecycle::FeedbackPlan,
    ) -> Result<bool, StorageError> {
        self.ensure_available()?;
        self.meta_store
            .apply_legacy_feedback(id, expected, namespace, plan)
    }

    fn apply_legacy_decay(
        &self,
        id: MemoryId,
        expected: &DiskRecord,
        namespace: &NamespaceConfig,
        phase: DecayPhase,
        strength: f32,
        decay_strength: f32,
    ) -> Result<bool, StorageError> {
        self.ensure_available()?;
        self.meta_store
            .apply_legacy_decay(id, expected, namespace, phase, strength, decay_strength)
    }

    fn strip_full_text(&self, id: MemoryId) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.meta_store.strip_full_text(id)
    }

    fn strip_summary(&self, id: MemoryId) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.meta_store.strip_summary(id)
    }

    fn ids_in_phase(&self, phase: DecayPhase) -> Result<Vec<MemoryId>, StorageError> {
        self.ensure_available()?;
        self.meta_store.ids_in_phase(phase)
    }

    fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        self.ensure_available()?;
        self.meta_store.scan_phase_records(phase)
    }

    fn decay_sweep(
        &self,
        compute_new_state: &dyn Fn(&DiskRecord) -> Option<DiskRecord>,
    ) -> Result<usize, StorageError> {
        self.ensure_available()?;
        self.meta_store.decay_sweep(compute_new_state)
    }

    // ── Access Tracking ─────────────────────────────────────────────

    fn update_access(
        &self,
        id: MemoryId,
        timestamp: i64,
        kind: AccessKind,
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
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
        self.ensure_available()?;
        self.batch_add_edges(&[PersistedEdge {
            source,
            target,
            edge_type,
            weight,
            auto_created,
            created_at,
        }])
    }

    fn batch_add_edges(&self, edges: &[PersistedEdge]) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.put_edges_with_effects(edges).map(|_| ())
    }

    fn get_outgoing_edges(
        &self,
        source: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError> {
        self.ensure_available()?;
        self.edge_store.get_outgoing(source)
    }

    fn get_incoming_edges(
        &self,
        target: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError> {
        self.ensure_available()?;
        self.edge_store.get_incoming(target)
    }

    fn remove_all_edges(&self, memory_id: MemoryId) -> Result<usize, StorageError> {
        self.ensure_available()?;
        let guard = self.meta_store.mutation_guard()?;
        let Some(record) = self.meta_store.get(memory_id)? else {
            return Ok(0);
        };
        let result = self.meta_store.commit_graph_effects(
            &guard,
            NamespaceId::new(record.namespace_id),
            &self.edge_store.load_all_edges()?,
            &[],
            &[memory_id],
            false,
            false,
        )?;
        effects::apply_pending(&self.meta_store, &self.edge_store, None).map_err(|source| {
            StorageError::CommittedEffectsPending {
                operation_id: result.receipt.operation_id,
                source: Box::new(source),
            }
        })?;
        Ok(result.edges_removed)
    }

    fn load_all_edges(&self) -> Result<Vec<PersistedEdge>, StorageError> {
        self.ensure_available()?;
        self.edge_store.load_all_edges()
    }

    // ── Namespaces ──────────────────────────────────────────────────

    /// # The row must never outlive the directory
    ///
    /// This used to commit the `NAMESPACE_TABLE` row and only then ask
    /// `open_or_create` whether the name could be a directory. A
    /// rejected name therefore left a committed row for a namespace
    /// whose directory can never be created — and [`Self::open`] step 6
    /// calls `open_or_create` for **every** row and `?`-propagates, so
    /// the next boot failed outright. One rejected `create_namespace`
    /// call wedged the database.
    ///
    /// So: validate first, and roll the row back if the vector store
    /// still refuses. Validation cannot see a directory already flocked
    /// by another store or a full disk, and both wedge the boot in
    /// exactly the same way.
    fn create_namespace(&mut self, config: &NamespaceConfig) -> Result<NamespaceId, StorageError> {
        self.ensure_available()?;
        validate_namespace_name(&config.name).map_err(|e| StorageError::InvalidNamespaceName {
            name: config.name.clone(),
            reason: e.to_string(),
        })?;

        let ns_id = self.meta_store.create_namespace(config)?;

        // Open/create the vector file for this namespace.
        if let Err(e) =
            self.vector_manager
                .open_or_create(ns_id, &config.name, config.embedding_dim as usize)
        {
            if let Err(rollback) = self.meta_store.delete_namespace(ns_id) {
                tracing::error!(
                    namespace = %config.name,
                    namespace_id = ns_id.get(),
                    error = %rollback,
                    "Could not roll back the namespace row after its vector store failed \
                     to open. The next startup will fail to open this namespace; remove \
                     the row by hand."
                );
            }
            return Err(e.into());
        }

        Ok(ns_id)
    }

    fn list_namespaces(&self) -> Result<Vec<NamespaceConfig>, StorageError> {
        self.ensure_available()?;
        self.meta_store.list_namespaces()
    }

    fn get_namespace(&self, id: NamespaceId) -> Result<Option<NamespaceConfig>, StorageError> {
        self.ensure_available()?;
        self.meta_store.get_namespace(id)
    }

    fn get_namespace_by_name(&self, name: &str) -> Result<Option<NamespaceConfig>, StorageError> {
        self.ensure_available()?;
        self.meta_store.get_namespace_by_name(name)
    }

    // ── Text Compaction ─────────────────────────────────────────────

    fn compact_text_log(&mut self) -> Result<CompactionResult, StorageError> {
        self.ensure_available()?;
        self.compact_text_at(|_| Ok(()))
    }

    // ── Tags ────────────────────────────────────────────────────────

    fn memories_with_tag(&self, tag: &str) -> Result<Vec<MemoryId>, StorageError> {
        self.ensure_available()?;
        self.meta_store.memories_with_tag(tag)
    }

    fn list_tags(&self) -> Result<Vec<(String, u64)>, StorageError> {
        self.ensure_available()?;
        self.meta_store.list_tags()
    }

    // ── Edge Count ──────────────────────────────────────────────────

    fn update_edge_count(&self, id: MemoryId, edge_count: u16) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.meta_store.update_edge_count(id, edge_count)
    }

    // ── Bulk / Diagnostic ───────────────────────────────────────────

    fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        self.ensure_available()?;
        self.meta_store.scan_all()
    }

    fn count(&self) -> Result<u64, StorageError> {
        self.ensure_available()?;
        self.meta_store.count()
    }

    fn count_active(&self) -> Result<u64, StorageError> {
        self.ensure_available()?;
        self.meta_store.count_active()
    }

    fn exists(&self, id: MemoryId) -> Result<bool, StorageError> {
        self.ensure_available()?;
        self.meta_store.exists(id)
    }

    fn free_vector_slot(
        &mut self,
        namespace_id: NamespaceId,
        slot: u32,
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        // Legacy callers carry no allocation epoch. Reject any release of a
        // currently owned slot, including delayed releases after slot reuse.
        // ponytail: O(n) guard until canonical vector_owners replaces the scan.
        if self.meta_store.scan_all()?.iter().any(|(_, record)| {
            record.namespace_id == namespace_id.get()
                && record.vector_slot == slot
                && record.phase != DecayPhase::Tombstone
        }) {
            return Err(StorageError::OwnershipMismatch(format!(
                "namespace {} slot {slot} is live",
                namespace_id.get()
            )));
        }
        if let Some(vector_store) = self.vector_manager.get_mut(namespace_id) {
            vector_store.free_slot(slot)?;
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        self.ensure_available()?;
        // Sync fulltext.dat.
        self.text_store.sync_data()?;

        // Sync all open vector files.
        for (_, store) in self.vector_manager.iter() {
            store.sync()?;
        }

        Ok(())
    }

    fn persist_phase_index(&self) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.meta_store.persist_phase_index()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EdgeType;

    #[test]
    fn effects_application_failure_returns_committed_id_and_denies_until_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "effects");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        let store_id = engine.control_state().unwrap().store_id;
        engine
            .meta_store
            .fail_effect_application
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = engine
            .add_edge(a, b, EdgeType::Associative, 0.5, false, 10)
            .unwrap_err();
        let StorageError::CommittedEffectsPending { operation_id, .. } = err else {
            panic!("missing commit evidence: {err}")
        };
        assert!(engine.get_record(a).is_err());
        assert!(engine.delete_memory(a).is_err());
        assert!(
            engine
                .meta_store
                .update_access(a, 100, AccessKind::DirectRetrieval)
                .is_err()
        );
        drop(engine);
        let engine = open_engine(dir.path());
        assert_eq!(engine.get_record(a).unwrap().unwrap().edge_count, 1);
        assert_eq!(engine.load_all_edges().unwrap().len(), 1);
        assert_eq!(
            engine
                .lookup_operation(store_id, MutationScope::Namespace(ns), operation_id)
                .unwrap()
                .unwrap()
                .operation_id,
            operation_id
        );
    }

    #[test]
    fn effects_process_child() {
        let Ok(path) = std::env::var("RECALLD_TEST_EFFECT_STORE") else {
            return;
        };
        let mut engine = open_engine(Path::new(&path));
        let records = engine.scan_all().unwrap();
        let [a, b, c] = records
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let bridge = PersistedEdge {
            source: a,
            target: c,
            edge_type: EdgeType::ParentChild,
            weight: 0.25,
            auto_created: true,
            created_at: 111,
        };
        if std::env::var_os("RECALLD_TEST_EFFECT_GRAPH").is_some() {
            let mut graph = crate::graph::RelationshipGraph::new();
            for (id, record) in &records {
                graph
                    .add_node(
                        *id,
                        NamespaceId::new(record.namespace_id),
                        record.phase,
                        record.decay_strength,
                        record.vector_slot,
                    )
                    .unwrap();
            }
            crate::graph::rebuild_from_storage(
                &mut graph,
                engine.load_all_edges().unwrap().into_iter(),
            );
            let graph = std::sync::Arc::new(tokio::sync::RwLock::new(graph));
            let storage = std::sync::Arc::new(std::sync::RwLock::new(engine));
            let cache = std::sync::Arc::new(crate::cache::CacheManager::new(
                crate::cache::CacheConfig::default(),
                None,
            ));
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    crate::graph::autolink::persist_edges(a, &[bridge], &graph, &storage, &cache)
                        .await
                        .unwrap();
                });
            panic!("graph crash boundary was not reached");
        }
        engine.delete_memory_with_edges(b, &[bridge]).unwrap();
        panic!("crash boundary was not reached");
    }

    #[test]
    fn effects_crash_reopen_replays_before_cleanup_once_logically() {
        for point in [
            "before_metadata_commit",
            "after_metadata_commit",
            "before_edge_commit",
            "after_edge_commit",
            "after_vector_sync",
            "after_canonical_ack",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut engine = open_engine(dir.path());
            let ns = make_namespace(&mut engine, "effects");
            let a = add_memory(&mut engine, ns);
            let b = add_memory(&mut engine, ns);
            let c = add_memory(&mut engine, ns);
            engine
                .add_edge(a, b, EdgeType::ParentChild, 0.5, false, 5)
                .unwrap();
            engine
                .add_edge(b, c, EdgeType::ParentChild, 0.5, false, 6)
                .unwrap();
            engine.meta_store.acknowledge_rebuilt_effects().unwrap();
            let before = engine.control_state().unwrap();
            let slot = engine.get_record(b).unwrap().unwrap().vector_slot;
            drop(engine);
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::engine::tests::effects_process_child",
                    "--nocapture",
                ])
                .env("RECALLD_TEST_EFFECT_STORE", dir.path())
                .env("RECALLD_TEST_EFFECT_CRASH", point)
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(91),
                "{point}: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            let committed = point != "before_metadata_commit";
            let canonical_pending = committed && point != "after_canonical_ack";
            {
                let meta = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
                assert_eq!(meta.get(b).is_err(), canonical_pending, "{point}");
            }
            if canonical_pending {
                assert!(
                    crate::storage::validation::validate_existing(dir.path()).is_err(),
                    "backup must refuse pending {point}"
                );
            }
            let mut op = None;
            for _ in 0..2 {
                let engine = open_engine(dir.path());
                assert_eq!(
                    engine.get_record(b).unwrap().is_none(),
                    committed,
                    "{point}"
                );
                let edges = engine.load_all_edges().unwrap();
                assert_eq!(edges.len(), if committed { 1 } else { 2 });
                assert_eq!(engine.get_record(a).unwrap().unwrap().edge_count, 1);
                assert_eq!(
                    engine.vector_manager.get(ns).unwrap().is_free(slot),
                    committed
                );
                let expected = if committed {
                    before.committed_generation.next().unwrap()
                } else {
                    before.committed_generation
                };
                assert_eq!(
                    engine.control_state().unwrap().committed_generation,
                    expected
                );
                let pending = engine.meta_store.pending_effects().unwrap();
                if committed {
                    assert_eq!(
                        edges[0],
                        PersistedEdge {
                            source: a,
                            target: c,
                            edge_type: EdgeType::ParentChild,
                            weight: 0.25,
                            auto_created: true,
                            created_at: 111
                        }
                    );
                    assert_eq!(pending.len(), 1);
                    assert!(pending[0].canonical_applied);
                    let id = pending[0].receipt.operation_id;
                    assert_eq!(
                        engine
                            .lookup_operation(before.store_id, MutationScope::Namespace(ns), id)
                            .unwrap(),
                        Some(pending[0].receipt.clone())
                    );
                    if let Some(previous) = op {
                        assert_eq!(previous, id);
                    }
                    op = Some(id);
                    assert!(engine.meta_store.effects_stamp().is_err());
                } else {
                    assert!(pending.is_empty());
                }
            }
        }
    }

    #[test]
    fn effects_graph_publication_crashes_remain_durable_and_unavailable() {
        for point in ["before_graph_publish", "before_derived_ack"] {
            let dir = tempfile::tempdir().unwrap();
            let mut engine = open_engine(dir.path());
            let ns = make_namespace(&mut engine, "effects");
            let a = add_memory(&mut engine, ns);
            let b = add_memory(&mut engine, ns);
            let c = add_memory(&mut engine, ns);
            engine
                .add_edge(a, b, EdgeType::ParentChild, 0.5, false, 5)
                .unwrap();
            engine
                .add_edge(b, c, EdgeType::ParentChild, 0.5, false, 6)
                .unwrap();
            engine.meta_store.acknowledge_rebuilt_effects().unwrap();
            let before = engine.control_state().unwrap();
            drop(engine);
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::engine::tests::effects_process_child",
                    "--nocapture",
                ])
                .env("RECALLD_TEST_EFFECT_STORE", dir.path())
                .env("RECALLD_TEST_EFFECT_CRASH", point)
                .env("RECALLD_TEST_EFFECT_GRAPH", "1")
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(91),
                "{point}: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            for _ in 0..2 {
                let engine = open_engine(dir.path());
                assert_eq!(engine.load_all_edges().unwrap().len(), 3);
                assert_eq!(engine.get_record(a).unwrap().unwrap().edge_count, 2);
                assert_eq!(
                    engine.control_state().unwrap().committed_generation,
                    before.committed_generation.next().unwrap()
                );
                assert_eq!(engine.meta_store.pending_effects().unwrap().len(), 1);
                assert!(engine.meta_store.effects_stamp().is_err());
            }
        }
    }

    #[test]
    fn effects_final_guard_checks_phase_and_supersedes_structure() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "effects");
        let old = add_memory(&mut engine, ns);
        let newer = add_memory(&mut engine, ns);
        let newest = add_memory(&mut engine, ns);
        engine.tombstone_memory(old).unwrap();
        let before = engine.control_state().unwrap();
        for kind in [EdgeType::Associative, EdgeType::Entity, EdgeType::Temporal] {
            assert!(engine.add_edge(newer, old, kind, 0.5, true, 0).is_err());
        }
        assert_eq!(engine.control_state().unwrap(), before);
        // Legacy structural correction keeps a content-free old endpoint.
        engine
            .add_edge(newer, old, EdgeType::Supersedes, 1.0, false, 1)
            .unwrap();
        let before = engine.control_state().unwrap();
        assert!(
            engine
                .add_edge(newest, old, EdgeType::Supersedes, 1.0, false, 2)
                .is_err()
        );
        assert_eq!(engine.control_state().unwrap(), before);
        engine
            .add_edge(newest, newer, EdgeType::Supersedes, 1.0, false, 3)
            .unwrap();
        assert!(
            engine
                .add_edge(newer, newest, EdgeType::Supersedes, 1.0, false, 4)
                .is_err()
        );
        engine
            .add_edge(old, newest, EdgeType::ParentChild, 1.0, false, 5)
            .unwrap();
        assert_eq!(
            engine.get_record(old).unwrap().unwrap().phase,
            DecayPhase::Tombstone
        );
        assert!(engine.get_record(old).unwrap().unwrap().summary.is_empty());
    }

    #[test]
    fn effects_prevalidation_is_atomic_and_counts_are_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "same");
        let other = make_namespace(&mut engine, "other");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        let c = add_memory(&mut engine, other);
        let before = engine.control_state().unwrap();
        let valid = PersistedEdge {
            source: a,
            target: b,
            edge_type: EdgeType::Associative,
            weight: 0.5,
            auto_created: false,
            created_at: 1,
        };
        let invalid = PersistedEdge {
            target: c,
            ..valid.clone()
        };
        assert!(engine.batch_add_edges(&[valid.clone(), invalid]).is_err());
        assert_eq!(engine.control_state().unwrap(), before);
        assert_eq!(engine.get_record(a).unwrap().unwrap().edge_count, 0);
        assert!(engine.load_all_edges().unwrap().is_empty());
        engine
            .batch_add_edges(std::slice::from_ref(&valid))
            .unwrap();
        engine.batch_add_edges(&[valid]).unwrap();
        assert_eq!(engine.get_record(a).unwrap().unwrap().edge_count, 1);
        engine.remove_all_edges(b).unwrap();
        assert_eq!(engine.get_record(a).unwrap().unwrap().edge_count, 0);
        assert!(engine.meta_store.effects_stamp().is_err());
        engine.meta_store.acknowledge_rebuilt_effects().unwrap();
        assert!(engine.meta_store.effects_stamp().is_ok());
        // The historical raw store admitted self edges. Removing one must not
        // commit an outbox row that rejects its own cleanup during replay.
        engine
            .edge_store
            .add_edge(a, a, EdgeType::Associative, 0.5, false, 0)
            .unwrap();
        engine.delete_memory(a).unwrap();
        assert!(engine.load_all_edges().unwrap().is_empty());
    }

    #[test]
    fn unavailable_handle_blocks_payloads_metadata_and_writes_until_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "gate");
        let id = add_memory(&mut engine, ns);
        let record = engine.get_record(id).unwrap().unwrap();
        engine.mark_unavailable();
        assert!(engine.get_record(id).is_err());
        assert!(engine.meta_store().get(id).is_err());
        assert!(
            engine
                .get_text(TextRef {
                    file_offset: record.text_offset,
                    length: record.text_length
                })
                .is_err()
        );
        assert!(engine.get_vector(ns, record.vector_slot).is_err());
        assert!(engine.load_all_edges().is_err());
        assert!(engine.delete_memory(id).is_err());
        assert!(engine.meta_store().update_edge_count(id, 9).is_err());
        assert!(engine.sync().is_err());
        drop(engine);
        let engine = open_engine(dir.path());
        assert_eq!(
            engine.get_record(id).unwrap().unwrap().to_bytes(),
            record.to_bytes()
        );
    }

    #[test]
    fn containment_rejects_insert_identity_conflicts_before_payload_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "one");
        let other = make_namespace(&mut engine, "two");
        let id = add_memory(&mut engine, ns);
        let original = engine.get_record(id).unwrap().unwrap();
        let vector_size = std::fs::metadata(dir.path().join("two/vectors.dat"))
            .unwrap()
            .len();
        let text_size = std::fs::metadata(dir.path().join("fulltext.dat"))
            .unwrap()
            .len();
        let mut wrong = original.clone();
        assert!(
            engine
                .insert_memory(id, other, &mut wrong, &[1.0; DIM], Some("rejected"))
                .is_err()
        );
        assert!(
            engine
                .insert_memory(
                    MemoryId::new(),
                    ns,
                    &mut wrong,
                    &[1.0; DIM],
                    Some("rejected")
                )
                .is_err()
        );
        assert!(matches!(
            engine.insert_memory(id, ns, &mut wrong, &[1.0; DIM], Some("duplicate")),
            Err(StorageError::DuplicateId(_))
        ));
        let fresh = MemoryId::new();
        wrong.id = *fresh.as_bytes();
        assert!(
            engine
                .insert_memory(fresh, ns, &mut wrong, &[1.0], Some("bad dimension"))
                .is_err()
        );
        assert_eq!(engine.count().unwrap(), 1);
        assert_eq!(
            std::fs::metadata(dir.path().join("two/vectors.dat"))
                .unwrap()
                .len(),
            vector_size
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("fulltext.dat"))
                .unwrap()
                .len(),
            text_size
        );
        assert_eq!(
            engine.get_record(id).unwrap().unwrap().to_bytes(),
            original.to_bytes()
        );
    }

    #[test]
    fn containment_tombstone_reuse_delete_and_stale_release_preserve_new_owner() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "default");
        let a = add_memory(&mut engine, ns);
        let slot = engine.get_record(a).unwrap().unwrap().vector_slot;
        engine.tombstone_memory(a).unwrap();
        engine.free_vector_slot(ns, slot).unwrap();
        engine.free_vector_slot(ns, slot).unwrap(); // idempotent, no free-list cycle
        let b = MemoryId::new();
        let mut record = engine.get_record(a).unwrap().unwrap();
        record.id = *b.as_bytes();
        record.phase = DecayPhase::Summary;
        engine
            .insert_memory(b, ns, &mut record, &[1., 2., 3., 4.], None)
            .unwrap();
        assert_eq!(record.vector_slot, slot);
        assert_eq!(engine.ids_in_phase(DecayPhase::Summary).unwrap(), vec![b]);
        assert_eq!(engine.ids_in_phase(DecayPhase::Tombstone).unwrap(), vec![a]);
        assert!(matches!(
            engine.free_vector_slot(ns, slot),
            Err(StorageError::OwnershipMismatch(_))
        ));
        engine.delete_memory(a).unwrap();
        assert!(engine.delete_memory(a).unwrap().is_none());
        let c = add_memory(&mut engine, ns);
        assert_ne!(engine.get_record(c).unwrap().unwrap().vector_slot, slot);
        engine.sync().unwrap();
        drop(engine);
        let engine = open_engine(dir.path());
        assert_eq!(
            engine.get_vector(ns, slot).unwrap().unwrap(),
            vec![1., 2., 3., 4.]
        );
        assert_eq!(engine.ids_in_phase(DecayPhase::Summary).unwrap(), vec![b]);
    }

    #[test]
    fn containment_preserves_ambiguous_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let text = std::fs::read(dir.path().join("fulltext.dat")).unwrap();
        engine.compact_text_log().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("fulltext.dat")).unwrap(),
            text
        );
        drop(engine);
        for artifact in [
            ".compacting",
            ".compaction-meta-committed",
            "fulltext.dat.new",
        ] {
            std::fs::write(dir.path().join(artifact), b"independent evidence").unwrap();
            for _ in 0..2 {
                assert!(matches!(
                    RedbStorageEngine::open(dir.path()),
                    Err(StorageError::RecoveryRequired(_))
                ));
                assert_eq!(
                    std::fs::read(dir.path().join(artifact)).unwrap(),
                    b"independent evidence"
                );
            }
            std::fs::remove_file(dir.path().join(artifact)).unwrap();
        }
    }

    #[test]
    fn containment_existing_store_never_initializes_missing_payloads() {
        for missing in ["meta.db", "edges.db", "fulltext.dat", "default/vectors.dat"] {
            let dir = tempfile::tempdir().unwrap();
            let mut engine = open_engine(dir.path());
            make_namespace(&mut engine, "default");
            drop(engine);
            let path = dir.path().join(missing);
            std::fs::remove_file(&path).unwrap();
            assert!(RedbStorageEngine::open(dir.path()).is_err(), "{missing}");
            assert!(!path.exists(), "{missing} was recreated");
        }
    }

    #[test]
    fn containment_reopen_rejects_duplicate_owners_and_owned_free_slots() {
        for duplicate in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut engine = open_engine(dir.path());
            let ns = make_namespace(&mut engine, "default");
            let a = add_memory(&mut engine, ns);
            let mut record = engine.get_record(a).unwrap().unwrap();
            if duplicate {
                let b = MemoryId::new();
                record.id = *b.as_bytes();
                engine.meta_store.insert(b, &record).unwrap();
            } else {
                engine
                    .vector_manager
                    .get_mut(ns)
                    .unwrap()
                    .free_slot(record.vector_slot)
                    .unwrap();
            }
            engine.sync().unwrap();
            drop(engine);
            assert!(matches!(
                RedbStorageEngine::open(dir.path()),
                Err(StorageError::OwnershipMismatch(_))
            ));
        }
    }

    const DIM: usize = 4;

    fn open_engine(dir: &Path) -> RedbStorageEngine {
        RedbStorageEngine::open(dir).expect("open engine")
    }

    fn make_namespace(engine: &mut RedbStorageEngine, name: &str) -> NamespaceId {
        let mut config = NamespaceConfig::default_namespace(0);
        config.name = name.to_string();
        config.embedding_dim = DIM as u32;
        engine.create_namespace(&config).expect("create namespace")
    }

    fn add_memory(engine: &mut RedbStorageEngine, namespace_id: NamespaceId) -> MemoryId {
        let id = MemoryId::new();
        let mut record = DiskRecord {
            schema: None,
            version: DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: namespace_id.get(),
            created_at: 0,
            last_accessed_at: 0,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability: 1.0,
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "fixture".into(),
            tags: Vec::new(),
            access_history: Vec::new(),
            text_offset: 0,
            text_length: 0,
        };
        engine
            .insert_memory(id, namespace_id, &mut record, &[0.0; DIM], None)
            .expect("insert memory");
        id
    }

    #[test]
    fn canonical_access_receipt_restarts_and_survives_namespace_purge() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let namespace = make_namespace(&mut engine, "receipts");
        let id = add_memory(&mut engine, namespace);
        let state = engine.control_state().unwrap();
        let ctx = MutationContext {
            store_id: Some(state.store_id),
            scope: MutationScope::Namespace(namespace),
            idempotency_key: Some("request-1".into()),
            expected_generation: Some(state.committed_generation),
            contract_version: 1,
        };
        let receipt = engine
            .update_access_with_receipt(&ctx, id, 123, AccessKind::DirectRetrieval)
            .unwrap();
        // Lost-response boundary: close immediately after the real commit,
        // without an explicit sync/checkpoint acknowledgement to the caller.
        drop(engine);
        let mut engine = open_engine(dir.path());
        assert_eq!(engine.control_state().unwrap().store_id, state.store_id);
        assert_eq!(
            engine
                .lookup_operation(state.store_id, ctx.scope, receipt.operation_id)
                .unwrap(),
            Some(receipt.clone())
        );
        let replay = engine
            .update_access_with_receipt(&ctx, id, 999, AccessKind::DirectRetrieval)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(replay.outcome, receipt.outcome);
        assert_eq!(
            engine.get_record(id).unwrap().unwrap().access_history.len(),
            1
        );
        engine.purge_namespace(namespace).unwrap();
        let replacement = make_namespace(&mut engine, "receipts");
        assert_ne!(replacement, namespace);
        drop(engine);
        let engine = open_engine(dir.path());
        let before = engine.control_state().unwrap();
        let replay = engine
            .update_access_with_receipt(&ctx, id, 1000, AccessKind::DirectRetrieval)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(engine.control_state().unwrap(), before);
        assert!(engine.get_record(id).unwrap().is_none());
        assert!(
            engine
                .lookup_operation(
                    state.store_id,
                    MutationScope::Namespace(replacement),
                    receipt.operation_id
                )
                .unwrap()
                .is_none()
        );
        let encoded = serde_json::to_value(&replay).unwrap();
        assert!(encoded["committedGeneration"].is_string());
        assert_eq!(encoded["receiptVersion"], 1);
        assert!(encoded.get("ready").is_none()); // Receipt is not a readiness claim.
        assert!(!encoded.to_string().contains("fixture"));
    }

    /// The directory shape the boot sweep recognizes, planted by hand.
    /// True when `root` lives on a case-insensitive filesystem, as APFS on
    /// macOS is by default.
    ///
    /// Tests that plant a case variant of a live namespace's directory have
    /// to know this: on such a filesystem the variant is not a second
    /// directory, it is the same one, so planting into it overwrites the
    /// live namespace's real `vectors.dat`.
    fn filesystem_is_case_insensitive(root: &Path) -> bool {
        let probe = root.join("CaseProbe");
        std::fs::create_dir_all(&probe).expect("create case probe");
        let insensitive = root.join("caseprobe").exists();
        std::fs::remove_dir_all(&probe).expect("remove case probe");
        insensitive
    }

    fn plant_orphan_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("create orphan dir");
        std::fs::write(dir.join("vectors.dat"), b"not really a vector file")
            .expect("write vectors.dat");
        dir
    }

    // ── purge_namespace ──────────────────────────────────────────────

    #[test]
    fn w13_purge_removes_records_edges_the_config_row_and_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        let ns = make_namespace(&mut engine, "scratch");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        engine
            .add_edge(a, b, EdgeType::Associative, 0.5, false, 0)
            .expect("add edge");

        let purged = engine.purge_namespace(ns).expect("purge");

        assert_eq!(purged.config.name, "scratch");
        assert_eq!(purged.records.len(), 2);
        assert_eq!(purged.live_removed, 2);
        assert_eq!(purged.tombstones_removed, 0);
        assert_eq!(purged.edges_removed, 1);
        assert!(purged.directory_removed);

        assert!(engine.get_record(a).expect("get").is_none());
        assert!(engine.get_record(b).expect("get").is_none());
        assert!(engine.get_namespace(ns).expect("get namespace").is_none());
        assert!(engine.load_all_edges().expect("edges").is_empty());
        assert!(!dir.path().join("scratch").exists());
    }

    /// An edge from a surviving memory *into* a purged one is as dead as
    /// one that starts there. Leaving it behind would point the graph at
    /// a memory that no longer exists.
    #[test]
    fn w14_purge_removes_edges_that_point_into_the_namespace_from_outside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        let keep = make_namespace(&mut engine, "keep");
        let doomed = make_namespace(&mut engine, "doomed");
        let survivor = add_memory(&mut engine, keep);
        let victim = add_memory(&mut engine, doomed);
        engine
            .edge_store
            .add_edge(survivor, victim, EdgeType::Associative, 0.5, false, 0)
            .expect("add edge");

        let purged = engine.purge_namespace(doomed).expect("purge");

        assert_eq!(purged.edges_removed, 1);
        assert!(engine.load_all_edges().expect("edges").is_empty());
        assert!(
            engine.get_record(survivor).expect("get").is_some(),
            "the surviving memory was destroyed along with the edge"
        );
    }

    /// A purge that resumes after a crash between the config-row delete
    /// and the unlink finds no directory. That is a completed deletion,
    /// not a failure.
    #[test]
    fn w15_purge_tolerates_a_directory_that_is_already_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        let ns = make_namespace(&mut engine, "scratch");
        add_memory(&mut engine, ns);

        // Drop the store first, exactly as the purge would.
        drop(engine.vector_manager_mut().remove(ns));
        std::fs::remove_dir_all(dir.path().join("scratch")).expect("remove dir early");

        let purged = engine.purge_namespace(ns).expect("purge");
        assert!(!purged.directory_removed);
        assert_eq!(purged.records.len(), 1);
    }

    #[test]
    fn w16_boot_sweep_removes_a_namespace_shaped_directory_with_no_live_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let _engine = open_engine(dir.path());
        }
        let orphan = plant_orphan_dir(dir.path(), "ghost-namespace");

        let _engine = open_engine(dir.path());
        assert!(
            !orphan.exists(),
            "orphan directory was left in the data directory"
        );
        assert_eq!(
            quarantined(dir.path()).len(),
            1,
            "the orphan was destroyed rather than quarantined"
        );
    }

    /// The sweep's safety argument — "no NAMESPACE_TABLE row means dead"
    /// — has no lower bound: a restored-from-backup empty meta.db
    /// makes `live` empty and EVERY namespace directory a candidate.
    /// Every guard passes, because they really are namespace
    /// directories. Quarantine is what makes that survivable.
    #[test]
    fn w59_a_swept_directory_is_moved_not_destroyed() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut engine = open_engine(dir.path());
            make_namespace(&mut engine, "irreplaceable");
            std::fs::write(
                dir.path().join("irreplaceable").join("vectors.dat"),
                b"the only copy",
            )
            .expect("write vectors.dat");
        }

        // A complete older catalog can omit the namespace. An absent meta.db
        // now fails closed instead (covered by the missing-payload regression).
        let older = tempfile::tempdir().unwrap();
        drop(open_engine(older.path()));
        // A selected text generation is part of the older catalog snapshot.
        for entry in std::fs::read_dir(older.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == "meta.db"
                || entry.file_name() == "fulltext.dat"
                || crate::storage::metadata::text_generation::is_generation_filename(
                    &entry.file_name().to_string_lossy(),
                )
            {
                std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
            }
        }

        {
            let _engine = open_engine(dir.path());
        }

        let survivors = quarantined(dir.path());
        assert_eq!(survivors.len(), 1, "{survivors:?}");
        assert!(
            survivors[0]
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("irreplaceable-")),
            "the quarantined directory must be named after the namespace: {survivors:?}"
        );
        assert_eq!(
            std::fs::read(survivors[0].join("vectors.dat")).expect("read"),
            b"the only copy",
            "the quarantined directory does not hold the original contents"
        );
    }

    /// The quarantine directory must never become the next sweep's
    /// candidate, or the recovery copy is swept into a copy of itself
    /// every boot.
    #[test]
    fn w60_the_quarantine_directory_is_not_itself_swept() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let _engine = open_engine(dir.path());
        }
        plant_orphan_dir(dir.path(), "ghost");

        {
            let _engine = open_engine(dir.path());
        }
        let after_first = quarantined(dir.path());
        assert_eq!(after_first.len(), 1);

        {
            let _engine = open_engine(dir.path());
        }
        assert_eq!(
            quarantined(dir.path()),
            after_first,
            "the second boot moved the quarantine"
        );
    }

    /// Everything currently in `<db_path>/.orphaned/`, sorted.
    fn quarantined(db_path: &Path) -> Vec<PathBuf> {
        let quarantine = db_path.join(QUARANTINE_DIR_NAME);
        let Ok(entries) = std::fs::read_dir(&quarantine) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        out.sort();
        out
    }

    /// `db_path` is user-configurable and may hold directories that are
    /// nothing to do with recalld. "Matches no live namespace" is not on
    /// its own a licence to recurse and delete.
    #[test]
    fn w17_boot_sweep_leaves_a_directory_that_is_not_shaped_like_a_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let _engine = open_engine(dir.path());
        }

        let with_extra = plant_orphan_dir(dir.path(), "not-ours");
        std::fs::write(with_extra.join("important.txt"), b"someone else's data")
            .expect("write extra file");

        let no_vectors = dir.path().join("empty-dir");
        std::fs::create_dir_all(&no_vectors).expect("create dir");

        let dotted = plant_orphan_dir(dir.path(), ".hidden-state");

        let _engine = open_engine(dir.path());

        assert!(with_extra.join("important.txt").exists());
        assert!(no_vectors.exists());
        assert!(dotted.exists());
    }

    /// THE catastrophic-regression test. The boot sweep deletes
    /// directories, it runs unattended on every open, and its blast
    /// radius if it misjudges "live" is every memory in the database.
    ///
    /// What makes it safe is an ordering guarantee: `open` step 6 opens
    /// (and thereby creates) a directory for every namespace in
    /// `NAMESPACE_TABLE` before step 7 runs the sweep, so at sweep time
    /// every live namespace has both a directory and a name in `live`.
    #[test]
    fn w18_boot_sweep_leaves_every_live_namespace_directory_on_a_plain_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ids = Vec::new();

        {
            let mut engine = open_engine(dir.path());
            for name in ["default", "work", "personal"] {
                let ns = make_namespace(&mut engine, name);
                ids.push(add_memory(&mut engine, ns));
            }
        }

        let engine = open_engine(dir.path());

        for name in ["default", "work", "personal"] {
            let ns_dir = dir.path().join(name);
            assert!(
                ns_dir.join("vectors.dat").exists(),
                "the boot sweep destroyed live namespace '{name}'"
            );
        }
        for id in ids {
            assert!(
                engine.get_record(id).expect("get").is_some(),
                "a live record went missing across a plain reopen"
            );
        }
    }

    /// The crash-recovery shape from D5: everything about the namespace
    /// is gone after a restart, including the edges that pointed at it.
    #[test]
    fn w19_a_purged_namespace_stays_gone_across_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");

        {
            let mut engine = open_engine(dir.path());
            let ns = make_namespace(&mut engine, "scratch");
            let a = add_memory(&mut engine, ns);
            let b = add_memory(&mut engine, ns);
            engine
                .add_edge(a, b, EdgeType::Associative, 0.5, false, 0)
                .expect("add edge");
            engine.purge_namespace(ns).expect("purge");
        }

        let engine = open_engine(dir.path());
        assert!(
            engine
                .get_namespace_by_name("scratch")
                .expect("lookup")
                .is_none()
        );
        assert!(!dir.path().join("scratch").exists());
        assert!(engine.load_all_edges().expect("edges").is_empty());
        assert_eq!(engine.count().expect("count"), 0);
    }

    // ── peer edge counts ─────────────────────────────────────────────

    /// `edge_count` counts OUTGOING edges. A purge removes the edge
    /// `survivor -> purged` from edges.db but used to leave the
    /// survivor's record still counting it — permanently, and in a
    /// number that feeds degree centrality and cache weight. The
    /// cross-namespace case is the one a user sees: the survivor is not
    /// in the namespace being deleted and nothing else will ever touch
    /// its record.
    #[test]
    fn w56_a_purge_repairs_the_outgoing_edge_count_of_a_surviving_peer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        let keep_ns = make_namespace(&mut engine, "keep");
        let doomed_ns = make_namespace(&mut engine, "doomed");

        let survivor = add_memory(&mut engine, keep_ns);
        let other_survivor = add_memory(&mut engine, keep_ns);
        let doomed_a = add_memory(&mut engine, doomed_ns);
        let doomed_b = add_memory(&mut engine, doomed_ns);

        // Two edges into the doomed namespace and one that stays.
        for target in [doomed_a, doomed_b, other_survivor] {
            engine
                .edge_store
                .add_edge(survivor, target, EdgeType::Associative, 0.5, false, 0)
                .expect("add edge");
        }
        engine.update_edge_count(survivor, 3).expect("seed count");

        let purged = engine.purge_namespace(doomed_ns).expect("purge");

        assert_eq!(
            engine
                .get_record(survivor)
                .expect("get")
                .expect("record")
                .edge_count,
            1,
            "the survivor still counts edges that no longer exist"
        );
        assert_eq!(purged.peer_edge_counts, vec![(survivor, 1)]);
    }

    /// A peer that is itself being purged needs no repair, and must not
    /// be reported as one: its record is gone.
    #[test]
    fn w57_a_purge_does_not_repair_a_peer_it_is_also_destroying() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        let ns = make_namespace(&mut engine, "doomed");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        engine
            .add_edge(a, b, EdgeType::Associative, 0.5, false, 0)
            .expect("add edge");
        engine.update_edge_count(a, 1).expect("seed count");

        let purged = engine.purge_namespace(ns).expect("purge");

        assert!(purged.peer_edge_counts.is_empty(), "{purged:?}");
    }

    /// The same repair on the single-memory hard delete, which removes
    /// edges for exactly the same reason and had exactly the same gap.
    #[test]
    fn w58_a_hard_delete_repairs_the_outgoing_edge_count_of_a_surviving_peer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        let ns = make_namespace(&mut engine, "scratch");
        let survivor = add_memory(&mut engine, ns);
        let doomed = add_memory(&mut engine, ns);
        engine
            .add_edge(survivor, doomed, EdgeType::Associative, 0.5, false, 0)
            .expect("add edge");
        engine.update_edge_count(survivor, 1).expect("seed count");

        engine.delete_memory(doomed).expect("delete");

        assert_eq!(
            engine
                .get_record(survivor)
                .expect("get")
                .expect("record")
                .edge_count,
            0,
            "the survivor still counts an edge that no longer exists"
        );
    }

    // ── namespace name → directory ───────────────────────────────────

    /// `Path::join` does **not** normalize, so `db_path.join("..")` is
    /// the parent of the data directory and `remove_dir_all` on it takes
    /// everything recalld owns — and everything that happens to sit
    /// beside it. The old guard checked only symlink-and-is-dir, both of
    /// which `..` passes.
    #[test]
    fn w45_a_dot_dot_name_cannot_delete_the_parent_of_the_data_directory() {
        let outer = tempfile::tempdir().expect("tempdir");
        let db_path = outer.path().join("data");
        std::fs::create_dir_all(&db_path).expect("create data dir");
        let bystander = outer.path().join("someone-elses-files");
        std::fs::create_dir_all(&bystander).expect("create bystander");

        let engine = open_engine(&db_path);

        assert!(
            !engine.remove_namespace_dir(".."),
            "'..' was accepted as a namespace directory name"
        );
        assert!(
            bystander.exists(),
            "the parent of the data directory was destroyed"
        );
        assert!(
            db_path.join("meta.db").exists(),
            "the database was destroyed"
        );
    }

    /// The one that needs no `..` and no daemon frame. `db_path.join("")`
    /// **is** `db_path`, `VectorManager::open_or_create` accepted `""`,
    /// so the namespace was live and healthy — and deleting it pointed
    /// `remove_dir_all` at the whole database.
    #[test]
    fn w46_an_empty_name_cannot_delete_the_data_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = open_engine(dir.path());

        assert!(
            !engine.remove_namespace_dir(""),
            "'' was accepted as a namespace directory name"
        );
        assert!(db_is_intact(dir.path()), "the database was destroyed");
    }

    #[cfg(unix)]
    #[test]
    fn parent_path_aliases_share_store_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        std::fs::create_dir(&real_parent).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real_parent, &alias).unwrap();
        let engine = open_engine(&alias.join("store"));
        assert!(matches!(
            RedbStorageEngine::open(real_parent.join("store")),
            Err(StorageError::DatabaseLocked)
        ));
        assert_eq!(
            engine.namespace_dir("scratch").unwrap(),
            real_parent.canonicalize().unwrap().join("store/scratch")
        );
        drop(engine);
        drop(open_engine(&real_parent.join("store")));
    }

    #[test]
    fn w47_only_a_single_ordinary_path_component_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = open_engine(dir.path());

        for bad in [
            "",
            ".",
            "..",
            "/",
            "sub/dir",
            "..\\up",
            "scratch/",
            "./scratch",
        ] {
            assert!(
                engine.namespace_dir(bad).is_err(),
                "{bad:?} was accepted as a namespace directory name"
            );
        }

        assert_eq!(
            engine.namespace_dir("scratch").expect("a plain name"),
            dir.path().canonicalize().unwrap().join("scratch")
        );
    }

    /// `create_namespace` used to commit the `NAMESPACE_TABLE` row and
    /// only THEN ask `open_or_create` whether the name could be a
    /// directory. The caller saw an error and the row survived — and
    /// [`RedbStorageEngine::open`] step 6 `?`-propagates that same
    /// rejection for every row, so one bad `create_namespace` call meant
    /// the database would not start again.
    #[test]
    fn w48_a_rejected_name_leaves_no_row_and_the_database_still_opens() {
        let dir = tempfile::tempdir().expect("tempdir");

        {
            let mut engine = open_engine(dir.path());
            for bad in ["", "..", "a/b", "has space", "ünïcode", &"x".repeat(65)] {
                let mut config = NamespaceConfig::default_namespace(0);
                config.name = bad.to_string();
                config.embedding_dim = DIM as u32;

                engine
                    .create_namespace(&config)
                    .expect_err(&format!("{bad:?} must be rejected"));

                assert!(
                    engine
                        .list_namespaces()
                        .expect("list")
                        .iter()
                        .all(|ns| ns.name != bad),
                    "{bad:?} left a NAMESPACE_TABLE row behind"
                );
            }
        }

        RedbStorageEngine::open(dir.path())
            .expect("a rejected create_namespace must not wedge the next startup");
    }

    // ── case-insensitive namespace identity ──────────────────────────

    /// Chain A. On a case-insensitive filesystem `notes/` and `Notes/`
    /// are one directory, so two rows pointing at it is two namespaces
    /// sharing one `vectors.dat`. The uniqueness check compared names
    /// exactly and let the second row in.
    #[test]
    fn w49_a_namespace_name_differing_only_in_case_is_a_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        make_namespace(&mut engine, "notes");

        for collision in ["Notes", "NOTES", "nOtEs"] {
            let mut config = NamespaceConfig::default_namespace(0);
            config.name = collision.to_string();
            config.embedding_dim = DIM as u32;
            let err = engine
                .create_namespace(&config)
                .expect_err("a case-variant name must collide");
            assert!(
                matches!(err, StorageError::DuplicateName(_)),
                "{collision}: {err}"
            );
        }

        assert_eq!(engine.list_namespaces().expect("list").len(), 1);
    }

    /// The lookup has to agree with the uniqueness rule, or
    /// `create_namespace` reports "duplicate" and `namespace_stats`
    /// reports "not found" for the same string.
    #[test]
    fn w50_a_namespace_resolves_by_name_regardless_of_case() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "notes");

        for spelling in ["notes", "Notes", "NOTES"] {
            assert_eq!(
                engine
                    .get_namespace_by_name(spelling)
                    .expect("lookup")
                    .map(|c| c.id),
                Some(ns),
                "{spelling} did not resolve"
            );
        }
    }

    /// The unattended half of chain A. A live namespace whose directory
    /// differs from its row only in case used to be invisible to the
    /// sweep's exact-string compare, and the shape guard cannot save it
    /// — it really is a namespace directory. Boot then deleted a live
    /// namespace's vectors.
    #[test]
    fn w51_the_boot_sweep_does_not_delete_a_case_variant_live_directory() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Probe before the engine exists, so the throwaway directory cannot
        // be seen by the boot sweep.
        let case_insensitive = filesystem_is_case_insensitive(dir.path());

        {
            let mut engine = open_engine(dir.path());
            make_namespace(&mut engine, "Notes");
            // On a case-sensitive filesystem the lowercase variant has to be
            // planted, so that the sweep is made to choose between deleting
            // it and recognising it as the live namespace's directory.
            //
            // On a case-insensitive one there is nothing to plant: creating
            // `Notes` already produced the directory `notes` resolves to.
            // Planting would write a stub over the live namespace's real
            // vectors.dat and the reopen below would fail to parse it --
            // which is exactly how this test used to fail on macOS while
            // passing on Linux.
            if !case_insensitive {
                plant_orphan_dir(dir.path(), "notes");
            }
        }

        let _engine = open_engine(dir.path());

        assert!(
            dir.path().join("notes").join("vectors.dat").exists(),
            "the boot sweep destroyed a live namespace's vectors"
        );
    }

    // ── partial purge ────────────────────────────────────────────────

    /// A failure before anything was committed must say so, or the
    /// caller cleans indexes for records that still exist.
    #[test]
    fn w52_a_purge_that_destroyed_nothing_reports_nothing_destroyed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());

        let err = engine
            .purge_namespace(NamespaceId::new(999))
            .expect_err("an unknown namespace cannot be purged");

        assert!(!err.is_partial(), "{err}");
        assert!(err.destroyed.is_empty());
        assert!(
            !err.to_string().contains("PARTIALLY"),
            "a failure that destroyed nothing must not claim otherwise: {err}"
        );
    }

    /// The other half of the contract, and the one that matters: an
    /// error that arrives after chunks have committed carries them, and
    /// says what the caller has to do about it. Without this the records
    /// are gone from meta.db, still in every index, and unreachable by a
    /// retry — which looks in meta.db and finds nothing.
    #[test]
    fn w53_a_partial_purge_carries_its_destroyed_records_and_asks_for_a_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "scratch");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);

        let destroyed: Vec<(MemoryId, DiskRecord)> = [a, b]
            .into_iter()
            .map(|id| (id, engine.get_record(id).expect("get").expect("record")))
            .collect();

        let err = NamespacePurgeError::partial(
            destroyed,
            StorageError::CorruptIndex("simulated mid-purge fault".into()),
        );

        assert!(err.is_partial());
        assert_eq!(err.destroyed.len(), 2);
        let message = err.to_string();
        assert!(message.contains("PARTIALLY destroyed"), "{message}");
        assert!(message.contains('2'), "the count must be in it: {message}");
        assert!(message.contains("Re-run"), "{message}");
    }

    /// Whether the database files a purge must never touch are all still
    /// where they belong.
    fn db_is_intact(db_path: &Path) -> bool {
        db_path.join("meta.db").exists()
            && db_path.join("edges.db").exists()
            && db_path.join("fulltext.dat").exists()
    }
    fn independent_join_corrupt_peer_plan(path: &Path, applied: bool) {
        use crate::storage::metadata::effects::{EFFECTS, PendingEffects};
        use redb::ReadableTable;
        let db = redb::Database::open(path.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(EFFECTS).unwrap();
            let (key, mut effect) = {
                let row = table.iter().unwrap().next().unwrap().unwrap();
                (
                    row.0.value().to_vec(),
                    crate::storage::control::decode::<PendingEffects>(row.1.value()).unwrap(),
                )
            };
            assert!(!effect.peer_counts.is_empty());
            effect.peer_counts[0].1 += 1;
            effect.canonical_applied = applied;
            table
                .insert(
                    key.as_slice(),
                    crate::storage::control::encode(&effect).unwrap().as_slice(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn independent_join_recovery_plan_preflight_preserves_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "review");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        engine
            .meta_store
            .fail_effect_application
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            engine.add_edge(a, b, EdgeType::Associative, 0.5, false, 10),
            Err(StorageError::CommittedEffectsPending { .. })
        ));
        drop(engine);
        independent_join_corrupt_peer_plan(dir.path(), false);
        let before = std::fs::read(dir.path().join("meta.db")).unwrap();
        let result = RedbStorageEngine::open(dir.path());
        assert!(result.is_err());
        eprintln!("corrupt-plan open rejected: {:?}", result.err());
        let after = std::fs::read(dir.path().join("meta.db")).unwrap();
        assert!(
            before == after,
            "malformed pending plan changed original metadata before rejection"
        );
    }

    #[test]
    fn independent_join_ledger_receipt_snapshot_and_pending_barrier() {
        use crate::storage::offline_snapshot::OfflineSnapshot;
        use crate::storage::schema_migration::{MigrationOptions, inspect_schema, migrate_schema};
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "review");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        engine
            .add_edge(a, b, EdgeType::Associative, 0.5, false, 10)
            .unwrap();
        engine.meta_store.acknowledge_rebuilt_effects().unwrap();
        let before = engine.control_state().unwrap();
        let effect = engine.delete_memory_with_edges(a, &[]).unwrap().unwrap();
        let ledger = engine.meta_store.removal_ledger_export().unwrap();
        let entry = ledger
            .entries
            .iter()
            .find(|e| e.memory_uuid == a.into_inner())
            .unwrap();
        assert_eq!(entry.operation_id, effect.receipt.operation_id);
        assert_eq!(entry.origin_lineage, effect.receipt.store_id);
        assert_eq!(entry.generation, effect.receipt.committed_generation);
        assert_eq!(
            entry.generation,
            before.committed_generation.next().unwrap()
        );
        let expected = engine.control_state().unwrap().generation_ref();
        drop(engine);
        let files = crate::storage::validation::canonical_files(dir.path()).unwrap();
        let bytes: Vec<_> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let target_root = tempfile::tempdir().unwrap();
        let target = target_root.path().join("schema");
        let opts = MigrationOptions {
            migration_now: 1234,
            expected_source: expected,
            attestations: vec![],
        };
        migrate_schema(dir.path(), &target, &opts).unwrap();
        let view = OfflineSnapshot::open(&target).unwrap();
        assert_eq!(view.control().generation_ref(), expected);
        assert!(view.record(a).unwrap().is_none());
        assert!(view.record(b).unwrap().is_some());
        let read = view.metadata.begin_read().unwrap();
        let copied =
            crate::storage::metadata::removal_ledger::read_export(&read, Some(view.control()))
                .unwrap()
                .unwrap();
        assert_eq!(copied, ledger);
        let pending = crate::storage::metadata::effects::validate_existing(&read).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].receipt, effect.receipt);
        assert!(pending[0].canonical_applied);
        drop(read);
        drop(view);
        assert!(inspect_schema(&target).is_ok());
        assert_eq!(
            files
                .iter()
                .map(|p| std::fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            bytes
        );
        // Flip only the pending status in a separate synthetic fixture: offline consumers must refuse replay.
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "review");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        engine
            .meta_store
            .fail_effect_application
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            engine
                .add_edge(a, b, EdgeType::Associative, 0.5, false, 10)
                .is_err()
        );
        drop(engine);
        let before = std::fs::read(dir.path().join("meta.db")).unwrap();
        assert!(inspect_schema(dir.path()).is_err());
        assert!(OfflineSnapshot::open(dir.path()).is_err());
        assert!(migrate_schema(dir.path(), &target_root.path().join("blocked"), &opts).is_err());
        assert!(!target_root.path().join("blocked").exists());
        assert_eq!(std::fs::read(dir.path().join("meta.db")).unwrap(), before);
    }

    #[test]
    fn preflight_preserves_valid_historical_applied_peer_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = open_engine(dir.path());
        let ns = make_namespace(&mut engine, "history");
        let a = add_memory(&mut engine, ns);
        let b = add_memory(&mut engine, ns);
        let c = add_memory(&mut engine, ns);
        engine
            .add_edge(a, b, EdgeType::Associative, 0.5, false, 10)
            .unwrap();
        engine
            .add_edge(a, c, EdgeType::Associative, 0.5, false, 11)
            .unwrap();
        let effects = engine.meta_store.pending_effects().unwrap();
        assert_eq!(effects.len(), 2);
        assert!(effects.iter().all(|effect| effect.canonical_applied));
        assert_eq!(effects[0].peer_counts[0].1, 1);
        assert_eq!(engine.get_record(a).unwrap().unwrap().edge_count, 2);
        let control = engine.control_state().unwrap();
        drop(engine);
        let reopened = open_engine(dir.path());
        assert_eq!(reopened.control_state().unwrap(), control);
        assert_eq!(reopened.get_record(a).unwrap().unwrap().edge_count, 2);
        assert_eq!(reopened.meta_store.pending_effects().unwrap().len(), 2);
    }
}
