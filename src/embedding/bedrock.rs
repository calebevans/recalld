//! AWS Bedrock embedding provider using the InvokeModel API.

use std::sync::Arc;
use std::time::Duration;

use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::SdkError;
use aws_sdk_bedrockruntime::operation::invoke_model::InvokeModelError;
use aws_sdk_bedrockruntime::primitives::Blob;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// Model family for Bedrock embedding models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    /// Amazon Titan Embeddings (V1/V2).
    Titan,
    /// Cohere Embed (v3+).
    Cohere,
}

/// Detect the model family from a Bedrock model ID.
fn detect_family(model_id: &str) -> Result<ModelFamily, EmbeddingError> {
    if model_id.contains("titan-embed") {
        Ok(ModelFamily::Titan)
    } else if model_id.contains("cohere.embed") {
        Ok(ModelFamily::Cohere)
    } else {
        Err(EmbeddingError::ModelNotFound {
            model: model_id.to_string(),
        })
    }
}

/// Validate that dimensions are supported for a given model family.
fn validate_dimensions(family: ModelFamily, dim: usize) -> Result<(), EmbeddingError> {
    if family == ModelFamily::Titan && !matches!(dim, 256 | 512 | 1024) {
        return Err(EmbeddingError::RequestFailed(format!(
            "Titan embedding models only support dimensions 256, 512, or 1024, got {}",
            dim
        )));
    }
    Ok(())
}

/// Whether an SDK error is transient and should be retried.
fn is_retryable(err: &SdkError<InvokeModelError>) -> bool {
    match err {
        SdkError::ServiceError(ctx) => matches!(
            ctx.err(),
            InvokeModelError::ThrottlingException(_)
                | InvokeModelError::ModelTimeoutException(_)
                | InvokeModelError::ModelNotReadyException(_)
                | InvokeModelError::InternalServerException(_)
                | InvokeModelError::ServiceUnavailableException(_)
        ),
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => true,
        _ => false,
    }
}

/// Map an AWS SDK error to an EmbeddingError.
fn map_sdk_error(model_id: &str, err: SdkError<InvokeModelError>) -> EmbeddingError {
    match &err {
        SdkError::ServiceError(service_err) => match service_err.err() {
            InvokeModelError::ThrottlingException(_) => EmbeddingError::RateLimited {
                retry_after_secs: 5,
            },
            InvokeModelError::AccessDeniedException(_) => {
                EmbeddingError::Unavailable(format!("Bedrock access denied: {}", err))
            }
            InvokeModelError::ResourceNotFoundException(_) => EmbeddingError::ModelNotFound {
                model: model_id.to_string(),
            },
            InvokeModelError::ValidationException(_) => {
                EmbeddingError::RequestFailed(format!("Bedrock validation error: {}", err))
            }
            InvokeModelError::ModelTimeoutException(_) => {
                EmbeddingError::Unavailable("Bedrock model timed out".into())
            }
            InvokeModelError::ModelNotReadyException(_) => {
                EmbeddingError::Unavailable("Bedrock model not ready".into())
            }
            InvokeModelError::ModelErrorException(_) => {
                EmbeddingError::RequestFailed(format!("Bedrock model error: {}", err))
            }
            _ => EmbeddingError::RequestFailed(format!("Bedrock service error: {}", err)),
        },
        SdkError::TimeoutError(_) => {
            EmbeddingError::Unavailable("Bedrock request timed out".into())
        }
        SdkError::DispatchFailure(_) => {
            EmbeddingError::Unavailable(format!("Bedrock connection failed: {}", err))
        }
        _ => EmbeddingError::RequestFailed(format!("Bedrock error: {}", err)),
    }
}

const MAX_RETRIES: u32 = 3;

/// Invoke a Bedrock model with a JSON body and return the response bytes.
///
/// Retries on transient errors (throttling, timeouts, server errors)
/// with exponential backoff and jitter. Does NOT retry on auth,
/// validation, or model-not-found errors.
async fn invoke_model_raw(
    client: &Client,
    model_id: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>, EmbeddingError> {
    let mut delay = Duration::from_secs(1);

    for attempt in 0..=MAX_RETRIES {
        let result = client
            .invoke_model()
            .model_id(model_id)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(body.clone()))
            .send()
            .await;

        match result {
            Ok(output) => return Ok(output.body().as_ref().to_vec()),
            Err(err) => {
                if attempt < MAX_RETRIES && is_retryable(&err) {
                    let jitter = Duration::from_millis(rand::random::<u64>() % 500);
                    warn!(
                        model = %model_id,
                        attempt = attempt + 1,
                        delay_ms = (delay + jitter).as_millis() as u64,
                        error = %err,
                        "Bedrock transient error, retrying"
                    );
                    tokio::time::sleep(delay + jitter).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(30));
                    continue;
                }

                if is_retryable(&err) {
                    warn!(
                        model = %model_id,
                        attempts = MAX_RETRIES + 1,
                        "Bedrock error persisted after all retries"
                    );
                }
                return Err(map_sdk_error(model_id, err));
            }
        }
    }

    unreachable!()
}

/// AWS Bedrock embeddings provider using the InvokeModel API.
///
/// Supports Amazon Titan Embeddings and Cohere Embed models via
/// the Bedrock runtime. Authentication uses the standard AWS
/// credential chain (environment variables, ~/.aws/credentials,
/// IAM roles, etc.).
pub struct BedrockProvider {
    client: Client,
    model_id: String,
    dim: usize,
    #[allow(dead_code)]
    region: String,
    family: ModelFamily,
}

impl BedrockProvider {
    /// Create a new Bedrock embedding provider.
    ///
    /// Loads AWS configuration from the standard credential chain
    /// and creates a Bedrock Runtime client for the specified region.
    pub async fn new(model_id: String, dim: usize, region: String) -> Result<Self, EmbeddingError> {
        let family = detect_family(&model_id)?;
        validate_dimensions(family, dim)?;

        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.clone()))
            .load()
            .await;

        let client = Client::new(&sdk_config);

        Ok(Self {
            client,
            model_id,
            dim,
            region,
            family,
        })
    }

    /// Embed a single text using Titan.
    async fn embed_titan(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let request = TitanRequest {
            input_text: text,
            dimensions: self.dim,
            normalize: true,
        };
        let body = serde_json::to_vec(&request)
            .map_err(|e| EmbeddingError::RequestFailed(e.to_string()))?;

        let response_bytes = invoke_model_raw(&self.client, &self.model_id, body).await?;
        let response: TitanResponse = serde_json::from_slice(&response_bytes)
            .map_err(|e| EmbeddingError::InvalidResponse(e.to_string()))?;

        if response.embedding.len() != self.dim {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.dim,
                got: response.embedding.len(),
            });
        }

        debug!(
            model = %self.model_id,
            tokens = response.input_text_token_count,
            "Titan embedding request succeeded"
        );

        Ok(response.embedding)
    }

    /// Embed texts using Cohere.
    async fn embed_cohere(
        &self,
        texts: &[&str],
        input_type: &str,
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let request = CohereRequest {
            texts: texts.to_vec(),
            input_type,
            truncate: "NONE",
        };
        let body = serde_json::to_vec(&request)
            .map_err(|e| EmbeddingError::RequestFailed(e.to_string()))?;

        let response_bytes = invoke_model_raw(&self.client, &self.model_id, body).await?;
        let response: CohereResponse = serde_json::from_slice(&response_bytes)
            .map_err(|e| EmbeddingError::InvalidResponse(e.to_string()))?;

        for embedding in &response.embeddings {
            if embedding.len() != self.dim {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: self.dim,
                    got: embedding.len(),
                });
            }
        }

        debug!(
            model = %self.model_id,
            texts = texts.len(),
            "Cohere embedding request succeeded"
        );

        Ok(response.embeddings)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for BedrockProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        match self.family {
            ModelFamily::Titan => self.embed_titan(text).await,
            ModelFamily::Cohere => {
                let results = self.embed_cohere(&[text], "search_document").await?;
                results
                    .into_iter()
                    .next()
                    .ok_or_else(|| EmbeddingError::InvalidResponse("empty response".into()))
            }
        }
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        match self.family {
            ModelFamily::Titan => self.embed(text).await,
            ModelFamily::Cohere => {
                let results = self.embed_cohere(&[text], "search_query").await?;
                results
                    .into_iter()
                    .next()
                    .ok_or_else(|| EmbeddingError::InvalidResponse("empty response".into()))
            }
        }
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        match self.family {
            ModelFamily::Titan => {
                let semaphore = Arc::new(Semaphore::new(10));
                let mut join_set = JoinSet::new();

                for (idx, text) in texts.iter().enumerate() {
                    let client = self.client.clone();
                    let model_id = self.model_id.clone();
                    let dim = self.dim;
                    let text = text.to_string();
                    let sem = semaphore.clone();

                    join_set.spawn(async move {
                        let _permit = sem.acquire().await.expect("semaphore closed");
                        let request = TitanRequest {
                            input_text: &text,
                            dimensions: dim,
                            normalize: true,
                        };
                        let body = serde_json::to_vec(&request)
                            .map_err(|e| EmbeddingError::RequestFailed(e.to_string()))?;

                        let response_bytes = invoke_model_raw(&client, &model_id, body).await?;
                        let response: TitanResponse = serde_json::from_slice(&response_bytes)
                            .map_err(|e| EmbeddingError::InvalidResponse(e.to_string()))?;

                        if response.embedding.len() != dim {
                            return Err(EmbeddingError::DimensionMismatch {
                                expected: dim,
                                got: response.embedding.len(),
                            });
                        }

                        Ok((idx, response.embedding))
                    });
                }

                let mut results: Vec<(usize, Vec<f32>)> = Vec::with_capacity(texts.len());
                while let Some(result) = join_set.join_next().await {
                    let pair = result.map_err(|e| {
                        EmbeddingError::RequestFailed(format!("task panicked: {}", e))
                    })??;
                    results.push(pair);
                }

                results.sort_by_key(|(idx, _)| *idx);
                Ok(results.into_iter().map(|(_, emb)| emb).collect())
            }
            ModelFamily::Cohere => {
                let mut all_results = Vec::with_capacity(texts.len());
                for chunk in texts.chunks(96) {
                    let embeddings = self.embed_cohere(chunk, "search_document").await?;
                    all_results.extend(embeddings);
                }
                Ok(all_results)
            }
        }
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        &self.model_id
    }
}

// ── Titan request/response types (private) ────────────────────────

#[derive(Serialize)]
struct TitanRequest<'a> {
    #[serde(rename = "inputText")]
    input_text: &'a str,
    dimensions: usize,
    normalize: bool,
}

#[derive(Deserialize)]
struct TitanResponse {
    embedding: Vec<f32>,
    #[serde(rename = "inputTextTokenCount")]
    #[serde(default)]
    input_text_token_count: u32,
}

// ── Cohere request/response types (private) ───────────────────────

#[derive(Serialize)]
struct CohereRequest<'a> {
    texts: Vec<&'a str>,
    input_type: &'a str,
    truncate: &'a str,
}

#[derive(Deserialize)]
struct CohereResponse {
    embeddings: Vec<Vec<f32>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_family_titan() {
        assert_eq!(
            detect_family("amazon.titan-embed-text-v2:0").unwrap(),
            ModelFamily::Titan
        );
        assert_eq!(
            detect_family("amazon.titan-embed-text-v1").unwrap(),
            ModelFamily::Titan
        );
    }

    #[test]
    fn test_detect_family_cohere() {
        assert_eq!(
            detect_family("cohere.embed-english-v3").unwrap(),
            ModelFamily::Cohere
        );
        assert_eq!(
            detect_family("cohere.embed-multilingual-v3").unwrap(),
            ModelFamily::Cohere
        );
    }

    #[test]
    fn test_detect_family_unknown() {
        assert!(detect_family("some-unknown-model").is_err());
    }

    #[test]
    fn test_titan_dimension_validation_valid() {
        for dim in [256, 512, 1024] {
            assert!(validate_dimensions(ModelFamily::Titan, dim).is_ok());
        }
    }

    #[test]
    fn test_titan_dimension_validation_invalid() {
        for dim in [128, 384, 768, 2048] {
            assert!(validate_dimensions(ModelFamily::Titan, dim).is_err());
        }
    }

    #[test]
    fn test_cohere_dimension_validation() {
        for dim in [256, 384, 768, 1024] {
            assert!(validate_dimensions(ModelFamily::Cohere, dim).is_ok());
        }
    }

    #[test]
    fn test_titan_request_serialization() {
        let request = TitanRequest {
            input_text: "hello world",
            dimensions: 1024,
            normalize: true,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["inputText"], "hello world");
        assert_eq!(json["dimensions"], 1024);
        assert_eq!(json["normalize"], true);
    }

    #[test]
    fn test_titan_response_deserialization() {
        let json = r#"{"embedding": [0.1, 0.2, 0.3], "inputTextTokenCount": 5}"#;
        let response: TitanResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.embedding, vec![0.1, 0.2, 0.3]);
        assert_eq!(response.input_text_token_count, 5);
    }

    #[test]
    fn test_cohere_request_serialization() {
        let request = CohereRequest {
            texts: vec!["hello", "world"],
            input_type: "search_document",
            truncate: "NONE",
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["texts"], serde_json::json!(["hello", "world"]));
        assert_eq!(json["input_type"], "search_document");
        assert_eq!(json["truncate"], "NONE");
    }

    #[test]
    fn test_cohere_response_deserialization() {
        let json = r#"{"embeddings": [[0.1, 0.2], [0.3, 0.4]], "id": "test", "texts": ["a", "b"]}"#;
        let response: CohereResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.embeddings.len(), 2);
        assert_eq!(response.embeddings[0], vec![0.1, 0.2]);
        assert_eq!(response.embeddings[1], vec![0.3, 0.4]);
    }
}
