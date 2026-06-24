//! Adapter implementations connecting cache warming to real subsystems.
//!
//! Each adapter wraps the concrete `RedbStorageEngine` (behind a lock)
//! and implements the corresponding warming trait from `warming.rs`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::model::{CachedRecord, EdgeType, MemoryId, NamespaceId};
// Import the real StorageEngine trait so its methods are in scope
// for RedbStorageEngine via the RwLockReadGuard deref.
use crate::storage::StorageEngine as _;
use crate::storage::RedbStorageEngine;

use super::warming::{EdgeStore, StorageEngine};

// ── StorageEngine adapter ──────────────────────────────────────────

/// Wraps `RedbStorageEngine` to satisfy the warming subsystem's
/// `StorageEngine` trait.
pub struct WarmingStorageAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl WarmingStorageAdapter {
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl StorageEngine for WarmingStorageAdapter {
    async fn load_record(
        &self,
        id: MemoryId,
    ) -> Result<Option<CachedRecord>, anyhow::Error> {
        let storage_r = self
            .storage
            .read()
            .map_err(|e| anyhow::anyhow!("storage lock poisoned: {e}"))?;
        match storage_r.get_record(id) {
            Ok(Some(disk)) => Ok(Some(CachedRecord::from(&disk))),
            Ok(None) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("{}", e)),
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
            Err(e) => Err(anyhow::anyhow!("{}", e)),
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
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}

// ── EdgeStore adapter ──────────────────────────────────────────────

/// Wraps `RedbStorageEngine` to satisfy the `EdgeStore` trait.
pub struct StorageEdgeAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl StorageEdgeAdapter {
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
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}
