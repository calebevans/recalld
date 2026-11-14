//! Optional, provider-local embedding cache. Keys retain the complete input
//! and task so hash collisions and asymmetric retrieval cannot alias results.

use std::collections::HashMap;

use moka::future::Cache;

use crate::embedding::{EmbeddingError, EmbeddingProvider};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Task {
    Document,
    Query,
}

/// A bounded-entry cache scoped to one immutable provider instance.
/// Provider/model identity is implicit in ownership, while task and exact
/// (possibly prefixed) text are explicit. Rebuilding a provider starts empty.
/// Retaining text costs memory; `max_entries` remains an entry count, not bytes.
pub struct CachedProvider {
    inner: Box<dyn EmbeddingProvider>,
    cache: Cache<(Task, String), Vec<f32>>,
}

impl CachedProvider {
    pub fn new(inner: Box<dyn EmbeddingProvider>, max_entries: u64) -> Self {
        Self {
            inner,
            cache: Cache::builder().max_capacity(max_entries).build(),
        }
    }

    fn validate(&self, embedding: &[f32]) -> Result<(), EmbeddingError> {
        if embedding.len() != self.inner.dimensions() {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.inner.dimensions(),
                got: embedding.len(),
            });
        }
        if embedding.iter().any(|value| !value.is_finite())
            || !embedding.iter().any(|value| *value != 0.0)
        {
            return Err(EmbeddingError::InvalidResponse(
                "embedding must be finite and nonzero".into(),
            ));
        }
        Ok(())
    }

    async fn cached_embed(&self, task: Task, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let key = (task, text.to_owned());
        if let Some(embedding) = self.cache.get(&key).await {
            return Ok(embedding);
        }
        let embedding = match task {
            Task::Document => self.inner.embed(text).await?,
            Task::Query => self.inner.embed_query(text).await?,
        };
        self.validate(&embedding)?;
        self.cache.insert(key, embedding.clone()).await;
        Ok(embedding)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for CachedProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.cached_embed(Task::Document, text).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.cached_embed(Task::Query, text).await
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut results = vec![None; texts.len()];
        let mut misses: Vec<&str> = Vec::new();
        let mut miss_positions = HashMap::new();
        let mut pending = Vec::new();
        for (index, text) in texts.iter().enumerate() {
            if let Some(embedding) = self.cache.get(&(Task::Document, (*text).to_owned())).await {
                results[index] = Some(embedding);
            } else {
                let slot = *miss_positions.entry(*text).or_insert_with(|| {
                    misses.push(text);
                    misses.len() - 1
                });
                pending.push((index, slot));
            }
        }
        if !misses.is_empty() {
            let embeddings = self.inner.embed_batch(&misses).await?;
            if embeddings.len() != misses.len() {
                return Err(EmbeddingError::InvalidResponse(format!(
                    "provider returned {} embeddings for {} inputs",
                    embeddings.len(),
                    misses.len()
                )));
            }
            // Validate the entire response before caching any of its entries.
            for embedding in &embeddings {
                self.validate(embedding)?;
            }
            for (text, embedding) in misses.iter().zip(&embeddings) {
                self.cache
                    .insert((Task::Document, (*text).to_owned()), embedding.clone())
                    .await;
            }
            for (index, slot) in pending {
                results[index] = Some(embeddings[slot].clone());
            }
        }
        Ok(results
            .into_iter()
            .map(|embedding| embedding.expect("cache hit or validated batch result"))
            .collect())
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

    type Calls = Arc<Mutex<Vec<Vec<String>>>>;

    struct Provider {
        calls: Calls,
        broken: Option<Vec<Vec<f32>>>,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for Provider {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            self.calls
                .lock()
                .unwrap()
                .push(vec![format!("document:{text}")]);
            Ok(self
                .broken
                .as_ref()
                .and_then(|values| values.first())
                .cloned()
                .unwrap_or_else(|| {
                    if text == "b" {
                        vec![0.0, 1.0]
                    } else {
                        vec![1.0, 0.0]
                    }
                }))
        }
        async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            self.calls
                .lock()
                .unwrap()
                .push(vec![format!("query:{text}")]);
            Ok(self
                .broken
                .as_ref()
                .and_then(|values| values.first())
                .cloned()
                .unwrap_or_else(|| vec![0.0, -1.0]))
        }
        async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            self.calls
                .lock()
                .unwrap()
                .push(texts.iter().map(|s| (*s).to_owned()).collect());
            Ok(self.broken.clone().unwrap_or_else(|| {
                texts
                    .iter()
                    .map(|s| {
                        if *s == "b" {
                            vec![0.0, 1.0]
                        } else {
                            vec![1.0, 0.0]
                        }
                    })
                    .collect()
            }))
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn model_name(&self) -> &str {
            "synthetic"
        }
    }

    fn provider(broken: Option<Vec<Vec<f32>>>) -> (CachedProvider, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            CachedProvider::new(
                Box::new(Provider {
                    calls: calls.clone(),
                    broken,
                }),
                100,
            ),
            calls,
        )
    }

    #[tokio::test]
    async fn query_and_document_do_not_alias_in_either_order() {
        for query_first in [true, false] {
            let (cache, calls) = provider(None);
            if query_first {
                cache.embed_query("same").await.unwrap();
            }
            assert_eq!(cache.embed("same").await.unwrap(), vec![1.0, 0.0]);
            assert_eq!(cache.embed_query("same").await.unwrap(), vec![0.0, -1.0]);
            cache.embed("same").await.unwrap();
            cache.embed_query("same").await.unwrap();
            assert_eq!(calls.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn batch_deduplicates_misses_and_preserves_order_with_hits() {
        let (cache, calls) = provider(None);
        cache.embed("b").await.unwrap();
        let output = cache.embed_batch(&["a", "b", "a", "c", "b"]).await.unwrap();
        assert_eq!(
            output,
            vec![
                vec![1.0, 0.0],
                vec![0.0, 1.0],
                vec![1.0, 0.0],
                vec![1.0, 0.0],
                vec![0.0, 1.0]
            ]
        );
        assert_eq!(calls.lock().unwrap()[1], vec!["a", "c"]);
        cache.embed_batch(&["c", "a", "b"]).await.unwrap();
        assert!(cache.embed_batch(&[]).await.unwrap().is_empty());
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn invalid_batches_never_populate_any_cache_entry() {
        for broken in [
            vec![vec![1.0, 0.0]],
            vec![vec![1.0, 0.0], vec![f32::NAN, 0.0]],
            vec![vec![1.0, 0.0], vec![0.0, 0.0]],
            vec![vec![1.0, 0.0], vec![1.0]],
        ] {
            let (cache, calls) = provider(Some(broken));
            assert!(cache.embed_batch(&["a", "b"]).await.is_err());
            cache.embed("a").await.unwrap();
            assert_eq!(
                calls.lock().unwrap().len(),
                2,
                "valid prefix was not cached from a failed batch"
            );
        }
    }
    #[tokio::test]
    async fn invalid_single_results_are_not_cached_for_either_task() {
        for invalid in [
            vec![0.0, 0.0],
            vec![f32::INFINITY, 1.0],
            vec![f32::NAN, 0.0],
            vec![1.0],
        ] {
            let (cache, calls) = provider(Some(vec![invalid]));
            for _ in 0..2 {
                assert!(cache.embed("bad").await.is_err());
                assert!(cache.embed_query("bad").await.is_err());
            }
            assert_eq!(calls.lock().unwrap().len(), 4);
        }
    }
}
