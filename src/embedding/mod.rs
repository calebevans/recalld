//! Pluggable embedding provider interface for Recalld.
//!
//! Defines the `EmbeddingProvider` trait and concrete implementations
//! for OpenAI, Ollama, and a passthrough (no-op) provider. Includes
//! an optional caching wrapper, a contextual prefix wrapper, and a
//! factory function for constructing providers from configuration.

mod cache;
mod ollama;
mod openai;
mod passthrough;
mod prefix;
mod provider;

#[cfg(feature = "bedrock")]
mod bedrock;

use async_trait::async_trait;
use thiserror::Error;

pub use cache::CachedProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAIProvider;
pub use passthrough::PassthroughProvider;
pub use prefix::PrefixedProvider;
pub use provider::{EmbeddingConfig, ProviderType, build_provider};

#[cfg(feature = "bedrock")]
pub use bedrock::BedrockProvider;

/// Errors that can occur during embedding operations.
#[derive(Debug, Error)]
pub enum EmbeddingError {
    /// Provider is not reachable or not configured.
    #[error("provider unavailable: {0}")]
    Unavailable(String),

    /// Upstream returned HTTP 429. Caller should wait `retry_after_secs`
    /// before retrying. The provider's internal retry loop has already
    /// been exhausted when this variant surfaces.
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    /// A non-retryable request failure (400, 401, 403, or unknown status).
    #[error("request failed: {0}")]
    RequestFailed(String),

    /// Response body was unparseable or structurally unexpected.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// The configured model does not exist on the provider.
    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    /// Input text exceeds the model's token limit.
    #[error("text too long: {tokens} tokens exceeds limit of {limit}")]
    TextTooLong { tokens: usize, limit: usize },

    /// Returned vector dimensionality does not match `dimensions()`.
    #[error("dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    /// Low-level transport error (TCP, TLS, DNS).
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}

/// A provider that converts text into embedding vectors.
///
/// Implementations handle the specifics of calling an embedding API
/// (OpenAI, Ollama, etc.) and returning f32 vectors.
///
/// All methods are async because embedding providers are typically
/// network services with non-trivial latency.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single text string (document/ingest context).
    ///
    /// Returns a vector of exactly `self.dimensions()` length.
    /// The vector MUST be L2-normalized (unit length). All supported
    /// providers (OpenAI, Ollama/nomic) output normalized vectors
    /// natively; if a future provider does not, the implementation
    /// must normalize before returning.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Embed a single text string in query context.
    ///
    /// Identical to `embed()` except that implementations aware of
    /// asymmetric retrieval (e.g., PrefixedProvider) may apply a
    /// different prefix. The default delegates to `embed()`.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.embed(text).await
    }

    /// Embed multiple texts in a single batch (document/ingest context).
    ///
    /// Returns one vector per input text, in the same order as the
    /// input slice. Providers with native batch support (OpenAI)
    /// should override the default implementation for efficiency.
    ///
    /// The default calls `embed` sequentially -- acceptable for Ollama
    /// (local, low latency) but suboptimal for OpenAI (network round trips).
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    /// The fixed dimensionality of vectors this provider produces.
    ///
    /// This value is immutable for the lifetime of the provider instance
    /// and must match the namespace's configured dimensionality.
    fn dimensions(&self) -> usize;

    /// Human-readable model name for logging and config display.
    fn model_name(&self) -> &str;
}
