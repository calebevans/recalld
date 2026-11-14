//! Additive control/receipt tables for the existing metadata layout.
//!
//! Control schema 1 is NOT canonical/record schema 2. It does not migrate
//! payloads, provide a whole-store read view, or certify derived readiness.
//! Until all writers/effects participate, this counter must not be advertised
//! as a public cursor/barrier generation. Existing metadata writes advance it.

use redb::{ReadableTable, TableDefinition, TableHandle, WriteTransaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::error::StorageError;
use crate::model::{MemoryId, NamespaceId};

pub(crate) const CONTROL: TableDefinition<&str, &[u8]> = TableDefinition::new("canonical_control");
pub(crate) const RECEIPTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("mutation_receipts");
pub(crate) const KEYS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mutation_keys");

/// Canonical decimal-string u63. JSON numbers, signs and leading zeros fail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Generation(u64);

impl Generation {
    pub const MAX: Self = Self(i64::MAX as u64);

    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn next(self) -> Result<Self, StorageError> {
        if self == Self::MAX {
            return Err(StorageError::GenerationExhausted);
        }
        Ok(Self(self.0 + 1))
    }
}

impl TryFrom<String> for Generation {
    type Error = StorageError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(StorageError::InvalidMutationContext(
                "noncanonical generation".into(),
            ));
        }
        match value.parse::<u64>() {
            Ok(n) if n <= Self::MAX.0 => Ok(Self(n)),
            _ => Err(StorageError::InvalidMutationContext(
                "generation outside u63 range".into(),
            )),
        }
    }
}

impl From<Generation> for String {
    fn from(value: Generation) -> Self {
        value.0.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationRef {
    pub store_id: Uuid,
    pub generation: Generation,
}

/// Versioned separately from the untouched DiskRecord v1 and payload layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlState {
    pub control_schema_version: u32,
    pub store_id: Uuid,
    pub committed_generation: Generation,
}

impl ControlState {
    pub fn generation_ref(&self) -> GenerationRef {
        GenerationRef {
            store_id: self.store_id,
            generation: self.committed_generation,
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
        let state: Self = decode(bytes)?;
        if state.control_schema_version != 1 || state.store_id.is_nil() {
            return Err(StorageError::InvalidControl(
                "unsupported version or nil store ID".into(),
            ));
        }
        Ok(state)
    }
}

/// Catalog cannot alias namespace 0 or a mutable namespace name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "namespaceId")]
pub enum MutationScope {
    Catalog,
    Namespace(NamespaceId),
}

impl MutationScope {
    fn bytes(self) -> Vec<u8> {
        match self {
            Self::Catalog => vec![0],
            Self::Namespace(id) => {
                let mut bytes = vec![1];
                bytes.extend(id.get().to_be_bytes());
                bytes
            }
        }
    }
}

/// Resolved immutable scope. A4 supplies normalized semantic fields; generated
/// IDs/times and provider output must not enter request identity.
#[derive(Debug, Clone)]
pub struct MutationContext {
    pub store_id: Option<Uuid>,
    pub scope: MutationScope,
    pub idempotency_key: Option<String>,
    /// Foundation-only metadata CAS; not public whole-store CAS until all
    /// canonical writers participate in the transaction/effects boundary.
    pub expected_generation: Option<Generation>,
    pub contract_version: u32,
}

impl MutationContext {
    pub fn unkeyed(namespace_id: NamespaceId) -> Self {
        Self {
            store_id: None,
            scope: MutationScope::Namespace(namespace_id),
            idempotency_key: None,
            expected_generation: None,
            contract_version: 1,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), StorageError> {
        if (self.idempotency_key.is_some() || self.expected_generation.is_some())
            && self.store_id.is_none()
        {
            return Err(StorageError::InvalidMutationContext(
                "storeId required with key or expectedGeneration".into(),
            ));
        }
        if let Some(key) = &self.idempotency_key
            && (key.is_empty()
                || key.len() > 128
                || !key.bytes().all(|b| (0x20..=0x7e).contains(&b)))
        {
            return Err(StorageError::InvalidMutationContext(
                "key must be 1–128 printable ASCII bytes".into(),
            ));
        }
        if !matches!(self.contract_version, 1 | 2)
            || matches!(self.scope, MutationScope::Namespace(id) if id.is_unset())
        {
            return Err(StorageError::InvalidMutationContext(
                "unsupported contract version or unset namespace".into(),
            ));
        }
        Ok(())
    }
}

/// Payload-free outcome; the timestamp is the originally committed value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum MutationOutcome {
    AccessRecorded {
        #[serde(rename = "memoryId")]
        memory_id: MemoryId,
        #[serde(rename = "accessedAt")]
        accessed_at: i64,
    },
}

/// Evidence of canonical metadata commit only, never of derived-index readiness.
/// Replay returns historical numbers/IDs; callers must revalidate content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteReceipt {
    pub store_id: Uuid,
    pub operation_id: Uuid,
    pub committed_generation: Generation,
    pub receipt_version: u32,
    pub outcome: MutationOutcome,
    pub replayed: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct StoredReceipt {
    pub request_hash_version: u32,
    pub request_hash: [u8; 32],
    pub receipt: WriteReceipt,
}

pub(crate) fn encode(value: &impl Serialize) -> Result<Vec<u8>, StorageError> {
    serde_json::to_vec(value).map_err(|e| StorageError::Serialize(e.to_string()))
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StorageError> {
    serde_json::from_slice(bytes).map_err(|e| StorageError::InvalidControl(e.to_string()))
}

/// One-time adoption at generation zero; a present but incomplete control schema
/// is corruption, not permission to mint a new retry lineage.
pub(crate) fn initialize(tx: &WriteTransaction) -> Result<(), StorageError> {
    let names: Vec<_> = tx.list_tables()?.map(|t| t.name().to_owned()).collect();
    let present =
        [CONTROL.name(), RECEIPTS.name(), KEYS.name()].map(|n| names.iter().any(|s| s == n));
    if present.iter().any(|p| *p) {
        if !present.iter().all(|p| *p) {
            return Err(StorageError::InvalidControl(
                "incomplete control tables".into(),
            ));
        }
        read_control(tx)?;
    } else {
        let state = ControlState {
            control_schema_version: 1,
            store_id: Uuid::new_v4(),
            committed_generation: Generation::default(),
        };
        tx.open_table(CONTROL)?
            .insert("state", encode(&state)?.as_slice())?;
        tx.open_table(RECEIPTS)?;
        tx.open_table(KEYS)?;
    }
    Ok(())
}

pub(crate) fn read_control(tx: &WriteTransaction) -> Result<ControlState, StorageError> {
    let table = tx.open_table(CONTROL)?;
    let value = table
        .get("state")?
        .ok_or_else(|| StorageError::InvalidControl("missing control state".into()))?;
    ControlState::decode(value.value())
}

/// Called inside each canonical metadata transaction, not checkpoint writes.
pub(crate) fn advance_generation(tx: &WriteTransaction) -> Result<ControlState, StorageError> {
    let mut state = read_control(tx)?;
    state.committed_generation = state.committed_generation.next()?;
    tx.open_table(CONTROL)?
        .insert("state", encode(&state)?.as_slice())?;
    Ok(state)
}

/// SHA-256 requestHashVersion 1: every component has a u64 BE byte-length.
/// Field order is semantic. Callers must normalize defaults/object keys first;
/// arrays preserve order and supplied float embeddings use their exact bytes.
pub(crate) fn request_hash(ctx: &MutationContext, operation: &str, fields: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    let expected = ctx.expected_generation.map(String::from);
    let scope = ctx.scope.bytes();
    let version = ctx.contract_version.to_be_bytes();
    for part in [
        operation.as_bytes(),
        version.as_slice(),
        scope.as_slice(),
        expected.as_deref().unwrap_or("").as_bytes(),
    ]
    .into_iter()
    .chain(fields.iter().copied())
    {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

pub(crate) fn scoped_key(store_id: Uuid, scope: MutationScope, suffix: &[u8]) -> Vec<u8> {
    let mut key = store_id.as_bytes().to_vec();
    key.extend(scope.bytes());
    key.extend(suffix);
    key
}

pub(crate) fn validate_receipt(bytes: &[u8]) -> Result<StoredReceipt, StorageError> {
    let stored: StoredReceipt = decode(bytes)?;
    if stored.request_hash_version != 1 || stored.receipt.receipt_version != 1 {
        return Err(StorageError::InvalidControl(
            "unsupported receipt/hash version".into(),
        ));
    }
    Ok(stored)
}

/// Lineage precedes replay; matching replay precedes CAS (including after purge).
pub(crate) fn replay_or_check(
    tx: &WriteTransaction,
    ctx: &MutationContext,
    hash: [u8; 32],
) -> Result<Option<WriteReceipt>, StorageError> {
    ctx.validate()?;
    let state = read_control(tx)?;
    if ctx.store_id.is_some_and(|id| id != state.store_id) {
        return Err(StorageError::StoreLineageMismatch);
    }
    if let Some(key) = &ctx.idempotency_key {
        let keys = tx.open_table(KEYS)?;
        if let Some(id) =
            keys.get(scoped_key(state.store_id, ctx.scope, key.as_bytes()).as_slice())?
        {
            let receipts = tx.open_table(RECEIPTS)?;
            let stored = receipts
                .get(id.value())?
                .ok_or_else(|| StorageError::InvalidControl("key without receipt".into()))?;
            let mut stored = validate_receipt(stored.value())?;
            if stored.request_hash != hash {
                return Err(StorageError::IdempotencyConflict);
            }
            stored.receipt.replayed = true;
            return Ok(Some(stored.receipt));
        }
    }
    if let Some(expected) = ctx.expected_generation
        && expected != state.committed_generation
    {
        return Err(StorageError::GenerationConflict {
            expected: expected.into(),
            actual: state.committed_generation.into(),
        });
    }
    Ok(None)
}

/// Caller commits this same transaction with its metadata changes. No payload
/// copies are retained; no receipt/key expiry or namespace-delete cleanup.
pub(crate) fn record_receipt(
    tx: &WriteTransaction,
    ctx: &MutationContext,
    hash: [u8; 32],
    outcome: MutationOutcome,
) -> Result<WriteReceipt, StorageError> {
    let state = advance_generation(tx)?;
    let receipt = WriteReceipt {
        store_id: state.store_id,
        operation_id: Uuid::new_v4(),
        committed_generation: state.committed_generation,
        receipt_version: 1,
        outcome,
        replayed: false,
    };
    let key = scoped_key(state.store_id, ctx.scope, receipt.operation_id.as_bytes());
    tx.open_table(RECEIPTS)?.insert(
        key.as_slice(),
        encode(&StoredReceipt {
            request_hash_version: 1,
            request_hash: hash,
            receipt: receipt.clone(),
        })?
        .as_slice(),
    )?;
    if let Some(client_key) = &ctx.idempotency_key {
        tx.open_table(KEYS)?.insert(
            scoped_key(state.store_id, ctx.scope, client_key.as_bytes()).as_slice(),
            key.as_slice(),
        )?;
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_canonical_decimal_u63_not_json_number() {
        for n in ["0", "1", "9007199254740993", "9223372036854775807"] {
            let json = format!("\"{n}\"");
            let value: Generation = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&value).unwrap(), json);
        }
        for json in [
            "0",
            "42",
            "null",
            "\"\"",
            "\"01\"",
            "\"+1\"",
            "\"-1\"",
            "\" 1\"",
            "\"1.0\"",
            "\"1e2\"",
            "\"9223372036854775808\"",
            "\"18446744073709551616\"",
        ] {
            assert!(serde_json::from_str::<Generation>(json).is_err(), "{json}");
        }
        assert!(matches!(
            Generation::MAX.next(),
            Err(StorageError::GenerationExhausted)
        ));
    }

    #[test]
    fn request_identity_is_length_delimited_ordered_and_operation_scoped() {
        let ctx = MutationContext::unkeyed(NamespaceId::new(1));
        let hash = request_hash(&ctx, "access", &[b"ab", b"c"]);
        assert_ne!(hash, request_hash(&ctx, "access", &[b"a", b"bc"]));
        assert_ne!(hash, request_hash(&ctx, "access", &[b"c", b"ab"]));
        assert_ne!(hash, request_hash(&ctx, "forget", &[b"ab", b"c"]));
        let mut changed = ctx.clone();
        changed.scope = MutationScope::Catalog;
        assert_ne!(hash, request_hash(&changed, "access", &[b"ab", b"c"]));
        changed = ctx.clone();
        changed.contract_version = 2;
        assert_ne!(hash, request_hash(&changed, "access", &[b"ab", b"c"]));
        changed = ctx.clone();
        changed.expected_generation = Some(Generation::default());
        assert_ne!(hash, request_hash(&changed, "access", &[b"ab", b"c"]));
        changed = ctx.clone();
        changed.idempotency_key = Some("transport-independent-key".into());
        assert_eq!(hash, request_hash(&changed, "access", &[b"ab", b"c"]));
        assert_ne!(
            scoped_key(Uuid::nil(), MutationScope::Catalog, b"x"),
            scoped_key(Uuid::nil(), ctx.scope, b"x")
        );
    }

    #[test]
    fn retry_context_requires_lineage_and_valid_key_scope_version() {
        let mut ctx = MutationContext::unkeyed(NamespaceId::new(1));
        ctx.expected_generation = Some(Generation::default());
        assert!(ctx.validate().is_err());
        ctx.expected_generation = None;
        ctx.idempotency_key = Some("key".into());
        assert!(ctx.validate().is_err());
        ctx.store_id = Some(Uuid::new_v4());
        assert!(ctx.validate().is_ok());
        for key in [String::new(), "x".repeat(129), "tab\t".into(), "é".into()] {
            ctx.idempotency_key = Some(key);
            assert!(ctx.validate().is_err());
        }
        ctx.idempotency_key = None;
        ctx.scope = MutationScope::Namespace(NamespaceId::UNSET);
        assert!(ctx.validate().is_err());
        ctx.scope = MutationScope::Catalog;
        ctx.contract_version = 3;
        assert!(ctx.validate().is_err());
    }
}
