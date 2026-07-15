//! OpenAI embedding provider using the /v1/embeddings API.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// A string wrapper that redacts its contents in `Debug` output.
///
/// Use this for secrets (API keys, tokens) to prevent accidental
/// exposure in logs or debug-formatted error messages.
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: String) -> Self {
        Self(s)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// OpenAI embeddings provider using the /v1/embeddings API.
///
/// Supports text-embedding-3-small (1536-dim default) and
/// text-embedding-3-large (3072-dim default), with optional
/// dimension reduction via the `dimensions` parameter.
pub struct OpenAIProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    /// Effective output dimensionality. Equals `reduced_dim` if set,
    /// otherwise the model's native dimensionality.
    dim: usize,
    /// If Some, passed as the `dimensions` parameter to the API.
    /// OpenAI truncates and re-normalizes the output to this size.
    reduced_dim: Option<usize>,
    /// Base URL. Defaults to "https://api.openai.com".
    /// Override for Azure OpenAI or proxy endpoints.
    base_url: String,
}

impl OpenAIProvider {
    /// Create a new OpenAI embedding provider.
    pub fn new(api_key: String, model: String, reduced_dim: Option<usize>) -> Self {
        let dim = reduced_dim.unwrap_or(match model.as_str() {
            "text-embedding-3-small" => 1536,
            "text-embedding-3-large" => 3072,
            _ => 1536, // conservative default
        });

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            api_key: SecretString::new(api_key),
            model,
            dim,
            reduced_dim,
            base_url: "https://api.openai.com".to_string(),
        }
    }

    /// Override the base URL (Azure OpenAI, proxy, test server).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    /// Execute an embedding request with exponential backoff retry.
    ///
    /// Retries on:
    ///   - 429 (rate limit): up to MAX_RETRIES, exponential backoff + jitter
    ///   - 500/502/503 (server error): up to MAX_RETRIES, exponential backoff
    ///   - Network errors (connect failures, timeouts): up to MAX_RETRIES
    ///
    /// Does NOT retry on:
    ///   - 400 (bad request): returns RequestFailed immediately
    ///   - 401/403 (auth): returns Unavailable immediately
    ///   - 404 (model not found): returns ModelNotFound immediately
    ///   - Non-transient network errors (e.g., invalid URL): returns immediately
    async fn request_with_retry(
        &self,
        texts: &[&str],
    ) -> Result<EmbeddingResponse, EmbeddingError> {
        let request_body = EmbeddingRequest {
            model: &self.model,
            input: texts.to_vec(),
            dimensions: self.reduced_dim,
            encoding_format: "float",
        };

        let mut delay = Duration::from_secs(1);
        const MAX_RETRIES: u32 = 5;

        for attempt in 0..=MAX_RETRIES {
            let response = match self
                .client
                .post(format!("{}/v1/embeddings", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key.expose()))
                .header("Content-Type", "application/json")
                .json(&request_body)
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) if e.is_connect() || e.is_timeout() => {
                    if attempt == MAX_RETRIES {
                        return Err(EmbeddingError::Network(e));
                    }
                    warn!(
                        model = %self.model,
                        attempt = attempt + 1,
                        %e,
                        "OpenAI connection failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(60));
                    continue;
                }
                Err(e) => return Err(EmbeddingError::Network(e)),
            };

            let status = response.status();

            if status.is_success() {
                let body: EmbeddingResponse = response
                    .json()
                    .await
                    .map_err(|e| EmbeddingError::InvalidResponse(e.to_string()))?;
                debug!(
                    model = %self.model,
                    texts = texts.len(),
                    prompt_tokens = body.usage.prompt_tokens,
                    total_tokens = body.usage.total_tokens,
                    "OpenAI embedding request succeeded"
                );
                return Ok(body);
            }

            let error_body: Result<ErrorResponse, _> = response.json().await;

            match status.as_u16() {
                429 => {
                    if attempt == MAX_RETRIES {
                        warn!(
                            model = %self.model,
                            attempts = MAX_RETRIES + 1,
                            "OpenAI rate limit exhausted after all retries"
                        );
                        return Err(EmbeddingError::RateLimited {
                            retry_after_secs: delay.as_secs(),
                        });
                    }
                    // Exponential backoff with random jitter (0-500ms)
                    let jitter = Duration::from_millis(rand::random::<u64>() % 500);
                    warn!(
                        model = %self.model,
                        attempt = attempt + 1,
                        delay_ms = (delay + jitter).as_millis() as u64,
                        "OpenAI rate limited, retrying"
                    );
                    tokio::time::sleep(delay + jitter).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(60));
                }
                500 | 502 | 503 => {
                    if attempt == MAX_RETRIES {
                        let msg = error_body
                            .map(|e| e.error.message)
                            .unwrap_or_else(|_| "server error".into());
                        warn!(
                            model = %self.model,
                            status = status.as_u16(),
                            "OpenAI server error after all retries"
                        );
                        return Err(EmbeddingError::RequestFailed(msg));
                    }
                    warn!(
                        model = %self.model,
                        status = status.as_u16(),
                        attempt = attempt + 1,
                        "OpenAI server error, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(60));
                }
                400 => {
                    let msg = error_body
                        .map(|e| e.error.message)
                        .unwrap_or_else(|_| "bad request".into());
                    return Err(EmbeddingError::RequestFailed(msg));
                }
                401 | 403 => {
                    return Err(EmbeddingError::Unavailable(
                        "authentication failed -- check API key".into(),
                    ));
                }
                404 => {
                    return Err(EmbeddingError::ModelNotFound {
                        model: self.model.clone(),
                    });
                }
                _ => {
                    let msg = error_body
                        .map(|e| e.error.message)
                        .unwrap_or_else(|_| format!("HTTP {status}"));
                    return Err(EmbeddingError::RequestFailed(msg));
                }
            }
        }

        unreachable!("retry loop always returns or errors")
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OpenAIProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let response = self.request_with_retry(&[text]).await?;
        let data = response
            .data
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::InvalidResponse("empty response".into()))?;

        if data.embedding.len() != self.dim {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.dim,
                got: data.embedding.len(),
            });
        }

        Ok(data.embedding)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_results = Vec::with_capacity(texts.len());

        // OpenAI allows up to 2048 inputs per request.
        for chunk in texts.chunks(2048) {
            let response = self.request_with_retry(chunk).await?;

            // Response `data` items have an `index` field -- sort to
            // guarantee output order matches input order.
            let mut data = response.data;
            data.sort_by_key(|d| d.index);

            // Validate the number of returned embeddings matches the chunk size.
            if data.len() != chunk.len() {
                return Err(EmbeddingError::InvalidResponse(format!(
                    "expected {} embeddings, got {}",
                    chunk.len(),
                    data.len()
                )));
            }

            for item in data {
                if item.embedding.len() != self.dim {
                    return Err(EmbeddingError::DimensionMismatch {
                        expected: self.dim,
                        got: item.embedding.len(),
                    });
                }
                all_results.push(item.embedding);
            }
        }

        Ok(all_results)
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
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    encoding_format: &'a str, // always "float"
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
    usage: Usage,
}

#[derive(Deserialize)]
struct EmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: u32,
    total_tokens: u32,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    error_type: String,
    #[allow(dead_code)]
    code: Option<String>,
}
