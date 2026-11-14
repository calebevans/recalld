//! Explicit native record intent and payload-free proof. Canonical record codecs
//! remain in model::schema; hashes never substitute for validating retained bytes.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

use super::StorageError;
use super::control::{Generation, GenerationRef, MutationContext, MutationOutcome, WriteReceipt};
use super::insertion::InsertMemory;
use super::native_lifecycle::{
    CapturedLifecycle, LifecycleScope, LifecycleState, history_hash, invalid,
};
use crate::model::schema::{
    EmbeddingBinding, EmbeddingProfile, LabelProvenance, LifecycleVersion, StructuredLabels,
    TimestampOrigin, VectorReference,
};
use crate::model::{AccessEvent, DecayPhase, DiskRecord, MemoryId, NamespaceConfig};

/// Original caller choices. Structured labels are persisted verbatim, never
/// normalized into compatibility tags. Provider output is supplied separately.
#[derive(Debug, Clone)]
pub struct NativeInsert {
    pub memory: InsertMemory,
    pub observed_at: Option<i64>,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
    /// Required for a supplied embedding. Generated provenance comes from the
    /// actual GeneratedEmbedding result and is not caller retry identity.
    pub embedding_profile: Option<EmbeddingProfile>,
}
impl NativeInsert {
    pub(crate) fn labels(&self) -> StructuredLabels {
        StructuredLabels {
            entities: self.entities.clone(),
            topics: self.topics.clone(),
            emotions: self.emotions.clone(),
            provenance: LabelProvenance::NativeV2,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), StorageError> {
        use crate::model::constants::*;
        crate::model::validation::validate_memory_input(crate::model::validation::MemoryInputRef {
            summary: &self.memory.summary,
            full_text: self.memory.full_text.as_deref(),
            tags: &self
                .memory
                .tags
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            entities: &[],
            topics: &[],
            emotions: &[],
        })
        .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
        for (name, values, max) in [
            ("entities", &self.entities, MAX_ENTITIES),
            ("topics", &self.topics, MAX_TOPICS),
            ("emotions", &self.emotions, MAX_EMOTIONS),
        ] {
            crate::model::validation::validate_count(name, values.len(), max)
                .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
            crate::model::validation::validate_item_lens(name, values, LABEL_MAX_BYTES)
                .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
        }
        self.labels()
            .validate()
            .map_err(StorageError::InvalidMutation)?;
        if self.memory.full_text.as_deref() == Some("") {
            return Err(StorageError::EmptyText);
        }
        if self.memory.embedding.is_some() != self.embedding_profile.is_some() {
            return Err(StorageError::InvalidMutation(
                "supplied vector and explicit profile must be provided together".into(),
            ));
        }
        for time in [self.observed_at, self.memory.created_at] {
            if time.is_some_and(|t| chrono::DateTime::from_timestamp_millis(t).is_none()) {
                return Err(StorageError::InvalidMutation(
                    "unrepresentable native creation/observation time".into(),
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn request_hash(
        &self,
        ctx: &MutationContext,
        stability: f32,
        difficulty: f32,
        dimensions: u32,
        profile_id: &str,
    ) -> Result<[u8; 32], StorageError> {
        // Reuse exact original caller float bits/default expansion. The v2
        // operation tag and additional fields keep its identity distinct.
        let original = self.memory.request_hash(ctx, stability, dimensions)?;
        let supplied_profile = self
            .embedding_profile
            .as_ref()
            .map(|p| p.profile_id())
            .transpose()
            .map_err(StorageError::InvalidMutation)?;
        let extra = super::control::encode(&(
            self.observed_at,
            &self.entities,
            &self.topics,
            &self.emotions,
            supplied_profile,
            difficulty.to_bits(),
            profile_id,
        ))?;
        Ok(super::control::request_hash(
            ctx,
            "memory_insert_v2",
            &[&original, &extra],
        ))
    }
}

#[derive(Debug, Clone)]
pub struct NativeInsertIntent {
    pub(crate) scope: LifecycleScope,
    pub(crate) original: NativeInsert,
    pub(crate) namespace: NamespaceConfig,
    pub(crate) capture: CapturedLifecycle,
}
#[derive(Debug, Clone)]
pub struct NativeTombstoneIntent {
    pub(crate) scope: LifecycleScope,
    pub(crate) memory_id: MemoryId,
    pub(crate) capture: CapturedLifecycle,
}
impl NativeInsertIntent {
    pub fn capture_generation(&self) -> GenerationRef {
        self.capture.generation
    }
}
impl NativeTombstoneIntent {
    pub fn capture_generation(&self) -> GenerationRef {
        self.capture.generation
    }
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn hash_value(v: &impl Serialize) -> Result<[u8; 32], StorageError> {
    Ok(hash_bytes(&super::control::encode(v)?))
}
pub(crate) fn profile_id(namespace: &NamespaceConfig) -> Option<String> {
    match &namespace.schema.as_ref()?.embedding_binding {
        EmbeddingBinding::Bound { profile_id, .. } => Some(profile_id.clone()),
        EmbeddingBinding::UnknownLegacy => None,
    }
}

/// Component commitments permit validating retained fields after partial loss.
/// Missing text hash means unknown legacy provenance when numeric.has_text is
/// true, or no retained text otherwise. Native creation always has a known hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContentProof {
    pub summary_sha256: [u8; 32],
    pub tags_sha256: [u8; 32],
    pub labels_sha256: [u8; 32],
    pub label_provenance: LabelProvenance,
    pub extensions_sha256: [u8; 32],
    pub text_sha256: Option<[u8; 32]>,
    pub vector: Option<VectorReference>,
    pub profile_id: Option<String>,
}
impl ContentProof {
    pub(crate) fn capture(
        record: &DiskRecord,
        namespace: &NamespaceConfig,
        text_hash: Option<[u8; 32]>,
    ) -> Result<Self, StorageError> {
        let schema = record
            .schema
            .as_ref()
            .ok_or_else(|| invalid("native record schema missing"))?;
        Ok(Self {
            summary_sha256: hash_bytes(record.summary.as_bytes()),
            tags_sha256: hash_value(&record.tags)?,
            labels_sha256: hash_value(&schema.labels)?,
            label_provenance: schema.labels.provenance,
            extensions_sha256: hash_value(&schema.extensions)?,
            // Migrated legacy rows have no immutable native creation commitment.
            // Keep that provenance unknown across peer-only edge mutations.
            text_sha256: if record.text_length == 0
                || schema.lifecycle.timestamp_origin == TimestampOrigin::LegacyInferred
            {
                None
            } else {
                text_hash
            },
            vector: schema.vector.clone(),
            profile_id: profile_id(namespace),
        })
    }
    pub(crate) fn erased(&self) -> Result<Self, StorageError> {
        if self.extensions_sha256 != hash_value(&BTreeMap::<String, serde_json::Value>::new())? {
            return Err(invalid(
                "native forget needs a consumer for unknown record extensions",
            ));
        }
        let mut result = self.clone();
        result.summary_sha256 = hash_bytes(b"");
        result.tags_sha256 = hash_value(&Vec::<crate::model::Tag>::new())?;
        result.labels_sha256 = hash_value(&StructuredLabels {
            entities: vec![],
            topics: vec![],
            emotions: vec![],
            provenance: self.label_provenance,
        })?;
        result.text_sha256 = None;
        result.vector = None;
        Ok(result)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeRecordState {
    pub lifecycle: LifecycleState,
    pub edge_count: u16,
    pub history_sha256: [u8; 32],
    pub content: ContentProof,
}
impl NativeRecordState {
    pub(crate) fn capture(
        r: &DiskRecord,
        ns: &NamespaceConfig,
        text_hash: Option<[u8; 32]>,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            lifecycle: LifecycleState::capture(r)?,
            edge_count: r.edge_count,
            history_sha256: history_hash(&r.access_history)?,
            content: ContentProof::capture(r, ns, text_hash)?,
        })
    }
    pub(crate) fn matches_record(&self, r: &DiskRecord) -> Result<bool, StorageError> {
        let schema = r
            .schema
            .as_ref()
            .ok_or_else(|| invalid("native schema missing"))?;
        Ok(self.lifecycle == LifecycleState::capture(r)?
            && self.edge_count == r.edge_count
            && self.history_sha256 == history_hash(&r.access_history)?
            && self.content.summary_sha256 == hash_bytes(r.summary.as_bytes())
            && self.content.tags_sha256 == hash_value(&r.tags)?
            && self.content.labels_sha256 == hash_value(&schema.labels)?
            && self.content.extensions_sha256 == hash_value(&schema.extensions)?
            && self.content.label_provenance == schema.labels.provenance
            && self.content.vector == schema.vector)
    }
    pub(crate) fn tombstone(&self, now: i64) -> Result<Self, StorageError> {
        if self.lifecycle.phase == DecayPhase::Tombstone {
            return Ok(self.clone());
        }
        let mut result = self.clone();
        let time = now.max(self.lifecycle.lifecycle.lifecycle_updated_at);
        result.lifecycle.phase = DecayPhase::Tombstone;
        result.lifecycle.strength = 0_f32.try_into().map_err(StorageError::InvalidControl)?;
        result.lifecycle.decay_strength = 0_f32.try_into().map_err(StorageError::InvalidControl)?;
        result.lifecycle.has_summary = false;
        result.lifecycle.has_text = false;
        result.lifecycle.lifecycle.lifecycle_updated_at = time;
        result.lifecycle.lifecycle.phase_changed_at = time;
        result.lifecycle.lifecycle.record_generation =
            result.lifecycle.lifecycle.record_generation.next()?;
        result.content = self.content.erased()?;
        Ok(result)
    }
}

/// Validation-only view; it never manufactures strings or restores payloads.
pub(crate) struct ProofRecord {
    pub id: MemoryId,
    pub namespace: u32,
    pub state: NativeRecordState,
    pub history: Vec<AccessEvent>,
}
impl ProofRecord {
    pub(crate) fn capture(
        r: &DiskRecord,
        ns: &NamespaceConfig,
        text_hash: Option<[u8; 32]>,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            id: MemoryId::from_bytes(r.id),
            namespace: r.namespace_id,
            state: NativeRecordState::capture(r, ns, text_hash)?,
            history: r.access_history.clone(),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeRecordChange {
    pub memory_id: MemoryId,
    pub before: Option<NativeRecordState>,
    pub after: NativeRecordState,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeEdgeProof {
    pub source: MemoryId,
    pub target: MemoryId,
    pub edge_type: crate::model::EdgeType,
    pub weight_bits: u32,
    pub auto_created: bool,
    pub created_at: u64,
}
impl From<&super::edges::PersistedEdge> for NativeEdgeProof {
    fn from(e: &super::edges::PersistedEdge) -> Self {
        Self {
            source: e.source,
            target: e.target,
            edge_type: e.edge_type,
            weight_bits: e.weight.to_bits(),
            auto_created: e.auto_created,
            created_at: e.created_at,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeRecordWrite {
    pub namespace_id: crate::model::NamespaceId,
    pub memory_id: MemoryId,
    pub lifecycle_version: LifecycleVersion,
    pub policy_revision: Generation,
    pub previous_generation: Generation,
    pub capture_generation: GenerationRef,
    pub sampled_at: i64,
    pub embedding_dim: u32,
    pub records: Vec<NativeRecordChange>,
    /// Actual canonical edge puts, retained as structural metadata only.
    pub edges: Vec<NativeEdgeProof>,
}
impl NativeRecordWrite {
    pub(crate) fn primary(&self) -> Result<&NativeRecordChange, StorageError> {
        self.records
            .iter()
            .find(|r| r.memory_id == self.memory_id)
            .ok_or_else(|| invalid("native mutation primary record absent"))
    }
    pub(crate) fn validate(
        &self,
        receipt: &WriteReceipt,
        insertion: bool,
    ) -> Result<(), StorageError> {
        if self.namespace_id.is_unset()
            || self.memory_id == MemoryId::nil()
            || self.capture_generation.store_id != receipt.store_id
            || self.capture_generation.generation > self.previous_generation
            || self.previous_generation.next()? != receipt.committed_generation
            || !(1..=8192).contains(&self.embedding_dim)
            || chrono::DateTime::from_timestamp_millis(self.sampled_at).is_none()
            || self.edges.len() > 2
            || (!insertion && !self.edges.is_empty())
        {
            return Err(invalid("invalid native record receipt boundary"));
        }
        let primary = self.primary()?;
        let mut edge_kinds = std::collections::HashSet::new();
        for edge in &self.edges {
            use crate::model::EdgeType;
            if !edge_kinds.insert(edge.edge_type.as_u8())
                || edge.source == edge.target
                || edge.weight_bits != 1_f32.to_bits()
                || edge.auto_created
                || edge.created_at != self.sampled_at.max(0) as u64
                || !matches!(edge.edge_type, EdgeType::ParentChild | EdgeType::Supersedes)
                || (edge.edge_type == EdgeType::ParentChild && edge.target != self.memory_id)
                || (edge.edge_type == EdgeType::Supersedes && edge.source != self.memory_id)
            {
                return Err(invalid("invalid native creation edge evidence"));
            }
        }
        let mut ids = std::collections::HashSet::new();
        for change in &self.records {
            let after = &change.after;
            let fields = &after.lifecycle.lifecycle;
            after.lifecycle.validate()?;
            if let Some(profile) = &after.content.profile_id {
                if profile.len() != 68
                    || !profile.starts_with("ep1:")
                    || !profile.as_bytes()[4..]
                        .iter()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                {
                    return Err(invalid("invalid native profile commitment"));
                }
            }
            if let Some(before) = &change.before {
                before.lifecycle.validate()?;
            }
            if !ids.insert(change.memory_id)
                || change.memory_id == MemoryId::nil()
                || fields.lifecycle_version != self.lifecycle_version
                || fields.policy_revision != self.policy_revision
            {
                return Err(invalid("invalid native record receipt policy/identity"));
            }
            if change.memory_id == self.memory_id {
                if insertion {
                    let n = &after.lifecycle;
                    if change.before.is_some()
                        || fields.record_generation != Generation::default().next()?
                        || fields.timestamp_origin != TimestampOrigin::Native
                        || fields.retrieved_at.is_some()
                        || fields.reinforced_at.is_some()
                        || fields.decay_anchor_at != n.created_at
                        || fields.phase_changed_at != n.created_at
                        || fields.lifecycle_updated_at != self.sampled_at.max(n.created_at)
                        || n.last_accessed_at != n.created_at
                        || n.phase != DecayPhase::Full
                        || !n.has_summary
                        || n.is_permastore
                        || n.strength.get() != 1.
                        || n.decay_strength.get() != 1.
                        || after.history_sha256 != history_hash(&[])?
                        || after.content.label_provenance != LabelProvenance::NativeV2
                        || after.content.profile_id.is_none()
                        || after.content.vector.is_none()
                        || after.content.extensions_sha256
                            != hash_value(&BTreeMap::<String, serde_json::Value>::new())?
                        || (n.has_text != after.content.text_sha256.is_some())
                    {
                        return Err(invalid("invalid native creation evidence"));
                    }
                    if after.content.vector.as_ref().unwrap().allocation_epoch
                        != receipt.committed_generation
                    {
                        return Err(invalid(
                            "native allocation epoch disagrees with its origin transaction",
                        ));
                    }
                } else {
                    let before = change
                        .before
                        .as_ref()
                        .ok_or_else(|| invalid("native tombstone prior state missing"))?;
                    if before.tombstone(self.sampled_at)? != *after {
                        return Err(invalid("native tombstone transition mismatch"));
                    }
                }
            } else {
                if !insertion {
                    return Err(invalid("native tombstone changed another record"));
                }
                let before = change
                    .before
                    .as_ref()
                    .ok_or_else(|| invalid("native peer prior state missing"))?;
                let mut expected = before.clone();
                expected.lifecycle.lifecycle.record_generation =
                    expected.lifecycle.lifecycle.record_generation.next()?;
                let added = u16::try_from(
                    self.edges
                        .iter()
                        .filter(|e| e.source == change.memory_id)
                        .count(),
                )
                .map_err(|_| invalid("native peer edge count overflow"))?;
                expected.edge_count = before
                    .edge_count
                    .checked_add(added)
                    .ok_or_else(|| invalid("native peer edge count overflow"))?;
                if expected != *after || before.edge_count == after.edge_count {
                    return Err(invalid("native edge peer token/state mismatch"));
                }
                if !self.edges.iter().any(|e| e.source == change.memory_id) {
                    return Err(invalid("native peer without an edge put"));
                }
            }
        }
        if insertion
            && primary.after.edge_count
                != self
                    .edges
                    .iter()
                    .filter(|e| e.source == self.memory_id)
                    .count() as u16
        {
            return Err(invalid("native creation outgoing count mismatch"));
        }
        if self.edges.iter().any(|edge| !ids.contains(&edge.source)) {
            return Err(invalid(
                "native edge source missing its atomic record proof",
            ));
        }
        Ok(())
    }
    pub(crate) fn rewind(&self, rows: &mut Vec<ProofRecord>) -> Result<(), StorageError> {
        for change in &self.records {
            let index = rows
                .iter()
                .position(|r| r.id == change.memory_id)
                .ok_or_else(|| invalid("native receipt record absent"))?;
            if rows[index].namespace != self.namespace_id.get() || rows[index].state != change.after
            {
                return Err(invalid(
                    "native record receipt does not match retained state",
                ));
            }
            if let Some(before) = &change.before {
                rows[index].state = before.clone();
            } else {
                rows.remove(index);
            }
        }
        Ok(())
    }
}

/// Creation commitments survive a fork. Actual retained text is checked by the
/// canonical inventory consumer, not trusted merely because the receipt hashes it.
pub(crate) fn creation_proofs(
    read: &redb::ReadTransaction,
) -> Result<HashMap<MemoryId, NativeRecordWrite>, StorageError> {
    use redb::ReadableTable;
    let mut result = HashMap::new();
    for row in read.open_table(super::control::RECEIPTS)?.iter()? {
        let (_, value) = row?;
        let receipt = super::control::validate_receipt(value.value())?.receipt;
        if let MutationOutcome::NativeMemoryInserted { transition } = receipt.outcome {
            if result.insert(transition.memory_id, transition).is_some() {
                return Err(invalid("duplicate native creation identity"));
            }
        }
    }
    Ok(result)
}

pub(crate) fn validate_retained_creation(
    record: &DiskRecord,
    namespace: &NamespaceConfig,
    creation: Option<&NativeRecordWrite>,
    ledger: Option<&super::metadata::removal_ledger::LedgerExport>,
) -> Result<(), StorageError> {
    let schema = record
        .schema
        .as_ref()
        .ok_or_else(|| invalid("native canonical record schema missing"))?;
    let Some(creation) = creation else {
        if schema.lifecycle.timestamp_origin == TimestampOrigin::Native {
            return Err(invalid("native record has no typed creation proof"));
        }
        return Ok(());
    };
    let original = &creation.primary()?.after;
    let expected = &original.content;
    let mask = ledger
        .into_iter()
        .flat_map(|l| &l.entries)
        .filter(|e| {
            e.namespace_id == record.namespace_id
                && e.memory_uuid == MemoryId::from_bytes(record.id).into_inner()
        })
        .fold(0_u8, |mask, e| mask | e.removed_fields_mask);
    use super::metadata::removal_ledger::{EMBEDDING, FULL_TEXT, SUMMARY, TAGS};
    let current = ContentProof::capture(record, namespace, expected.text_sha256)?;
    if record.namespace_id != creation.namespace_id.get()
        || record.created_at != original.lifecycle.created_at
        || schema.lifecycle.observed_at != original.lifecycle.lifecycle.observed_at
        || schema.lifecycle.timestamp_origin != TimestampOrigin::Native
        || record.difficulty != original.lifecycle.difficulty.get()
        || current.label_provenance != LabelProvenance::NativeV2
        || current.profile_id != expected.profile_id
        || current.extensions_sha256 != expected.extensions_sha256
        || (mask & SUMMARY == 0 && current.summary_sha256 != expected.summary_sha256)
        || (mask & TAGS == 0
            && (current.tags_sha256 != expected.tags_sha256
                || current.labels_sha256 != expected.labels_sha256))
        || (mask & FULL_TEXT == 0 && (record.text_length != 0) != original.lifecycle.has_text)
        || (mask & EMBEDDING == 0 && current.vector != expected.vector)
    {
        return Err(invalid(
            "retained native record disagrees with its creation commitment",
        ));
    }
    Ok(())
}
pub(crate) fn validate_retained_text(
    record: &DiskRecord,
    creation: Option<&NativeRecordWrite>,
    text: &str,
) -> Result<(), StorageError> {
    if let Some(creation) = creation {
        if record.text_length != 0
            && creation.primary()?.after.content.text_sha256 != Some(hash_bytes(text.as_bytes()))
        {
            return Err(invalid(
                "retained native text disagrees with creation commitment",
            ));
        }
    }
    Ok(())
}

impl super::native_lifecycle::NativeLifecycleSession {
    pub fn capture_insert(
        &self,
        scope: LifecycleScope,
        input: &NativeInsert,
    ) -> Result<NativeInsertIntent, StorageError> {
        self.metadata()
            .capture_native_insert(scope, input, &self.policy)
    }
    pub fn insert_prepared(
        &mut self,
        ctx: &MutationContext,
        intent: &NativeInsertIntent,
        generated: &crate::embedding::runtime::GeneratedEmbedding,
    ) -> Result<WriteReceipt, StorageError> {
        self.engine.insert_native_record(
            ctx,
            intent,
            generated,
            &self.policy,
            self.clock.as_ref(),
            None,
        )
    }
    pub fn capture_tombstone(
        &self,
        id: MemoryId,
        namespace: Option<crate::model::NamespaceId>,
    ) -> Result<NativeTombstoneIntent, StorageError> {
        self.metadata()
            .capture_native_tombstone(id, namespace, &self.policy)
    }
    pub fn tombstone(
        &mut self,
        ctx: &MutationContext,
        intent: &NativeTombstoneIntent,
    ) -> Result<WriteReceipt, StorageError> {
        self.engine
            .tombstone_native_record(ctx, intent, &self.policy, self.clock.as_ref(), None)
    }
    /// Explicit offline wrapper only. The accepted root lives across provider
    /// await; no publication/storage lock or second ownership session does.
    pub async fn insert_generated(
        &mut self,
        ctx: &MutationContext,
        scope: LifecycleScope,
        input: &NativeInsert,
        provider: &dyn crate::embedding::EmbeddingProvider,
    ) -> Result<WriteReceipt, StorageError> {
        let root = self.metadata().work_tracker().begin()?;
        let result = self
            .insert_accepted(ctx, scope, input, provider, &root)
            .await;
        self.metadata().finish_work(root);
        result
    }
    async fn insert_accepted(
        &mut self,
        ctx: &MutationContext,
        scope: LifecycleScope,
        input: &NativeInsert,
        provider: &dyn crate::embedding::EmbeddingProvider,
        root: &super::work::WorkTicket,
    ) -> Result<WriteReceipt, StorageError> {
        if ctx.scope != super::control::MutationScope::Namespace(scope.namespace_id) {
            return Err(StorageError::MutationNamespaceMismatch);
        }
        if let Some(receipt) = self
            .metadata()
            .lookup_native_insert(ctx, input, Some(root))?
        {
            return Ok(receipt);
        }
        let intent = self.capture_insert(scope, input)?;
        let generated = if input.memory.embedding.is_some() {
            crate::embedding::runtime::GeneratedEmbedding {
                values: vec![],
                profile: None,
            }
        } else {
            let profile = provider.profile().ok_or_else(|| {
                StorageError::ProviderVectorUnavailable("provider identity missing".into())
            })?;
            crate::embedding::profile::require_compatible(&intent.namespace, &profile)
                .map_err(|e| StorageError::ProviderVectorUnavailable(e.to_string()))?;
            if provider.dimensions() != intent.namespace.embedding_dim as usize {
                return Err(StorageError::ProviderVectorUnavailable(
                    "provider dimension differs".into(),
                ));
            }
            let values = provider
                .embed(
                    input
                        .memory
                        .full_text
                        .as_deref()
                        .unwrap_or(&input.memory.summary),
                )
                .await
                .map_err(|e| StorageError::ProviderVectorUnavailable(e.to_string()))?;
            crate::embedding::runtime::GeneratedEmbedding {
                values,
                profile: Some(profile),
            }
        };
        self.engine.insert_native_record(
            ctx,
            &intent,
            &generated,
            &self.policy,
            self.clock.as_ref(),
            Some(root),
        )
    }
}
