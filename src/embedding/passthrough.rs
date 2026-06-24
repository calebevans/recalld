//! Passthrough (no-op) embedding provider for testing and pre-computed vectors.

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// A no-op provider for testing and pre-computed embedding use cases.
///
/// Deliberately errors on `embed()`. Its purpose is to let a namespace
/// exist with a fixed dimensionality while the caller supplies vectors
/// out-of-band via the memory creation API. Semantic search is not
/// available on passthrough namespaces -- only tag/metadata/graph
/// queries work.
pub struct PassthroughProvider {
    dim: usize,
}

impl PassthroughProvider {
    /// Create a new passthrough provider with the given dimensionality.
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for PassthroughProvider {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::RequestFailed(
            "PassthroughProvider does not generate embeddings. \
             Use the direct vector API to provide pre-computed embeddings."
                .into(),
        ))
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        "passthrough"
    }
}
