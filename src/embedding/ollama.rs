//! Ollama embedding provider using the local /api/embed endpoint.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// Ollama embeddings provider using the local /api/embed endpoint.
///
/// Ollama runs locally (default: `http://localhost:11434`) and supports
/// models like nomic-embed-text (768-dim), mxbai-embed-large (1024-dim),
/// and all-minilm (384-dim).
pub struct OllamaProvider {
    client: Client,
    /// Base URL. Defaults to "http://localhost:11434".
    base_url: String,
    model: String,
    dim: usize,
}

impl OllamaProvider {
    /// Create a new Ollama provider.
    ///
    /// Known model dimensions:
    ///   - nomic-embed-text: 768
    ///   - mxbai-embed-large: 1024
    ///   - all-minilm: 384
    pub fn new(model: String, dim: usize) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120)) // local models can be slow on first load
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            base_url: "http://localhost:11434".to_string(),
            model,
            dim,
        }
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    /// Check if Ollama is running and reachable.
    pub async fn health_check(&self) -> Result<(), EmbeddingError> {
        let response = self.client.get(&self.base_url).send().await.map_err(|e| {
            if e.is_connect() {
                EmbeddingError::Unavailable(
                    "Ollama is not running. Start it with `ollama serve`".into(),
                )
            } else if e.is_timeout() {
                EmbeddingError::Unavailable("Ollama connection timed out".into())
            } else {
                EmbeddingError::Network(e)
            }
        })?;

        if !response.status().is_success() {
            return Err(EmbeddingError::Unavailable(
                "Ollama returned non-200 status on health check".into(),
            ));
        }

        Ok(())
    }

    /// List available models on the Ollama instance.
    pub async fn list_models(&self) -> Result<Vec<String>, EmbeddingError> {
        let response = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await?;

        let tags: TagsResponse = response
            .json()
            .await
            .map_err(|e| EmbeddingError::InvalidResponse(e.to_string()))?;

        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OllamaProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let batch = self.embed_batch(&[text]).await?;
        batch
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::InvalidResponse("empty response".into()))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let request_body = EmbedRequest {
            model: &self.model,
            input: texts.to_vec(),
        };

        let response = self
            .client
            .post(format!("{}/api/embed", self.base_url))
            .json(&request_body)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    EmbeddingError::Unavailable(
                        "Ollama is not running. Start it with `ollama serve`".into(),
                    )
                } else {
                    EmbeddingError::Network(e)
                }
            })?;

        let status = response.status();

        if !status.is_success() {
            let error: Result<OllamaErrorResponse, _> = response.json().await;
            let msg = error
                .map(|e| e.error)
                .unwrap_or_else(|_| format!("HTTP {status}"));

            return match status.as_u16() {
                404 => {
                    warn!(model = %self.model, "Ollama model not found");
                    Err(EmbeddingError::ModelNotFound {
                        model: self.model.clone(),
                    })
                }
                _ => Err(EmbeddingError::RequestFailed(msg)),
            };
        }

        let body: EmbedResponse = response
            .json()
            .await
            .map_err(|e| EmbeddingError::InvalidResponse(e.to_string()))?;

        debug!(
            model = %self.model,
            texts = texts.len(),
            duration_ns = body.total_duration,
            prompt_eval_count = body.prompt_eval_count,
            "Ollama embedding request succeeded"
        );

        // Validate every returned vector's dimensionality.
        for embedding in &body.embeddings {
            if embedding.len() != self.dim {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: self.dim,
                    got: embedding.len(),
                });
            }
        }

        Ok(body.embeddings)
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

// ── Request/Response types (private) ────────────────────────────────

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
    #[serde(default)]
    total_duration: u64, // nanoseconds
    #[serde(default)]
    prompt_eval_count: u32,
}

#[derive(Deserialize)]
struct OllamaErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Deserialize)]
struct ModelInfo {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    details: ModelDetails,
}

#[derive(Default, Deserialize)]
struct ModelDetails {
    #[serde(default)]
    #[allow(dead_code)]
    family: String,
    #[serde(default)]
    #[allow(dead_code)]
    parameter_size: String,
}
