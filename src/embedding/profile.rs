//! Profile checks for schema-aware consumers. These do not enable v2 serving.
//!
//! A caller must supply the explicitly resolved active descriptor; configuration
//! and vector width alone never establish the provenance of stored vectors.
use crate::model::schema::{EmbeddingBinding, EmbeddingProfile};
use crate::model::{NamespaceConfig, NamespaceId};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("schema-v2 semantic serving requires participating writers; serving remains disabled")]
    ServingDisabled,
    #[error("namespace assertion differs from the canonical namespace identity")]
    NamespaceMismatch,
    #[error("migrate the canonical namespace schema before preparing an attestation")]
    SchemaMigrationRequired,
    #[error("namespace already has an embedding binding; attestation cannot replace it")]
    AlreadyBound,
    #[error("invalid or unsupported embedding profile descriptor")]
    InvalidDescriptor,
    #[error("invalid canonical namespace schema or embedding binding")]
    InvalidNamespace,
    #[error("embedding provenance is unknown; explicitly attest or re-embed before semantic use")]
    UnknownLegacy,
    #[error("embedding dimension differs from the canonical namespace")]
    DimensionMismatch,
    #[error("embedding profile differs from the canonical namespace binding")]
    ProfileMismatch,
}

/// Parse the canonical descriptor directly, including duplicate-field rejection.
/// Missing revision means unknown; all other identity fields remain mandatory.
/// Diagnostics deliberately omit supplied values and raw JSON.
pub fn parse_profile(bytes: &[u8]) -> Result<EmbeddingProfile, ProfileError> {
    let profile: EmbeddingProfile =
        serde_json::from_slice(bytes).map_err(|_| ProfileError::InvalidDescriptor)?;
    profile
        .validate()
        .map_err(|_| ProfileError::InvalidDescriptor)?;
    Ok(profile)
}

/// Require an already-bound canonical namespace to match an explicit profile.
/// The returned ID is suitable for a future profile-aware consumer, not a claim
/// that its indexes, writer barrier, or provider runtime are ready.
pub fn require_compatible(
    namespace: &NamespaceConfig,
    active: &EmbeddingProfile,
) -> Result<String, ProfileError> {
    let active_id = active
        .profile_id()
        .map_err(|_| ProfileError::InvalidDescriptor)?;
    let schema = namespace
        .schema
        .as_ref()
        .ok_or(ProfileError::UnknownLegacy)?;
    schema
        .validate(namespace)
        .map_err(|_| ProfileError::InvalidNamespace)?;
    let EmbeddingBinding::Bound { profile_id, .. } = &schema.embedding_binding else {
        return Err(ProfileError::UnknownLegacy);
    };
    if active.dimension != namespace.embedding_dim {
        return Err(ProfileError::DimensionMismatch);
    }
    if active_id != *profile_id {
        return Err(ProfileError::ProfileMismatch);
    }
    Ok(active_id)
}

/// A validated operator proposal, NOT verified vector provenance or a binding.
/// Storage must verify vector provenance/ownership and atomically adopt it under
/// the all-writer barrier. This value grants no serving or mutation permission.
#[derive(Debug)]
pub struct PreparedAttestation {
    namespace_id: NamespaceId,
    profile_id: String,
    descriptor: EmbeddingProfile,
}

impl PreparedAttestation {
    pub fn namespace_id(&self) -> NamespaceId {
        self.namespace_id
    }

    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub fn descriptor(&self) -> &EmbeddingProfile {
        &self.descriptor
    }
}

/// Prepare an explicit operator proposal without adopting it or touching vectors.
/// Existing bindings are immutable here, including those with the same profile.
/// The caller must resolve a canonical namespace, not a cached/session default.
pub fn prepare_attestation(
    namespace: &NamespaceConfig,
    expected_namespace_id: NamespaceId,
    descriptor: EmbeddingProfile,
) -> Result<PreparedAttestation, ProfileError> {
    if namespace.id != expected_namespace_id {
        return Err(ProfileError::NamespaceMismatch);
    }
    let profile_id = descriptor
        .profile_id()
        .map_err(|_| ProfileError::InvalidDescriptor)?;
    let schema = namespace
        .schema
        .as_ref()
        .ok_or(ProfileError::SchemaMigrationRequired)?;
    schema
        .validate(namespace)
        .map_err(|_| ProfileError::InvalidNamespace)?;
    if !matches!(schema.embedding_binding, EmbeddingBinding::UnknownLegacy) {
        return Err(ProfileError::AlreadyBound);
    }
    if descriptor.dimension != namespace.embedding_dim {
        return Err(ProfileError::DimensionMismatch);
    }
    Ok(PreparedAttestation {
        namespace_id: namespace.id,
        profile_id,
        descriptor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::schema::{
        BindingProvenance, NamespaceSchemaV2, Normalization, PreprocessingIdentity, ProviderKind,
    };
    use serde_json::json;

    fn profile() -> EmbeddingProfile {
        EmbeddingProfile {
            identity_version: 1,
            provider_kind: ProviderKind::Ollama,
            model_id: "model-v1".into(),
            model_revision: None,
            dimension: 4,
            document_preprocessing: PreprocessingIdentity {
                version: "recalld-document-v1".into(),
                prefix: "document: ".into(),
            },
            query_preprocessing: PreprocessingIdentity {
                version: "recalld-query-v1".into(),
                prefix: "query: ".into(),
            },
            normalization: Normalization::L2UnitV1,
        }
    }

    fn namespace() -> NamespaceConfig {
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.embedding_dim = 4;
        ns.schema = Some(NamespaceSchemaV2::from_legacy());
        ns
    }

    #[test]
    fn profile_input_rejects_types_nulls_unknowns_and_duplicate_fields() {
        let valid = serde_json::to_value(profile()).unwrap();
        for (pointer, values) in [
            (
                "/dimension",
                vec![
                    json!(null),
                    json!("4"),
                    json!(4.0),
                    json!(0),
                    json!(8193),
                    json!(u64::MAX),
                ],
            ),
            ("/identityVersion", vec![json!(null), json!(2), json!("1")]),
            ("/modelId", vec![json!(null), json!(false), json!("")]),
            ("/modelRevision", vec![json!(false), json!(3)]),
            ("/providerKind", vec![json!(null), json!("unknown")]),
            ("/normalization", vec![json!(null), json!("none")]),
            (
                "/documentPreprocessing/version",
                vec![json!(null), json!("future")],
            ),
            (
                "/queryPreprocessing/prefix",
                vec![json!(null), json!(false)],
            ),
        ] {
            for value in values {
                let mut input = valid.clone();
                *input.pointer_mut(pointer).unwrap() = value;
                assert_eq!(
                    parse_profile(&serde_json::to_vec(&input).unwrap()),
                    Err(ProfileError::InvalidDescriptor),
                    "{pointer}"
                );
            }
        }
        for field in [
            "identityVersion",
            "dimension",
            "providerKind",
            "modelId",
            "normalization",
            "documentPreprocessing",
            "queryPreprocessing",
        ] {
            let mut input = valid.clone();
            input.as_object_mut().unwrap().remove(field);
            assert!(
                parse_profile(&serde_json::to_vec(&input).unwrap()).is_err(),
                "{field}"
            );
        }
        let mut unknown = valid.clone();
        unknown["apiKey"] = json!("not-a-real-secret");
        let error = parse_profile(&serde_json::to_vec(&unknown).unwrap()).unwrap_err();
        assert!(!error.to_string().contains("not-a-real-secret"));
        let text = serde_json::to_string(&valid).unwrap();
        let duplicate = text.replacen('{', "{\"dimension\":4,", 1);
        assert!(parse_profile(duplicate.as_bytes()).is_err());
        for dimension in [1, 8192] {
            let mut input = valid.clone();
            input["dimension"] = json!(dimension);
            assert_eq!(
                parse_profile(&serde_json::to_vec(&input).unwrap())
                    .unwrap()
                    .dimension,
                dimension
            );
        }
        let mut missing_revision = valid;
        missing_revision
            .as_object_mut()
            .unwrap()
            .remove("modelRevision");
        assert_eq!(
            parse_profile(&serde_json::to_vec(&missing_revision).unwrap())
                .unwrap()
                .model_revision,
            None
        );
    }

    #[test]
    fn compatibility_never_blesses_unknown_legacy_or_equal_width_different_profiles() {
        let active = profile();
        let mut ns = namespace();
        assert_eq!(
            require_compatible(&ns, &active),
            Err(ProfileError::UnknownLegacy)
        );
        ns.schema = None;
        assert_eq!(
            require_compatible(&ns, &active),
            Err(ProfileError::UnknownLegacy)
        );
        let mut schema = NamespaceSchemaV2::from_legacy();
        schema.embedding_binding = EmbeddingBinding::Bound {
            profile_id: active.profile_id().unwrap(),
            descriptor: active.clone(),
            provenance: BindingProvenance::OperatorAttestation,
        };
        ns.schema = Some(schema);
        let before = serde_json::to_vec(&ns).unwrap();
        assert_eq!(
            require_compatible(&ns, &active).unwrap(),
            active.profile_id().unwrap()
        );
        for change in 0..6 {
            let mut other = active.clone();
            match change {
                0 => other.model_id.push(' '),
                1 => other.model_revision = Some(String::new()),
                2 => other.document_preprocessing.prefix.push(' '),
                3 => other.query_preprocessing.prefix.push(' '),
                4 => other.provider_kind = ProviderKind::Openai,
                _ => other.model_id = "MODEL-V1".into(),
            }
            assert_eq!(
                require_compatible(&ns, &other),
                Err(ProfileError::ProfileMismatch)
            );
        }
        let mut wider = active.clone();
        wider.dimension = 5;
        assert_eq!(
            require_compatible(&ns, &wider),
            Err(ProfileError::DimensionMismatch)
        );
        assert_eq!(serde_json::to_vec(&ns).unwrap(), before);
        if let EmbeddingBinding::Bound { profile_id, .. } =
            &mut ns.schema.as_mut().unwrap().embedding_binding
        {
            *profile_id = "ep1:forged".into();
        }
        assert_eq!(
            require_compatible(&ns, &active),
            Err(ProfileError::InvalidNamespace)
        );
    }

    #[test]
    fn attestation_preparation_asserts_identity_and_never_changes_provenance() {
        let active = profile();
        let mut ns = namespace();
        let before = serde_json::to_vec(&ns).unwrap();
        assert_eq!(
            prepare_attestation(&ns, NamespaceId::new(2), active.clone()).unwrap_err(),
            ProfileError::NamespaceMismatch
        );
        let proposal = prepare_attestation(&ns, ns.id, active.clone()).unwrap();
        assert_eq!(proposal.namespace_id(), ns.id);
        assert_eq!(proposal.profile_id(), active.profile_id().unwrap());
        assert_eq!(proposal.descriptor(), &active);
        assert_eq!(
            require_compatible(&ns, &active),
            Err(ProfileError::UnknownLegacy)
        );
        let mut wrong_width = active.clone();
        wrong_width.dimension = 5;
        assert_eq!(
            prepare_attestation(&ns, ns.id, wrong_width).unwrap_err(),
            ProfileError::DimensionMismatch
        );
        assert_eq!(serde_json::to_vec(&ns).unwrap(), before);
        ns.schema.as_mut().unwrap().embedding_binding = EmbeddingBinding::Bound {
            profile_id: active.profile_id().unwrap(),
            descriptor: active.clone(),
            provenance: BindingProvenance::NativeV2,
        };
        let before = serde_json::to_vec(&ns).unwrap();
        assert_eq!(
            prepare_attestation(&ns, ns.id, active.clone()).unwrap_err(),
            ProfileError::AlreadyBound
        );
        assert_eq!(serde_json::to_vec(&ns).unwrap(), before);
        ns.schema = None;
        assert_eq!(
            prepare_attestation(&ns, ns.id, active).unwrap_err(),
            ProfileError::SchemaMigrationRequired
        );
    }
}
