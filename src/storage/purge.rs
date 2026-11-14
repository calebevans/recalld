//! Durable namespace deletion identity and payload-free progress.
use super::control::WriteReceipt;
use crate::model::{MemoryId, NamespaceId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PurgeStage {
    Deleting,
    /// Configuration is gone; the name remains leased until directory cleanup.
    Cleanup,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PurgeProgress {
    pub version: u32,
    pub store_id: Uuid,
    pub namespace_id: NamespaceId,
    pub name: String,
    pub start_receipt: WriteReceipt,
    pub last_receipt: WriteReceipt,
    pub sequence: u64,
    pub stage: PurgeStage,
    pub next_operation_id: Option<Uuid>,
    pub cursor: Option<MemoryId>,
    pub removed: u64,
    pub tombstones_removed: u64,
    pub edges_removed: u64,
}
