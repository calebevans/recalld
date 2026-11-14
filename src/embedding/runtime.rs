//! Runtime profile evidence. This does not bind namespaces or enable v2 writers.
use super::{EmbeddingError, EmbeddingProvider};
use crate::model::NamespaceConfig;
use crate::model::schema::{EmbeddingProfile, Normalization, PreprocessingIdentity, ProviderKind};

/// Internal result carried from the actual provider to canonical insertion.
/// Never deserialize this as caller provenance.
#[derive(Debug, Clone)]
pub struct GeneratedEmbedding {
    pub values: Vec<f32>,
    pub profile: Option<EmbeddingProfile>,
}

impl From<Vec<f32>> for GeneratedEmbedding {
    fn from(values: Vec<f32>) -> Self {
        Self {
            values,
            profile: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NamespaceProfileError {
    #[error("embeddingDim must be an integer in 1..8192")]
    InvalidDimension,
    #[error("embeddingDim {requested} differs from the active provider dimension {active}")]
    InactiveDimension { requested: u32, active: u32 },
    #[error("active embedding provider configuration is unavailable: {0}")]
    Unavailable(&'static str),
}

/// Resolve creation against the actual single provider, never a default
/// namespace row. This permits the staged unverified v1 lane, not attestation.
pub fn creation_dimension(
    provider: &dyn EmbeddingProvider,
    requested: Option<u32>,
) -> Result<u32, NamespaceProfileError> {
    if requested.is_some_and(|dim| !(1..=8192).contains(&dim)) {
        return Err(NamespaceProfileError::InvalidDimension);
    }
    let active = u32::try_from(provider.dimensions())
        .ok()
        .filter(|dim| (1..=8192).contains(dim))
        .ok_or(NamespaceProfileError::Unavailable("invalid dimensions"))?;
    if let Some(profile) = provider.profile() {
        profile
            .validate()
            .map_err(|_| NamespaceProfileError::Unavailable("invalid profile"))?;
        if profile.dimension != active {
            return Err(NamespaceProfileError::Unavailable(
                "profile dimension differs from provider",
            ));
        }
    }
    if let Some(requested) = requested {
        if requested != active {
            return Err(NamespaceProfileError::InactiveDimension { requested, active });
        }
    }
    Ok(active)
}

pub(crate) fn native_profile(
    kind: ProviderKind,
    model: &str,
    dimension: usize,
) -> EmbeddingProfile {
    EmbeddingProfile {
        identity_version: 1,
        provider_kind: kind,
        model_id: model.into(),
        model_revision: None, // Configuration does not prove the remote revision.
        dimension: u32::try_from(dimension).unwrap_or(0),
        document_preprocessing: PreprocessingIdentity {
            version: "recalld-document-v1".into(),
            prefix: String::new(),
        },
        query_preprocessing: PreprocessingIdentity {
            version: "recalld-query-v1".into(),
            prefix: String::new(),
        },
        normalization: Normalization::L2UnitV1,
    }
}

/// The schema-less v1 lane is deliberately unverified compatibility. A migrated
/// namespace must match its immutable binding, and still cannot serve until
/// all writers participate. Metadata-only operations do not call this gate.
pub fn check_namespace(
    namespace: &NamespaceConfig,
    active: Option<&EmbeddingProfile>,
) -> Result<(), super::profile::ProfileError> {
    use super::profile::ProfileError;
    if let Some(active) = active {
        active
            .validate()
            .map_err(|_| ProfileError::InvalidDescriptor)?;
        if active.dimension != namespace.embedding_dim {
            return Err(ProfileError::DimensionMismatch);
        }
    }
    if namespace.schema.is_some() {
        super::profile::require_compatible(namespace, active.ok_or(ProfileError::UnknownLegacy)?)?;
        return Err(ProfileError::ServingDisabled);
    }
    Ok(())
}

pub async fn generate(
    provider: &dyn EmbeddingProvider,
    namespace: &NamespaceConfig,
    text: &str,
) -> Result<GeneratedEmbedding, EmbeddingError> {
    let profile = provider.profile();
    check_namespace(namespace, profile.as_ref())
        .map_err(|e| EmbeddingError::Unavailable(e.to_string()))?;
    if provider.dimensions() != namespace.embedding_dim as usize {
        return Err(EmbeddingError::Unavailable(
            "provider dimension differs from namespace".into(),
        ));
    }
    let values = provider.embed(text).await?;
    // Validate without replacing the raw result: advisory and final transaction
    // must each normalize the same original values (rounding is not idempotent).
    super::validation::normalize_vector(&mut values.clone(), namespace.embedding_dim as usize)
        .map_err(EmbeddingError::InvalidResponse)?;
    Ok(GeneratedEmbedding { values, profile })
}

/// Factory boundary inside the cache: invalid outputs never become cache hits.
pub(crate) struct CheckedProvider(pub Box<dyn EmbeddingProvider>);
impl CheckedProvider {
    fn checked(&self, mut values: Vec<f32>) -> Result<Vec<f32>, EmbeddingError> {
        super::validation::normalize_vector(&mut values, self.dimensions())
            .map_err(EmbeddingError::InvalidResponse)?;
        Ok(values)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for CheckedProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.checked(self.0.embed(text).await?)
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.checked(self.0.embed_query(text).await?)
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let values = self.0.embed_batch(texts).await?;
        if values.len() != texts.len() {
            return Err(EmbeddingError::InvalidResponse(
                "embedding batch count mismatch".into(),
            ));
        }
        values.into_iter().map(|v| self.checked(v)).collect()
    }
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    fn model_name(&self) -> &str {
        self.0.model_name()
    }
    fn profile(&self) -> Option<EmbeddingProfile> {
        self.0.profile()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::embedding::{CachedProvider, PrefixedProvider};
    use crate::model::schema::{BindingProvenance, EmbeddingBinding, NamespaceSchemaV2};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    pub(crate) fn profile() -> EmbeddingProfile {
        native_profile(ProviderKind::Ollama, "configured-model", 4)
    }
    pub(crate) fn bound() -> NamespaceConfig {
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.embedding_dim = 4;
        ns.schema = Some(NamespaceSchemaV2::from_legacy());
        ns.schema.as_mut().unwrap().embedding_binding = EmbeddingBinding::Bound {
            profile_id: profile().profile_id().unwrap(),
            descriptor: profile(),
            provenance: BindingProvenance::OperatorAttestation,
        };
        ns
    }

    struct Output {
        calls: Arc<AtomicUsize>,
        profile: Option<EmbeddingProfile>,
        values: Vec<f32>,
        batch_count: usize,
    }
    #[async_trait::async_trait]
    impl EmbeddingProvider for Output {
        async fn embed(&self, _: &str) -> Result<Vec<f32>, EmbeddingError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.values.clone())
        }
        async fn embed_batch(&self, _: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![self.values.clone(); self.batch_count])
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn model_name(&self) -> &str {
            "configured-model"
        }
        fn profile(&self) -> Option<EmbeddingProfile> {
            self.profile.clone()
        }
    }

    #[tokio::test]
    async fn native_profile_cache_rejects_bad_generated_vectors_and_batches_without_caching() {
        for values in [
            vec![1.],
            vec![0.; 4],
            vec![f32::NAN; 4],
            vec![f32::INFINITY; 4],
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = CachedProvider::new(
                Box::new(CheckedProvider(Box::new(Output {
                    calls: calls.clone(),
                    profile: Some(profile()),
                    values,
                    batch_count: 2,
                }))),
                10,
            );
            for _ in 0..2 {
                assert!(provider.embed("same").await.is_err());
                assert!(provider.embed_query("same").await.is_err());
                assert!(provider.embed_batch(&["a", "b"]).await.is_err());
            }
            assert_eq!(calls.load(Ordering::SeqCst), 6);
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CachedProvider::new(
            Box::new(CheckedProvider(Box::new(Output {
                calls: calls.clone(),
                profile: Some(profile()),
                values: vec![3., 4., 0., 0.],
                batch_count: 1,
            }))),
            10,
        );
        for _ in 0..2 {
            assert!(provider.embed_batch(&["a", "b"]).await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(provider.embed("a").await.unwrap(), vec![0.6, 0.8, 0., 0.]);
        assert_eq!(provider.embed("a").await.unwrap(), vec![0.6, 0.8, 0., 0.]);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn native_profile_generation_checks_binding_before_provider_and_keeps_legacy_unverified()
    {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut output = Output {
            calls: calls.clone(),
            profile: Some(profile()),
            values: vec![1., 0., 0., 0.],
            batch_count: 1,
        };
        let ns = bound();
        output.profile.as_mut().unwrap().model_id = "different-same-width".into();
        assert!(
            generate(&output, &ns, "secret")
                .await
                .unwrap_err()
                .to_string()
                .contains("profile differs")
        );
        output.profile = Some(profile());
        assert!(
            generate(&output, &ns, "secret")
                .await
                .unwrap_err()
                .to_string()
                .contains("serving remains disabled")
        );
        let mut unknown = ns.clone();
        unknown.schema.as_mut().unwrap().embedding_binding = EmbeddingBinding::UnknownLegacy;
        assert!(
            generate(&output, &unknown, "secret")
                .await
                .unwrap_err()
                .to_string()
                .contains("provenance is unknown")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut legacy = ns;
        legacy.schema = None;
        output.profile = None;
        assert!(
            generate(&output, &legacy, "secret")
                .await
                .unwrap()
                .profile
                .is_none()
        );
        assert!(legacy.schema.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_profile_factory_and_wrappers_preserve_effective_credential_free_identity() {
        use crate::embedding::{EmbeddingConfig, ProviderType, build_provider};
        for kind in [
            ProviderType::OpenAI,
            ProviderType::Ollama,
            ProviderType::Passthrough,
        ] {
            let config = EmbeddingConfig {
                provider: kind.clone(),
                dimensions: 4,
                model: "configured-model".into(),
                api_key: Some("DO-NOT-PERSIST".into()),
                base_url: Some("http://secret-endpoint.invalid".into()),
                batch_size: None,
                timeout_secs: None,
                region: None,
                cache_embeddings: true,
                cache_max_entries: 10,
            };
            let provider = build_provider(&config, "doc: ", "query: ").await.unwrap();
            let Some(p) = provider.profile() else {
                assert_eq!(kind, ProviderType::Passthrough);
                continue;
            };
            assert_eq!(p.document_preprocessing.prefix, "doc: ");
            assert_eq!(p.query_preprocessing.prefix, "query: ");
            assert_eq!(p.dimension, 4);
            assert_eq!(p.model_revision, None);
            let json = serde_json::to_string(&p).unwrap();
            assert!(!json.contains("DO-NOT-PERSIST") && !json.contains("secret-endpoint"));
            let id = p.profile_id().unwrap();
            let outer = PrefixedProvider::new(provider, "outer: ".into(), "outer-query: ".into());
            let p = outer.profile().unwrap();
            assert_eq!(p.document_preprocessing.prefix, "doc: outer: ");
            assert_eq!(p.query_preprocessing.prefix, "query: outer-query: ");
            assert_ne!(p.profile_id().unwrap(), id);
        }
    }
}
