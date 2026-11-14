//! Payload-free catalog batch manifests. Canonical child receipts remain in the
//! ordinary receipt table; a manifest reserves identities, never another journal.
use super::control::{MutationContext, MutationScope, WriteReceipt};
use crate::model::{MemoryId, NamespaceId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BatchKind {
    Insert,
    Forget,
}

/// Bounded diagnostics only. Never persist rejected input or provider messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BatchError {
    pub code: String,
    pub field: Option<String>,
}
impl BatchError {
    pub(crate) fn new(code: &str) -> Self {
        Self {
            code: code.into(),
            field: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BatchDefaults {
    pub namespace_id: NamespaceId,
    pub namespace_name: String,
    pub namespace_policy_hash: [u8; 32],
    pub initial_stability_bits: u32,
    pub embedding_dim: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum BatchItemState {
    NotAttempted,
    Ok { receipt: WriteReceipt },
    Error { error: BatchError },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BatchItem {
    pub index: u32,
    pub request_hash: [u8; 32],
    pub operation_id: Uuid,
    pub defaults: Option<BatchDefaults>,
    pub canonical_hash: Option<[u8; 32]>,
    pub resolution_error: Option<BatchError>,
    pub state: BatchItemState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BatchProgress {
    pub version: u32,
    pub kind: BatchKind,
    pub caller_hash: [u8; 32],
    /// A real write even when every item was invalid. Do not confuse this with
    /// successful child count or a single atomic batch generation.
    pub reservation_receipt: WriteReceipt,
    /// Later final-error bookkeeping also advances canonical generation.
    pub bookkeeping_receipts: Vec<WriteReceipt>,
    pub items: Vec<BatchItem>,
}

/// Only trusted adapter preparation creates these ephemeral plans. Payloads
/// are required again on resume; none of this enum is stored in a manifest.
#[derive(Clone)]
pub(crate) enum BatchItemPlan {
    Insert {
        request_hash: [u8; 32],
        namespace: String,
        input: super::insertion::InsertMemory,
    },
    Forget {
        request_hash: [u8; 32],
        id: MemoryId,
    },
    Invalid {
        request_hash: [u8; 32],
        error: BatchError,
    },
}
impl BatchItemPlan {
    pub(crate) fn request_hash(&self) -> [u8; 32] {
        match self {
            Self::Insert { request_hash, .. }
            | Self::Forget { request_hash, .. }
            | Self::Invalid { request_hash, .. } => *request_hash,
        }
    }
}

/// Private capability: final transaction always reloads/validates its manifest.
/// Neither caller nor daemon can supply arbitrary canonical operation IDs.
#[derive(Debug, Clone)]
pub(crate) struct BatchChild {
    pub(super) store_id: Uuid,
    pub(super) batch_operation_id: Uuid,
    pub(super) index: u32,
    pub(super) request_hash: [u8; 32],
    pub(super) operation_id: Uuid,
    pub(super) defaults: BatchDefaults,
    pub(super) canonical_hash: [u8; 32],
}
impl BatchChild {
    pub(crate) fn context(&self) -> MutationContext {
        MutationContext {
            store_id: Some(self.store_id),
            scope: MutationScope::Namespace(self.defaults.namespace_id),
            idempotency_key: None,
            expected_generation: None,
            contract_version: 1,
        }
    }
    pub(crate) fn namespace(&self) -> NamespaceId {
        self.defaults.namespace_id
    }
}

impl BatchProgress {
    pub(crate) fn child(&self, index: usize) -> Option<BatchChild> {
        let item = self.items.get(index)?;
        Some(BatchChild {
            store_id: self.reservation_receipt.store_id,
            batch_operation_id: self.reservation_receipt.operation_id,
            index: item.index,
            request_hash: item.request_hash,
            operation_id: item.operation_id,
            defaults: item.defaults.clone()?,
            canonical_hash: item.canonical_hash?,
        })
    }
    pub fn unresolved(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.state, BatchItemState::NotAttempted))
            .count()
    }
}

/// Structured transport result. This is a live response, never a durable row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchOutcome {
    pub result_version: u32,
    pub batch_operation_id: Uuid,
    pub store_id: Uuid,
    pub reservation_state: BatchReservationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reservation_receipt: Option<WriteReceipt>,
    pub bookkeeping_receipts: Vec<WriteReceipt>,
    pub unknown_bookkeeping_operations: Vec<Uuid>,
    pub items: Vec<BatchItemOutcome>,
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub unresolved: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItemOutcome {
    pub index: u32,
    pub status: BatchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace_id: Option<NamespaceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<WriteReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchError>,
    pub ready: bool,
    pub retryable: bool,
    /// Current response-only compatibility result. Never retained in receipts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BatchStatus {
    Ok,
    Error,
    NotAttempted,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BatchReservationState {
    Committed,
    Unknown,
}

impl BatchOutcome {
    pub fn all_ready(&self) -> bool {
        self.reservation_state == BatchReservationState::Committed
            && self.unresolved == 0
            && self.failed == 0
            && self.items.iter().all(|item| item.ready)
    }
    pub(crate) fn recount(&mut self) {
        self.total = self.items.len();
        self.succeeded = self
            .items
            .iter()
            .filter(|i| i.status == BatchStatus::Ok)
            .count();
        self.failed = self
            .items
            .iter()
            .filter(|i| i.status == BatchStatus::Error)
            .count();
        self.unresolved = self.total - self.succeeded - self.failed;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BatchInput {
    /// This negotiates only the additive batch envelope, not lifecycle/API v2.
    pub result_version: u32,
    pub kind: BatchKind,
    pub items: Vec<serde_json::Value>,
    pub default_namespace: String,
    #[serde(default)]
    pub store_id: Option<Uuid>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Reconcile an existing reservation (including an originally unkeyed
    /// call). This never selects an identity for a new write.
    #[serde(default)]
    pub batch_operation_id: Option<Uuid>,
}
impl BatchInput {
    pub(crate) fn context(&self) -> MutationContext {
        MutationContext {
            store_id: self.store_id,
            scope: MutationScope::Catalog,
            idempotency_key: self.idempotency_key.clone(),
            expected_generation: None,
            contract_version: 1,
        }
    }
}

/// Request identity before namespace/default resolution. Arrays remain ordered.
pub(crate) fn caller_hash(
    kind: BatchKind,
    hashes: &[[u8; 32]],
) -> Result<[u8; 32], super::StorageError> {
    let ctx = MutationContext {
        scope: MutationScope::Catalog,
        ..MutationContext::unkeyed(NamespaceId::new(1))
    };
    Ok(super::control::request_hash(
        &ctx,
        "batch_caller_v1",
        &[&super::control::encode(&(kind, hashes))?],
    ))
}
pub(crate) fn resolution_hash(items: &[BatchItem]) -> Result<[u8; 32], super::StorageError> {
    let rows: Vec<_> = items
        .iter()
        .map(|i| {
            (
                i.index,
                i.request_hash,
                i.operation_id,
                &i.defaults,
                i.canonical_hash,
                &i.resolution_error,
            )
        })
        .collect();
    let ctx = MutationContext {
        scope: MutationScope::Catalog,
        ..MutationContext::unkeyed(NamespaceId::new(1))
    };
    Ok(super::control::request_hash(
        &ctx,
        "batch_resolved_v1",
        &[&super::control::encode(&rows)?],
    ))
}
