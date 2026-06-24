//! Graph edge persistence backed by a dedicated redb instance (edges.db).
//!
//! Stores directed edges in forward and reverse B-tree indexes using
//! 33-byte composite keys. Separate from meta.db for independent
//! compaction, independent locking, and backup granularity.
//!
//! See CS-09 for the full specification.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use redb::{Database, ReadableTable, TableDefinition};

use crate::model::{EdgeType, MemoryId};
use crate::storage::error::StorageError;

// ═══════════════════════════════════════════════════════════════════════
// Table Definitions
// ═══════════════════════════════════════════════════════════════════════

/// Forward edge index.
/// Key:   (source_uuid, edge_type, target_uuid) = 33 bytes
/// Value: EdgeMetadata (weight, auto_created, created_at) = 13 bytes
const FORWARD_EDGES: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("edges_forward");

/// Reverse edge index for efficient incoming-edge lookups.
/// Key:   (target_uuid, edge_type, source_uuid) = 33 bytes
/// Value: () -- metadata lives only in the forward index.
const REVERSE_EDGES: TableDefinition<&[u8], ()> =
    TableDefinition::new("edges_reverse");

// ═══════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════

/// Composite key size: 16 (UUID) + 1 (EdgeType) + 16 (UUID) = 33 bytes.
const EDGE_KEY_SIZE: usize = 33;

/// Size of edge metadata value in the forward index.
const EDGE_VALUE_SIZE: usize = 13;

// ═══════════════════════════════════════════════════════════════════════
// Key Encoding / Decoding
// ═══════════════════════════════════════════════════════════════════════

/// Encode a forward-index key: (source, edge_type, target).
fn encode_forward_key(
    source: &MemoryId,
    edge_type: EdgeType,
    target: &MemoryId,
) -> [u8; EDGE_KEY_SIZE] {
    let mut key = [0u8; EDGE_KEY_SIZE];
    key[0..16].copy_from_slice(source.as_bytes());
    key[16] = edge_type.as_u8();
    key[17..33].copy_from_slice(target.as_bytes());
    key
}

/// Encode a reverse-index key: (target, edge_type, source).
fn encode_reverse_key(
    target: &MemoryId,
    edge_type: EdgeType,
    source: &MemoryId,
) -> [u8; EDGE_KEY_SIZE] {
    let mut key = [0u8; EDGE_KEY_SIZE];
    key[0..16].copy_from_slice(target.as_bytes());
    key[16] = edge_type.as_u8();
    key[17..33].copy_from_slice(source.as_bytes());
    key
}

/// Decode a forward-index key into (source, edge_type, target).
fn decode_forward_key(
    key: &[u8],
) -> Result<(MemoryId, EdgeType, MemoryId), StorageError> {
    if key.len() != EDGE_KEY_SIZE {
        return Err(StorageError::CorruptEdgeKey {
            expected: EDGE_KEY_SIZE,
            found: key.len(),
        });
    }
    let source = MemoryId::from_bytes(
        key[0..16].try_into().unwrap(),
    );
    let edge_type = EdgeType::from_u8(key[16])
        .ok_or(StorageError::InvalidEdgeType(key[16]))?;
    let target = MemoryId::from_bytes(
        key[17..33].try_into().unwrap(),
    );
    Ok((source, edge_type, target))
}

/// Decode a reverse-index key into (target, edge_type, source).
fn decode_reverse_key(
    key: &[u8],
) -> Result<(MemoryId, EdgeType, MemoryId), StorageError> {
    if key.len() != EDGE_KEY_SIZE {
        return Err(StorageError::CorruptEdgeKey {
            expected: EDGE_KEY_SIZE,
            found: key.len(),
        });
    }
    let target = MemoryId::from_bytes(
        key[0..16].try_into().unwrap(),
    );
    let edge_type = EdgeType::from_u8(key[16])
        .ok_or(StorageError::InvalidEdgeType(key[16]))?;
    let source = MemoryId::from_bytes(
        key[17..33].try_into().unwrap(),
    );
    Ok((target, edge_type, source))
}

// ═══════════════════════════════════════════════════════════════════════
// Value Encoding / Decoding
// ═══════════════════════════════════════════════════════════════════════

/// Encode edge metadata for the forward index value.
fn encode_edge_value(
    weight: f32,
    auto_created: bool,
    created_at: u64,
) -> [u8; EDGE_VALUE_SIZE] {
    let mut value = [0u8; EDGE_VALUE_SIZE];
    value[0..4].copy_from_slice(&weight.to_le_bytes());
    value[4] = auto_created as u8;
    value[5..13].copy_from_slice(&created_at.to_le_bytes());
    value
}

/// Decode edge metadata from a forward index value.
fn decode_edge_value(
    value: &[u8],
) -> Result<(f32, bool, u64), StorageError> {
    if value.len() < EDGE_VALUE_SIZE {
        return Err(StorageError::CorruptEdgeValue {
            expected: EDGE_VALUE_SIZE,
            found: value.len(),
        });
    }
    let weight =
        f32::from_le_bytes(value[0..4].try_into().unwrap());
    let auto_created = value[4] != 0;
    let created_at =
        u64::from_le_bytes(value[5..13].try_into().unwrap());
    Ok((weight, auto_created, created_at))
}

// ═══════════════════════════════════════════════════════════════════════
// PersistedEdge
// ═══════════════════════════════════════════════════════════════════════

/// An edge record as stored in edges.db.
/// Used for disk I/O -- the in-memory graph uses `GraphEdge` (CS-10).
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedEdge {
    pub source: MemoryId,
    pub target: MemoryId,
    pub edge_type: EdgeType,
    pub weight: f32,
    pub auto_created: bool,
    /// Milliseconds since Unix epoch.
    pub created_at: u64,
}

impl PersistedEdge {
    /// Construct from a decoded forward key and value.
    fn from_forward(
        key: &[u8],
        value: &[u8],
    ) -> Result<Self, StorageError> {
        let (source, edge_type, target) = decode_forward_key(key)?;
        let (weight, auto_created, created_at) =
            decode_edge_value(value)?;
        Ok(PersistedEdge {
            source,
            target,
            edge_type,
            weight,
            auto_created,
            created_at,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
// EdgeStore
// ═══════════════════════════════════════════════════════════════════════

/// Persistent edge storage backed by a dedicated redb database.
///
/// Thread safety: NOT internally synchronized. The caller
/// (`RedbStorageEngine`) must hold an appropriate lock before
/// calling write methods.
pub struct EdgeStore {
    db: Database,
    path: PathBuf,
}

impl EdgeStore {
    /// Open or create the edge store at the given path.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let db = Database::create(path)?;

        // Ensure tables exist.
        {
            let write_txn = db.begin_write()?;
            let _ = write_txn.open_table(FORWARD_EDGES)?;
            let _ = write_txn.open_table(REVERSE_EDGES)?;
            write_txn.commit()?;
        }

        tracing::info!("EdgeStore opened at {}", path.display());

        Ok(EdgeStore {
            db,
            path: path.to_path_buf(),
        })
    }

    /// Persist a single edge to both forward and reverse indexes.
    ///
    /// If an edge with the same (source, edge_type, target) already
    /// exists, it is silently overwritten (upsert semantics).
    pub fn add_edge(
        &self,
        source: MemoryId,
        target: MemoryId,
        edge_type: EdgeType,
        weight: f32,
        auto_created: bool,
        created_at: u64,
    ) -> Result<(), StorageError> {
        let fwd_key =
            encode_forward_key(&source, edge_type, &target);
        let rev_key =
            encode_reverse_key(&target, edge_type, &source);
        let value =
            encode_edge_value(weight, auto_created, created_at);

        let write_txn = self.db.begin_write()?;
        {
            let mut fwd = write_txn.open_table(FORWARD_EDGES)?;
            let mut rev = write_txn.open_table(REVERSE_EDGES)?;

            fwd.insert(fwd_key.as_slice(), value.as_slice())?;
            rev.insert(rev_key.as_slice(), &())?;
        }
        write_txn.commit()?;

        Ok(())
    }

    /// Remove a single edge from both forward and reverse indexes.
    ///
    /// Returns `Ok(true)` if the edge existed and was removed,
    /// `Ok(false)` if the edge was not found.
    pub fn remove_edge(
        &self,
        source: MemoryId,
        target: MemoryId,
        edge_type: EdgeType,
    ) -> Result<bool, StorageError> {
        let fwd_key =
            encode_forward_key(&source, edge_type, &target);
        let rev_key =
            encode_reverse_key(&target, edge_type, &source);

        let write_txn = self.db.begin_write()?;
        let removed;
        {
            let mut fwd = write_txn.open_table(FORWARD_EDGES)?;
            let mut rev = write_txn.open_table(REVERSE_EDGES)?;

            removed = fwd.remove(fwd_key.as_slice())?.is_some();
            rev.remove(rev_key.as_slice())?;
        }
        write_txn.commit()?;

        Ok(removed)
    }

    /// Return all outgoing edges from `source` (any edge type).
    ///
    /// Prefix scan on the first 16 bytes of the forward index key.
    pub fn get_outgoing(
        &self,
        source: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(FORWARD_EDGES)?;

        let mut start = [0u8; EDGE_KEY_SIZE];
        start[0..16].copy_from_slice(source.as_bytes());

        let mut end = [0u8; EDGE_KEY_SIZE];
        end[0..16].copy_from_slice(source.as_bytes());
        end[16..33].fill(0xFF);

        let mut results = Vec::new();
        for entry in table.range(start.as_slice()..=end.as_slice())? {
            let (key_guard, _value_guard) = entry?;
            let key = key_guard.value();
            let (_source, edge_type, target) =
                decode_forward_key(key)?;
            results.push((target, edge_type));
        }

        Ok(results)
    }

    /// Return all incoming edges to `target` (any edge type).
    ///
    /// Scans the reverse index.
    pub fn get_incoming(
        &self,
        target: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let rev_table = read_txn.open_table(REVERSE_EDGES)?;

        let mut start = [0u8; EDGE_KEY_SIZE];
        start[0..16].copy_from_slice(target.as_bytes());

        let mut end = [0u8; EDGE_KEY_SIZE];
        end[0..16].copy_from_slice(target.as_bytes());
        end[16..33].fill(0xFF);

        let mut results = Vec::new();
        for entry in rev_table.range(start.as_slice()..=end.as_slice())? {
            let (key_guard, _) = entry?;
            let key = key_guard.value();
            let (_target, edge_type, source) =
                decode_reverse_key(key)?;
            results.push((source, edge_type));
        }

        Ok(results)
    }

    /// Return all outgoing edges of a specific type from `source`.
    ///
    /// Uses a 17-byte prefix: [source_uuid: 16][edge_type: 1].
    pub fn get_outgoing_of_type(
        &self,
        source: MemoryId,
        edge_type: EdgeType,
    ) -> Result<Vec<MemoryId>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(FORWARD_EDGES)?;

        let mut start = [0u8; EDGE_KEY_SIZE];
        start[0..16].copy_from_slice(source.as_bytes());
        start[16] = edge_type.as_u8();

        let mut end = [0u8; EDGE_KEY_SIZE];
        end[0..16].copy_from_slice(source.as_bytes());
        end[16] = edge_type.as_u8();
        end[17..33].fill(0xFF);

        let mut targets = Vec::new();
        for entry in table.range(start.as_slice()..=end.as_slice())? {
            let (key_guard, _) = entry?;
            let key = key_guard.value();
            let target = MemoryId::from_bytes(
                key[17..33].try_into().unwrap(),
            );
            targets.push(target);
        }

        Ok(targets)
    }

    /// Remove all edges where `memory_id` appears as source or target.
    ///
    /// Returns the total number of edges removed.
    pub fn remove_all_edges(
        &self,
        memory_id: MemoryId,
    ) -> Result<usize, StorageError> {
        let outgoing = self.get_outgoing_full(memory_id)?;
        let incoming = self.get_incoming_full(memory_id)?;

        if outgoing.is_empty() && incoming.is_empty() {
            return Ok(0);
        }

        // Deduplicate: a self-loop appears in both outgoing and
        // incoming.
        let mut to_remove: Vec<(MemoryId, EdgeType, MemoryId)> =
            Vec::with_capacity(outgoing.len() + incoming.len());

        for edge in &outgoing {
            to_remove
                .push((edge.source, edge.edge_type, edge.target));
        }
        for edge in &incoming {
            let triple =
                (edge.source, edge.edge_type, edge.target);
            if !to_remove.contains(&triple) {
                to_remove.push(triple);
            }
        }

        // Batch removal in a single transaction.
        let write_txn = self.db.begin_write()?;
        {
            let mut fwd = write_txn.open_table(FORWARD_EDGES)?;
            let mut rev = write_txn.open_table(REVERSE_EDGES)?;

            for &(source, edge_type, target) in &to_remove {
                let fwd_key =
                    encode_forward_key(&source, edge_type, &target);
                let rev_key =
                    encode_reverse_key(&target, edge_type, &source);
                fwd.remove(fwd_key.as_slice())?;
                rev.remove(rev_key.as_slice())?;
            }
        }
        write_txn.commit()?;

        let count = to_remove.len();
        tracing::debug!(
            memory_id = %memory_id,
            removed = count,
            "Removed all edges for memory"
        );

        Ok(count)
    }

    /// Persist multiple edges atomically in a single transaction.
    pub fn batch_add_edges(
        &self,
        edges: &[PersistedEdge],
    ) -> Result<(), StorageError> {
        if edges.is_empty() {
            return Ok(());
        }

        let write_txn = self.db.begin_write()?;
        {
            let mut fwd = write_txn.open_table(FORWARD_EDGES)?;
            let mut rev = write_txn.open_table(REVERSE_EDGES)?;

            for edge in edges {
                let fwd_key = encode_forward_key(
                    &edge.source,
                    edge.edge_type,
                    &edge.target,
                );
                let rev_key = encode_reverse_key(
                    &edge.target,
                    edge.edge_type,
                    &edge.source,
                );
                let value = encode_edge_value(
                    edge.weight,
                    edge.auto_created,
                    edge.created_at,
                );

                fwd.insert(fwd_key.as_slice(), value.as_slice())?;
                rev.insert(rev_key.as_slice(), &())?;
            }
        }
        write_txn.commit()?;

        tracing::debug!(count = edges.len(), "Batch inserted edges");

        Ok(())
    }

    /// Remove multiple edges atomically in a single transaction.
    pub fn batch_remove_edges(
        &self,
        edges: &[PersistedEdge],
    ) -> Result<(), StorageError> {
        if edges.is_empty() {
            return Ok(());
        }

        let write_txn = self.db.begin_write()?;
        {
            let mut fwd = write_txn.open_table(FORWARD_EDGES)?;
            let mut rev = write_txn.open_table(REVERSE_EDGES)?;

            for edge in edges {
                let fwd_key = encode_forward_key(
                    &edge.source,
                    edge.edge_type,
                    &edge.target,
                );
                let rev_key = encode_reverse_key(
                    &edge.target,
                    edge.edge_type,
                    &edge.source,
                );
                fwd.remove(fwd_key.as_slice())?;
                rev.remove(rev_key.as_slice())?;
            }
        }
        write_txn.commit()?;

        tracing::debug!(count = edges.len(), "Batch removed edges");

        Ok(())
    }

    /// Load all edges from the forward index.
    /// Used for startup graph loading.
    pub fn load_all_edges(
        &self,
    ) -> Result<Vec<PersistedEdge>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(FORWARD_EDGES)?;

        let mut edges = Vec::new();
        for entry in table.iter()? {
            let (key_guard, value_guard) = entry?;
            let edge = PersistedEdge::from_forward(
                key_guard.value(),
                value_guard.value(),
            )?;
            edges.push(edge);
        }

        tracing::info!(
            count = edges.len(),
            "Loaded all edges from edges.db"
        );

        Ok(edges)
    }

    /// Return the path to the edges.db file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ── Full-Edge Retrieval Helpers (internal) ───────────────────────────

impl EdgeStore {
    /// Return full `PersistedEdge` records for all outgoing edges.
    fn get_outgoing_full(
        &self,
        source: MemoryId,
    ) -> Result<Vec<PersistedEdge>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(FORWARD_EDGES)?;

        let mut start = [0u8; EDGE_KEY_SIZE];
        start[0..16].copy_from_slice(source.as_bytes());

        let mut end = [0u8; EDGE_KEY_SIZE];
        end[0..16].copy_from_slice(source.as_bytes());
        end[16..33].fill(0xFF);

        let mut edges = Vec::new();
        for entry in table.range(start.as_slice()..=end.as_slice())? {
            let (key_guard, value_guard) = entry?;
            let edge = PersistedEdge::from_forward(
                key_guard.value(),
                value_guard.value(),
            )?;
            edges.push(edge);
        }

        Ok(edges)
    }

    /// Return full `PersistedEdge` records for all incoming edges.
    /// Looks up metadata from the forward index for each reverse key.
    fn get_incoming_full(
        &self,
        target: MemoryId,
    ) -> Result<Vec<PersistedEdge>, StorageError> {
        let read_txn = self.db.begin_read()?;
        let fwd_table = read_txn.open_table(FORWARD_EDGES)?;
        let rev_table = read_txn.open_table(REVERSE_EDGES)?;

        let mut start = [0u8; EDGE_KEY_SIZE];
        start[0..16].copy_from_slice(target.as_bytes());

        let mut end = [0u8; EDGE_KEY_SIZE];
        end[0..16].copy_from_slice(target.as_bytes());
        end[16..33].fill(0xFF);

        let mut edges = Vec::new();
        for entry in
            rev_table.range(start.as_slice()..=end.as_slice())?
        {
            let (rev_key_guard, _) = entry?;
            let rev_key = rev_key_guard.value();

            // Reconstruct forward key from reverse key fields.
            let target_bytes = &rev_key[0..16];
            let edge_type_byte = rev_key[16];
            let source_bytes = &rev_key[17..33];

            let mut fwd_key = [0u8; EDGE_KEY_SIZE];
            fwd_key[0..16].copy_from_slice(source_bytes);
            fwd_key[16] = edge_type_byte;
            fwd_key[17..33].copy_from_slice(target_bytes);

            // Fetch metadata from the forward index.
            if let Some(value_guard) =
                fwd_table.get(fwd_key.as_slice())?
            {
                let edge = PersistedEdge::from_forward(
                    &fwd_key,
                    value_guard.value(),
                )?;
                edges.push(edge);
            }
            // If the forward entry is missing, the indexes are
            // inconsistent. Orphan cleanup will fix this on
            // the next startup.
        }

        Ok(edges)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Orphan Cleanup (free function)
// ═══════════════════════════════════════════════════════════════════════

/// Scan edges.db for edges referencing memory IDs not in `known_ids`.
/// Remove any orphaned edges found.
///
/// Called during `RedbStorageEngine::startup_validation()`.
pub fn cleanup_orphaned_edges(
    edge_store: &EdgeStore,
    known_ids: &HashSet<MemoryId>,
) -> Result<usize, StorageError> {
    let read_txn = edge_store.db.begin_read()?;
    let table = read_txn.open_table(FORWARD_EDGES)?;

    let mut orphans: Vec<PersistedEdge> = Vec::new();

    for entry in table.iter()? {
        let (key_guard, value_guard) = entry?;
        let key = key_guard.value();

        let source = MemoryId::from_bytes(
            key[0..16].try_into().unwrap(),
        );
        let target = MemoryId::from_bytes(
            key[17..33].try_into().unwrap(),
        );

        if !known_ids.contains(&source)
            || !known_ids.contains(&target)
        {
            if let Ok(edge) = PersistedEdge::from_forward(
                key,
                value_guard.value(),
            ) {
                orphans.push(edge);
            }
        }
    }

    // Must drop the read transaction before writing.
    drop(table);
    drop(read_txn);

    let count = orphans.len();
    if count > 0 {
        edge_store.batch_remove_edges(&orphans)?;
        tracing::warn!(
            removed = count,
            "Removed orphaned edges during startup"
        );
    } else {
        tracing::debug!("No orphaned edges found");
    }

    Ok(count)
}
