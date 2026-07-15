//! Adapter implementations connecting cache warming to real subsystems.
//!
//! Each adapter wraps the concrete `RedbStorageEngine` (behind a lock)
//! and implements the corresponding warming trait from `warming.rs`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::model::{CachedRecord, EdgeType, MemoryId, NamespaceId};
// Import the real StorageEngine trait so its methods are in scope
// for RedbStorageEngine via the RwLockReadGuard deref.
use super::warming::{EdgeStore, StorageEngine};
use crate::storage::RedbStorageEngine;
use crate::storage::StorageEngine as _;

// ── StorageEngine adapter ──────────────────────────────────────────

/// Wraps `RedbStorageEngine` to satisfy the warming subsystem's
/// `StorageEngine` trait.
pub struct WarmingStorageAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl WarmingStorageAdapter {
    /// Create a new adapter wrapping the given storage engine.
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl StorageEngine for WarmingStorageAdapter {
    async fn load_record(&self, id: MemoryId) -> Result<Option<CachedRecord>, anyhow::Error> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| anyhow::anyhow!("storage lock poisoned: {e}"))?;
        match storage_r.get_record(id) {
            Ok(Some(disk)) => Ok(Some(CachedRecord::from(&disk))),
            Ok(None) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e)),
        }
    }

    fn load_embedding(
        &self,
        vector_slot: u32,
        namespace_id: NamespaceId,
    ) -> Result<Vec<f32>, anyhow::Error> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| anyhow::anyhow!("storage lock poisoned: {e}"))?;
        match storage_r.get_vector(namespace_id, vector_slot) {
            Ok(Some(v)) => Ok(v),
            Ok(None) => Err(anyhow::anyhow!("vector not found")),
            Err(e) => Err(anyhow::Error::from(e)),
        }
    }

    async fn load_outgoing_edges(
        &self,
        id: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, anyhow::Error> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| anyhow::anyhow!("storage lock poisoned: {e}"))?;
        storage_r
            .get_outgoing_edges(id)
            .map_err(anyhow::Error::from)
    }
}

// ── EdgeStore adapter ──────────────────────────────────────────────

/// Wraps `RedbStorageEngine` to satisfy the `EdgeStore` trait.
pub struct StorageEdgeAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl StorageEdgeAdapter {
    /// Create a new adapter wrapping the given storage engine.
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl EdgeStore for StorageEdgeAdapter {
    async fn outgoing_edges(
        &self,
        id: MemoryId,
    ) -> Result<Vec<(MemoryId, EdgeType)>, anyhow::Error> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| anyhow::anyhow!("storage lock poisoned: {e}"))?;
        storage_r
            .get_outgoing_edges(id)
            .map_err(anyhow::Error::from)
    }
}
