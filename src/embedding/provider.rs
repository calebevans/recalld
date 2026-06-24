//! Embedding configuration types and provider factory.

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::embedding::cache::CachedProvider;
use crate::embedding::ollama::OllamaProvider;
use crate::embedding::openai::OpenAIProvider;
use crate::embedding::passthrough::PassthroughProvider;
use crate::embedding::prefix::PrefixedProvider;
use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// Per-namespace embedding provider configuration.
///
/// Stored in the namespace metadata (meta.db). The `api_key` field is
/// intentionally excluded from serialization -- it is resolved at runtime
/// from environment variables or the config file, never persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Which provider backend to use.
    pub provider: ProviderType,

    /// Fixed dimensionality for this namespace. Set at creation time,
    /// immutable thereafter. Changing this requires a namespace migration.
    pub dimensions: usize,

    /// Model identifier (e.g., "text-embedding-3-small", "nomic-embed-text").
    pub model: String,

    /// API key for authenticated providers (OpenAI). Resolved from:
    ///   1. This field (if set explicitly in config)
    ///   2. Environment variable `OPENAI_API_KEY`
    /// Never serialized to disk.
    #[serde(skip_serializing)]
    pub api_key: Option<String>,

    /// Base URL override. If None, each provider uses its default:
    ///   - OpenAI: "https://api.openai.com"
    ///   - Ollama: "http://localhost:11434"
    pub base_url: Option<String>,

    /// Maximum texts per batch for `embed_batch`. If None, provider
    /// defaults apply (OpenAI: 2048, Ollama: unlimited).
    pub batch_size: Option<usize>,

    /// HTTP request timeout in seconds. If None, provider defaults
    /// apply (OpenAI: 30s, Ollama: 120s).
    pub timeout_secs: Option<u64>,

    /// Whether to wrap the provider in a CachedProvider. Default: false.
    #[serde(default)]
    pub cache_embeddings: bool,

    /// Maximum entries in the embedding cache. Only used when
    /// `cache_embeddings` is true. Default: 1000.
    #[serde(default = "default_cache_max")]
    pub cache_max_entries: u64,
}

fn default_cache_max() -> u64 {
    1000
}

/// Which embedding provider backend to use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProviderType {
    /// OpenAI embedding API (text-embedding-3-small, text-embedding-3-large).
    OpenAI,
    /// Ollama local embedding server (nomic-embed-text, mxbai-embed-large, etc.).
    Ollama,
    /// Pre-computed embeddings supplied by the caller.
    Passthrough,
}

/// Construct the appropriate provider from configuration.
///
/// This is the only place where provider structs are instantiated.
/// All other code interacts with `Box<dyn EmbeddingProvider>`.
///
/// # Wrapping order
///
/// `PrefixedProvider(CachedProvider(inner))` -- the cache layer wraps
/// the raw provider so that cache keys are computed on unprefixed text.
/// The prefix layer wraps the cache so that it prepends the prefix
/// before delegating. Since the prefixed text (e.g., "search_document: X"
/// vs "search_query: X") is what reaches the cache, document and query
/// embeddings of the same raw text produce different cache keys.
pub fn build_provider(
    config: &EmbeddingConfig,
    document_prefix: &str,
    query_prefix: &str,
) -> Result<Box<dyn EmbeddingProvider>, EmbeddingError> {
    let inner: Box<dyn EmbeddingProvider> = match config.provider {
        ProviderType::OpenAI => {
            let api_key = config
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .ok_or_else(|| {
                    EmbeddingError::Unavailable(
                        "OPENAI_API_KEY not set in config or environment".into(),
                    )
                })?;

            let mut provider = OpenAIProvider::new(
                api_key,
                config.model.clone(),
                Some(config.dimensions),
            );

            if let Some(ref url) = config.base_url {
                provider = provider.with_base_url(url.clone());
            }

            info!(
                provider = "openai",
                model = %config.model,
                dimensions = config.dimensions,
                "Built OpenAI embedding provider"
            );

            Box::new(provider)
        }

        ProviderType::Ollama => {
            let mut provider = OllamaProvider::new(
                config.model.clone(),
                config.dimensions,
            );

            if let Some(ref url) = config.base_url {
                provider = provider.with_base_url(url.clone());
            }

            info!(
                provider = "ollama",
                model = %config.model,
                dimensions = config.dimensions,
                "Built Ollama embedding provider"
            );

            Box::new(provider)
        }

        ProviderType::Passthrough => {
            info!(
                provider = "passthrough",
                dimensions = config.dimensions,
                "Built passthrough embedding provider"
            );

            Box::new(PassthroughProvider::new(config.dimensions))
        }
    };

    // Layer 1: Embedding cache (innermost, caches raw/unprefixed text).
    let cached: Box<dyn EmbeddingProvider> = if config.cache_embeddings {
        info!(
            max_entries = config.cache_max_entries,
            "Wrapping embedding provider with cache layer"
        );
        Box::new(CachedProvider::new(inner, config.cache_max_entries))
    } else {
        inner
    };

    // Layer 2: Contextual prefix (outermost, so the cache sees
    // already-prefixed text and document/query embeddings get
    // distinct cache keys).
    if !document_prefix.is_empty() || !query_prefix.is_empty() {
        info!(
            document_prefix = document_prefix,
            query_prefix = query_prefix,
            "Wrapping embedding provider with contextual prefix layer"
        );
        Ok(Box::new(PrefixedProvider::new(
            cached,
            document_prefix.to_string(),
            query_prefix.to_string(),
        )))
    } else {
        Ok(cached)
    }
}
