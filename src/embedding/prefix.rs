//! Contextual embedding prefix wrapper.
//!
//! Prepends configurable prefixes to text before delegating to the
//! inner embedding provider. Document embeddings (via `embed()` and
//! `embed_batch()`) use `document_prefix`. Query embeddings (via
//! `embed_query()`) use `query_prefix`.
//!
//! This implements the rule-based version of Anthropic's "Contextual
//! Retrieval" technique. The prefixes bridge the semantic gap between
//! declarative statements (memories) and questions (search queries).

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// Wraps any `EmbeddingProvider` with asymmetric prefix injection.
///
/// Document prefix is applied to `embed()` and `embed_batch()`.
/// Query prefix is applied to `embed_query()`.
///
/// If a prefix is empty, no prefixing occurs for that context.
pub struct PrefixedProvider {
    inner: Box<dyn EmbeddingProvider>,
    document_prefix: String,
    query_prefix: String,
}

impl PrefixedProvider {
    /// Create a new PrefixedProvider.
    ///
    /// # Arguments
    ///
    /// * `inner` - The embedding provider to wrap.
    /// * `document_prefix` - Prefix for document/ingest embeddings.
    ///   Pass empty string to disable.
    /// * `query_prefix` - Prefix for query/search embeddings.
    ///   Pass empty string to disable.
    pub fn new(
        inner: Box<dyn EmbeddingProvider>,
        document_prefix: String,
        query_prefix: String,
    ) -> Self {
        Self {
            inner,
            document_prefix,
            query_prefix,
        }
    }

    /// Prepend a prefix to text. Returns the text unchanged if the
    /// prefix is empty.
    fn prefixed(prefix: &str, text: &str) -> String {
        if prefix.is_empty() {
            text.to_string()
        } else {
            format!("{prefix}{text}")
        }
    }

    /// Returns true if document prefixing is active.
    pub fn has_document_prefix(&self) -> bool {
        !self.document_prefix.is_empty()
    }

    /// Returns true if query prefixing is active.
    pub fn has_query_prefix(&self) -> bool {
        !self.query_prefix.is_empty()
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for PrefixedProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if self.document_prefix.is_empty() {
            self.inner.embed(text).await
        } else {
            let prefixed = Self::prefixed(&self.document_prefix, text);
            self.inner.embed(&prefixed).await
        }
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if self.query_prefix.is_empty() {
            self.inner.embed_query(text).await
        } else {
            let prefixed = Self::prefixed(&self.query_prefix, text);
            self.inner.embed_query(&prefixed).await
        }
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if self.document_prefix.is_empty() {
            self.inner.embed_batch(texts).await
        } else {
            let prefixed: Vec<String> = texts
                .iter()
                .map(|t| Self::prefixed(&self.document_prefix, t))
                .collect();
            let refs: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();
            self.inner.embed_batch(&refs).await
        }
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Mock provider that records what text it receives.
    struct MockProvider {
        received: Arc<Mutex<Vec<String>>>,
        dims: usize,
    }

    impl MockProvider {
        fn new(dims: usize) -> (Self, Arc<Mutex<Vec<String>>>) {
            let received = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    received: received.clone(),
                    dims,
                },
                received,
            )
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for MockProvider {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            self.received.lock().unwrap().push(text.to_string());
            Ok(vec![0.0; self.dims])
        }

        async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            let mut received = self.received.lock().unwrap();
            for text in texts {
                received.push(text.to_string());
            }
            Ok(texts.iter().map(|_| vec![0.0; self.dims]).collect())
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        fn model_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn test_document_prefix_applied() {
        let (mock, received) = MockProvider::new(4);
        let provider = PrefixedProvider::new(
            Box::new(mock),
            "search_document: ".to_string(),
            "search_query: ".to_string(),
        );
        provider.embed("hello").await.unwrap();
        let texts = received.lock().unwrap();
        assert_eq!(texts[0], "search_document: hello");
    }

    #[tokio::test]
    async fn test_query_prefix_applied() {
        let (mock, received) = MockProvider::new(4);
        let provider = PrefixedProvider::new(
            Box::new(mock),
            "search_document: ".to_string(),
            "search_query: ".to_string(),
        );
        provider.embed_query("hello").await.unwrap();
        let texts = received.lock().unwrap();
        assert_eq!(texts[0], "search_query: hello");
    }

    #[tokio::test]
    async fn test_empty_document_prefix_passthrough() {
        let (mock, received) = MockProvider::new(4);
        let provider =
            PrefixedProvider::new(Box::new(mock), "".to_string(), "search_query: ".to_string());
        provider.embed("hello").await.unwrap();
        let texts = received.lock().unwrap();
        assert_eq!(texts[0], "hello");
    }

    #[tokio::test]
    async fn test_empty_query_prefix_passthrough() {
        let (mock, received) = MockProvider::new(4);
        let provider = PrefixedProvider::new(
            Box::new(mock),
            "search_document: ".to_string(),
            "".to_string(),
        );
        provider.embed_query("hello").await.unwrap();
        let texts = received.lock().unwrap();
        assert_eq!(texts[0], "hello");
    }

    #[tokio::test]
    async fn test_batch_prefix_applied() {
        let (mock, received) = MockProvider::new(4);
        let provider =
            PrefixedProvider::new(Box::new(mock), "doc: ".to_string(), "query: ".to_string());
        provider.embed_batch(&["a", "b"]).await.unwrap();
        let texts = received.lock().unwrap();
        assert_eq!(texts[0], "doc: a");
        assert_eq!(texts[1], "doc: b");
    }

    #[tokio::test]
    async fn test_batch_empty_prefix() {
        let (mock, received) = MockProvider::new(4);
        let provider = PrefixedProvider::new(Box::new(mock), "".to_string(), "query: ".to_string());
        provider.embed_batch(&["a", "b"]).await.unwrap();
        let texts = received.lock().unwrap();
        assert_eq!(texts[0], "a");
        assert_eq!(texts[1], "b");
    }

    #[tokio::test]
    async fn test_dimensions_passthrough() {
        let (mock, _) = MockProvider::new(768);
        let provider =
            PrefixedProvider::new(Box::new(mock), "doc: ".to_string(), "query: ".to_string());
        assert_eq!(provider.dimensions(), 768);
    }

    #[tokio::test]
    async fn test_model_name_passthrough() {
        let (mock, _) = MockProvider::new(4);
        let provider =
            PrefixedProvider::new(Box::new(mock), "doc: ".to_string(), "query: ".to_string());
        assert_eq!(provider.model_name(), "mock");
    }

    #[tokio::test]
    async fn test_has_document_prefix() {
        let (mock1, _) = MockProvider::new(4);
        let p1 = PrefixedProvider::new(Box::new(mock1), "doc: ".to_string(), "".to_string());
        assert!(p1.has_document_prefix());

        let (mock2, _) = MockProvider::new(4);
        let p2 = PrefixedProvider::new(Box::new(mock2), "".to_string(), "".to_string());
        assert!(!p2.has_document_prefix());
    }

    #[tokio::test]
    async fn test_has_query_prefix() {
        let (mock1, _) = MockProvider::new(4);
        let p1 = PrefixedProvider::new(Box::new(mock1), "".to_string(), "query: ".to_string());
        assert!(p1.has_query_prefix());

        let (mock2, _) = MockProvider::new(4);
        let p2 = PrefixedProvider::new(Box::new(mock2), "".to_string(), "".to_string());
        assert!(!p2.has_query_prefix());
    }

    /// Mock provider that distinguishes embed() from embed_query()
    /// by recording which method was called.
    struct AsymmetricMockProvider {
        calls: Arc<Mutex<Vec<(&'static str, String)>>>,
        dims: usize,
    }

    impl AsymmetricMockProvider {
        fn new(dims: usize) -> (Self, Arc<Mutex<Vec<(&'static str, String)>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    calls: calls.clone(),
                    dims,
                },
                calls,
            )
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for AsymmetricMockProvider {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            self.calls
                .lock()
                .unwrap()
                .push(("embed", text.to_string()));
            Ok(vec![1.0; self.dims])
        }

        async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            self.calls
                .lock()
                .unwrap()
                .push(("embed_query", text.to_string()));
            Ok(vec![2.0; self.dims])
        }

        async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            let mut calls = self.calls.lock().unwrap();
            for text in texts {
                calls.push(("embed_batch", text.to_string()));
            }
            Ok(texts.iter().map(|_| vec![1.0; self.dims]).collect())
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        fn model_name(&self) -> &str {
            "asymmetric-mock"
        }
    }

    #[tokio::test]
    async fn test_embed_query_delegates_to_inner_embed_query() {
        let (mock, calls) = AsymmetricMockProvider::new(4);
        let provider = PrefixedProvider::new(
            Box::new(mock),
            "doc: ".to_string(),
            "query: ".to_string(),
        );
        provider.embed_query("hello").await.unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "embed_query");
        assert_eq!(calls[0].1, "query: hello");
    }

    #[tokio::test]
    async fn test_embed_delegates_to_inner_embed() {
        let (mock, calls) = AsymmetricMockProvider::new(4);
        let provider = PrefixedProvider::new(
            Box::new(mock),
            "doc: ".to_string(),
            "query: ".to_string(),
        );
        provider.embed("hello").await.unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "embed");
        assert_eq!(calls[0].1, "doc: hello");
    }

    #[tokio::test]
    async fn test_embed_query_empty_prefix_delegates_to_inner_embed_query() {
        let (mock, calls) = AsymmetricMockProvider::new(4);
        let provider = PrefixedProvider::new(
            Box::new(mock),
            "doc: ".to_string(),
            "".to_string(),
        );
        provider.embed_query("hello").await.unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "embed_query");
        assert_eq!(calls[0].1, "hello");
    }
}
