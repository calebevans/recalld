//! Canonical schema-v2 values. Schema version never selects lifecycle policy.
use crate::model::{AccessKind, DecayPhase, DiskRecord, NamespaceConfig};
use crate::storage::control::Generation;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const RECORD_SCHEMA_VERSION: u8 = 2;
pub const NAMESPACE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleVersion {
    LegacyAccessV1,
    ExplicitFeedbackV2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampOrigin {
    Native,
    LegacyInferred,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelProvenance {
    NativeV2,
    LegacyRetainedOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StructuredLabels {
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
    pub provenance: LabelProvenance,
}
impl StructuredLabels {
    fn from_legacy(record: &DiskRecord) -> Self {
        let mut result = Self {
            entities: vec![],
            topics: vec![],
            emotions: vec![],
            provenance: LabelProvenance::LegacyRetainedOnly,
        };
        if matches!(record.phase, DecayPhase::Ghost | DecayPhase::Tombstone) {
            return result;
        }
        for tag in &record.tags {
            for (prefix, labels) in [
                ("entity/", &mut result.entities),
                ("topic/", &mut result.topics),
                ("emotion/", &mut result.emotions),
            ] {
                if let Some(suffix) = tag.as_str().strip_prefix(prefix) {
                    labels.push(suffix.to_owned());
                }
            }
        }
        result
    }
    pub fn validate(&self) -> Result<(), String> {
        for values in [&self.entities, &self.topics, &self.emotions] {
            if values.len() > crate::model::constants::MAX_TAGS
                || values
                    .iter()
                    .any(|v| v.len() > crate::model::constants::LABEL_MAX_BYTES || v.is_empty())
            {
                return Err("invalid structured label length/count".into());
            }
        }
        Ok(())
    }
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.topics.is_empty() && self.emotions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleFields {
    pub observed_at: i64,
    pub retrieved_at: Option<i64>,
    pub reinforced_at: Option<i64>,
    pub decay_anchor_at: i64,
    pub lifecycle_version: LifecycleVersion,
    pub lifecycle_updated_at: i64,
    pub timestamp_origin: TimestampOrigin,
    pub phase_changed_at: i64,
    pub policy_revision: Generation,
    pub record_generation: Generation,
}
impl LifecycleFields {
    pub fn from_legacy(record: &DiskRecord, migration_now: i64) -> Self {
        let latest = |kinds: &[AccessKind]| {
            record
                .access_history
                .iter()
                .filter(|e| kinds.contains(&e.kind))
                .map(|e| e.timestamp)
                .max()
        };
        Self {
            observed_at: record.created_at,
            retrieved_at: latest(&[
                AccessKind::DirectRetrieval,
                AccessKind::AssociativeRetrieval,
            ]),
            reinforced_at: latest(&[AccessKind::ManualReinforcement]),
            decay_anchor_at: record.last_accessed_at,
            lifecycle_version: LifecycleVersion::LegacyAccessV1,
            lifecycle_updated_at: record.created_at.max(record.last_accessed_at),
            timestamp_origin: TimestampOrigin::LegacyInferred,
            phase_changed_at: migration_now,
            policy_revision: Generation::default(),
            record_generation: Generation::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Openai,
    Ollama,
    Bedrock,
    Passthrough,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreprocessingIdentity {
    pub version: String,
    pub prefix: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Normalization {
    L2UnitV1,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddingProfile {
    pub identity_version: u32,
    pub provider_kind: ProviderKind,
    pub model_id: String,
    pub model_revision: Option<String>,
    pub dimension: u32,
    pub document_preprocessing: PreprocessingIdentity,
    pub query_preprocessing: PreprocessingIdentity,
    pub normalization: Normalization,
}
impl EmbeddingProfile {
    pub fn validate(&self) -> Result<(), String> {
        if self.identity_version != 1
            || !(1..=8192).contains(&self.dimension)
            || self.model_id.is_empty()
            || self.document_preprocessing.version != "recalld-document-v1"
            || self.query_preprocessing.version != "recalld-query-v1"
        {
            return Err("unsupported or incomplete embedding profile".into());
        }
        Ok(())
    }
    /// ep1 digest: ordered u64-BE-length-delimited byte fields. Revision has a
    /// separate 0/1 presence field, so None never aliases Some(""). No secrets.
    pub fn profile_id(&self) -> Result<String, String> {
        self.validate()?;
        let mut hash = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hash.update((bytes.len() as u64).to_be_bytes());
            hash.update(bytes);
        };
        field(b"recalld-embedding-profile");
        field(&self.identity_version.to_be_bytes());
        field(match self.provider_kind {
            ProviderKind::Openai => b"openai",
            ProviderKind::Ollama => b"ollama",
            ProviderKind::Bedrock => b"bedrock",
            ProviderKind::Passthrough => b"passthrough",
        });
        field(self.model_id.as_bytes());
        field(&[u8::from(self.model_revision.is_some())]);
        if let Some(revision) = &self.model_revision {
            field(revision.as_bytes());
        }
        field(&self.dimension.to_be_bytes());
        field(self.document_preprocessing.version.as_bytes());
        field(self.document_preprocessing.prefix.as_bytes());
        field(self.query_preprocessing.version.as_bytes());
        field(self.query_preprocessing.prefix.as_bytes());
        field(b"l2_unit_v1");
        Ok(format!(
            "ep1:{}",
            hash.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingProvenance {
    NativeV2,
    OperatorAttestation,
    Reembedding,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum EmbeddingBinding {
    UnknownLegacy,
    Bound {
        #[serde(rename = "profileId")]
        profile_id: String,
        descriptor: EmbeddingProfile,
        provenance: BindingProvenance,
    },
}
impl EmbeddingBinding {
    pub fn validate(&self, dimension: u32) -> Result<(), String> {
        if let Self::Bound {
            profile_id,
            descriptor,
            ..
        } = self
        {
            if descriptor.dimension != dimension || &descriptor.profile_id()? != profile_id {
                return Err("embedding binding mismatch".into());
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VectorReference {
    pub slot: u32,
    pub allocation_epoch: Generation,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordSchemaV2 {
    pub lifecycle: LifecycleFields,
    pub labels: StructuredLabels,
    /// None for tombstones. Unknown legacy model identity is namespace-bound.
    pub vector: Option<VectorReference>,
    /// Unknown required features block decode. Optional fields round-trip verbatim.
    pub required_features: Vec<String>,
    pub extensions: BTreeMap<String, serde_json::Value>,
}
impl RecordSchemaV2 {
    pub fn from_legacy(
        record: &DiskRecord,
        migration_now: i64,
        vector: Option<VectorReference>,
    ) -> Self {
        Self {
            lifecycle: LifecycleFields::from_legacy(record, migration_now),
            labels: StructuredLabels::from_legacy(record),
            vector,
            required_features: vec![],
            extensions: BTreeMap::new(),
        }
    }
    pub fn validate(&self, record: &DiskRecord) -> Result<(), String> {
        if !self.required_features.is_empty() {
            return Err("unknown required record feature".into());
        }
        self.labels.validate()?;
        if !record.strength.is_finite()
            || !record.decay_strength.is_finite()
            || !record.stability.is_finite()
            || !record.difficulty.is_finite()
            || record.stability <= 0.
            || !(1.0..=10.0).contains(&record.difficulty)
            || record.is_permastore > 1
            || record.namespace_id == 0
            || (record.text_offset == 0) != (record.text_length == 0)
        {
            return Err("invalid canonical record fields".into());
        }
        let t = &self.lifecycle;
        for time in [
            Some(record.created_at),
            Some(record.last_accessed_at),
            Some(t.observed_at),
            t.retrieved_at,
            t.reinforced_at,
            Some(t.decay_anchor_at),
            Some(t.lifecycle_updated_at),
            Some(t.phase_changed_at),
        ] {
            if time.is_some_and(|v| chrono::DateTime::from_timestamp_millis(v).is_none()) {
                return Err("unrepresentable lifecycle timestamp".into());
            }
        }
        if matches!(record.phase, DecayPhase::Ghost | DecayPhase::Tombstone)
            && !self.labels.is_empty()
        {
            return Err("removed phase contains structured labels".into());
        }
        match (&self.vector, record.phase) {
            (None, DecayPhase::Tombstone) => {}
            (Some(v), phase) if phase != DecayPhase::Tombstone && v.slot == record.vector_slot => {}
            _ => return Err("record vector ownership mismatch".into()),
        }
        // A rollback preserves distinct native anchors/retrieval times. Revision
        // zero keeps the legacy invariant; canonical inspection additionally
        // requires receipt-backed namespace history for every later revision.
        if self.lifecycle.lifecycle_version == LifecycleVersion::LegacyAccessV1
            && self.lifecycle.policy_revision == Generation::default()
            && self.lifecycle.decay_anchor_at != record.last_accessed_at
        {
            return Err("legacy lifecycle anchor mismatch".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespaceSchemaV2 {
    pub namespace_schema_version: u32,
    pub lifecycle_version: LifecycleVersion,
    pub policy_revision: Generation,
    pub embedding_binding: EmbeddingBinding,
    pub required_features: Vec<String>,
    pub extensions: BTreeMap<String, serde_json::Value>,
}
impl NamespaceSchemaV2 {
    pub fn from_legacy() -> Self {
        Self {
            namespace_schema_version: NAMESPACE_SCHEMA_VERSION,
            lifecycle_version: LifecycleVersion::LegacyAccessV1,
            policy_revision: Generation::default(),
            embedding_binding: EmbeddingBinding::UnknownLegacy,
            required_features: vec![],
            extensions: BTreeMap::new(),
        }
    }
    pub fn validate(&self, namespace: &NamespaceConfig) -> Result<(), String> {
        if self.namespace_schema_version != NAMESPACE_SCHEMA_VERSION
            || !self.required_features.is_empty()
        {
            return Err("unsupported namespace schema/features".into());
        }
        let floats = [
            namespace.initial_stability,
            namespace.default_difficulty,
            namespace.permastore_threshold,
            namespace.desired_retention,
            namespace.phase_thresholds.full_to_summary,
            namespace.phase_thresholds.summary_to_ghost,
            namespace.phase_thresholds.ghost_to_delete,
            namespace.decay_rate_multiplier.unwrap_or(1.),
        ];
        if floats.iter().any(|f| !f.is_finite())
            || namespace.initial_stability <= 0.
            || namespace.permastore_threshold <= 0.
            || !(1.0..=10.0).contains(&namespace.default_difficulty)
            || namespace.decay_rate_multiplier.is_some_and(|v| v < 0.)
            || !(1..=8192).contains(&namespace.embedding_dim)
            || namespace.phase_thresholds.validate().is_err()
        {
            return Err("invalid canonical namespace policy".into());
        }
        self.embedding_binding.validate(namespace.embedding_dim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccessEvent, MemoryId, Tag};
    pub(crate) fn record() -> DiskRecord {
        DiskRecord {
            version: 1,
            schema: None,
            id: *MemoryId::new().as_bytes(),
            namespace_id: 1,
            created_at: 100,
            last_accessed_at: 400,
            phase: DecayPhase::Full,
            strength: -0.0,
            decay_strength: 0.375,
            stability: 3.7145,
            difficulty: 5.,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "exact résumé".into(),
            tags: vec![Tag::new("entity/john-smith").unwrap()],
            access_history: vec![
                AccessEvent {
                    timestamp: 200,
                    kind: AccessKind::DirectRetrieval,
                },
                AccessEvent {
                    timestamp: 300,
                    kind: AccessKind::ManualReinforcement,
                },
            ],
            text_offset: 0,
            text_length: 0,
        }
    }
    fn upgraded() -> DiskRecord {
        let mut r = record();
        r.schema = Some(RecordSchemaV2::from_legacy(
            &r,
            500,
            Some(VectorReference {
                slot: 0,
                allocation_epoch: Generation::default(),
                sha256: [7; 32],
            }),
        ));
        r.version = 2;
        r
    }
    #[test]
    fn schema_codec_preserves_utf8_order_duplicates_float_bits_and_optional_extensions() {
        let mut r = upgraded();
        let schema = r.schema.as_mut().unwrap();
        schema.labels = StructuredLabels {
            entities: vec![
                "John Smith".into(),
                "José".into(),
                "Alice's".into(),
                "José".into(),
            ],
            topics: vec!["日本語".into()],
            emotions: vec!["mixed feelings".into()],
            provenance: LabelProvenance::NativeV2,
        };
        schema.extensions.insert(
            "future.optional.v7".into(),
            serde_json::json!({"exact":"Keep Case", "array":[2,1,1]}),
        );
        let bytes = r.encode().unwrap();
        let decoded = DiskRecord::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.encode().unwrap(), bytes);
        assert_eq!(decoded.schema, r.schema);
        assert_eq!(decoded.strength.to_bits(), r.strength.to_bits());
        assert_eq!(decoded.stability.to_bits(), r.stability.to_bits());
        assert_eq!(decoded.last_accessed_at, 400);
    }
    #[test]
    fn schema_cached_entities_preserve_exact_utf8_order_duplicates_and_byte_limit() {
        let mut r = upgraded();
        let entities = vec![
            "John Smith".into(),
            "José".into(),
            "Jose\u{301}".into(),
            "Alice's".into(),
            "日本語".into(),
            "José".into(),
            "é".repeat(64),
        ];
        assert_eq!(
            entities.last().unwrap().len(),
            crate::model::constants::LABEL_MAX_BYTES
        );
        let labels = &mut r.schema.as_mut().unwrap().labels;
        labels.entities = entities.clone();
        labels.provenance = LabelProvenance::NativeV2;
        let bytes = r.encode().unwrap();
        let decoded = DiskRecord::from_bytes(&bytes).unwrap();
        let cached = crate::model::CachedRecord::from(&decoded);
        assert_eq!(cached.entities, entities);
        assert_eq!(cached.tags, decoded.tags);
        assert_eq!(decoded.encode().unwrap(), bytes);
        assert_eq!(
            decoded.schema.unwrap().labels.provenance,
            LabelProvenance::NativeV2
        );
    }

    #[test]
    fn schema_cached_entities_empty_is_authoritative_over_retained_tags() {
        let mut r = upgraded();
        r.schema.as_mut().unwrap().labels.entities.clear();
        assert!(
            !crate::model::parse_structured_tags(&r.tags)
                .entities
                .is_empty()
        );
        let bytes = r.encode().unwrap();
        let decoded = DiskRecord::from_bytes(&bytes).unwrap();
        assert!(
            crate::model::CachedRecord::from(&decoded)
                .entities
                .is_empty()
        );
        assert_eq!(decoded.encode().unwrap(), bytes);
    }

    #[test]
    fn schema_cached_entities_keep_legacy_parser_and_retained_only_provenance_distinct() {
        let legacy = record();
        let cached = crate::model::CachedRecord::from(&legacy);
        assert_eq!(
            cached.entities,
            crate::model::parse_structured_tags(&legacy.tags).entities
        );
        assert_eq!(cached.entities, ["John-smith"]);
        assert!(legacy.schema.is_none());
        let migrated = upgraded();
        let bytes = migrated.encode().unwrap();
        let decoded = DiskRecord::from_bytes(&bytes).unwrap();
        // Only surviving tag suffixes were migrated; no original label is inferred.
        assert_eq!(
            crate::model::CachedRecord::from(&decoded).entities,
            ["john-smith"]
        );
        assert_eq!(
            decoded.schema.as_ref().unwrap().labels.provenance,
            LabelProvenance::LegacyRetainedOnly
        );
        assert_eq!(decoded.encode().unwrap(), bytes);
    }

    #[test]
    fn schema_legacy_conversion_labels_evidence_and_defaults_are_explicit() {
        let r = upgraded();
        let schema = r.schema.unwrap();
        assert_eq!(schema.lifecycle.observed_at, 100);
        assert_eq!(schema.lifecycle.decay_anchor_at, 400);
        assert_eq!(schema.lifecycle.retrieved_at, Some(200));
        assert_eq!(schema.lifecycle.reinforced_at, Some(300));
        assert_eq!(schema.lifecycle.phase_changed_at, 500);
        assert_eq!(
            schema.lifecycle.timestamp_origin,
            TimestampOrigin::LegacyInferred
        );
        assert_eq!(schema.labels.entities, vec!["john-smith"]);
        let mut legacy = record();
        legacy.access_history.clear();
        legacy.phase = DecayPhase::Tombstone;
        let schema = RecordSchemaV2::from_legacy(&legacy, 500, None);
        assert_eq!(schema.lifecycle.reinforced_at, None);
        assert_eq!(schema.lifecycle.retrieved_at, None);
        assert!(schema.labels.is_empty());
        assert_eq!(
            NamespaceSchemaV2::from_legacy().embedding_binding,
            EmbeddingBinding::UnknownLegacy
        );
        assert_eq!(
            NamespaceSchemaV2::from_legacy().lifecycle_version,
            LifecycleVersion::LegacyAccessV1
        );
    }
    #[test]
    fn schema_codec_rejects_trailing_truncated_unknown_and_lossy_legacy_data() {
        let r = upgraded();
        let bytes = r.encode().unwrap();
        for end in 0..bytes.len() {
            assert!(DiskRecord::from_bytes(&bytes[..end]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(DiskRecord::from_bytes(&trailing).is_err());
        let mut unknown = r;
        unknown
            .schema
            .as_mut()
            .unwrap()
            .required_features
            .push("future-required".into());
        assert!(DiskRecord::from_bytes(&unknown.to_bytes()).is_err());
        assert!(unknown.encode().is_err());
        let mut legacy = record().to_bytes();
        *legacy.last_mut().unwrap() = 255;
        assert!(
            DiskRecord::from_bytes(&legacy).is_err(),
            "unknown history cannot become retrieval"
        );
    }
    #[test]
    fn schema_profile_identity_distinguishes_revision_presence_prefix_and_case() {
        let profile = EmbeddingProfile {
            identity_version: 1,
            provider_kind: ProviderKind::Openai,
            model_id: "Model".into(),
            model_revision: None,
            dimension: 4,
            document_preprocessing: PreprocessingIdentity {
                version: "recalld-document-v1".into(),
                prefix: "doc: ".into(),
            },
            query_preprocessing: PreprocessingIdentity {
                version: "recalld-query-v1".into(),
                prefix: "query: ".into(),
            },
            normalization: Normalization::L2UnitV1,
        };
        let id = profile.profile_id().unwrap();
        let mut changed = profile.clone();
        changed.model_revision = Some(String::new());
        assert_ne!(id, changed.profile_id().unwrap());
        changed = profile.clone();
        changed.model_id = "model".into();
        assert_ne!(id, changed.profile_id().unwrap());
        changed = profile.clone();
        changed.query_preprocessing.prefix.push(' ');
        assert_ne!(id, changed.profile_id().unwrap());
        assert!(
            EmbeddingBinding::Bound {
                profile_id: id,
                descriptor: profile,
                provenance: BindingProvenance::OperatorAttestation
            }
            .validate(5)
            .is_err()
        );
    }
}
