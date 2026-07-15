//! Optional embedding cache layer wrapping any `EmbeddingProvider`.
//!
//! `CachedProvider` wraps any `EmbeddingProvider` with a moka in-memory
//! cache keyed on the 64-bit hash of input text. It is opt-in via the
//! `cache_embeddings` config flag and has zero overhead when disabled
//! (the caller simply does not wrap the provider).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use moka::future::Cache;

use crate::embedding::{EmbeddingError, EmbeddingProvider};

/// Wraps any `EmbeddingProvider` with an in-memory cache.
///
/// Cache key: 64-bit hash of the input text.
/// Cache value: the embedding vector (`Vec<f32>`).
///
/// The cache is bounded by entry count with TinyLFU eviction.
/// No TTL/TTI -- embeddings are deterministic for a given model
/// and never go stale.
pub struct CachedProvider {
    /// The underlying embedding provider.
    inner: Box<dyn EmbeddingProvider>,
    /// moka cache: hash(text) -> embedding vector.
    cache: Cache<u64, Vec<f32>>,
}

impl CachedProvider {
    /// Create a new cached provider wrapping the given inner provider.
    ///
    /// # Arguments
    ///
    /// * `inner` - The embedding provider to wrap.
    /// * `max_entries` - Maximum number of cached embeddings. When full,
    ///   TinyLFU eviction removes the least valuable entry.
    pub fn new(inner: Box<dyn EmbeddingProvider>, max_entries: u64) -> Self {
        let cache = Cache::builder().max_capacity(max_entries).build();

        Self { inner, cache }
    }

    /// Hash the input text to produce a cache key.
    ///
    /// Uses `DefaultHasher` (SipHash-1-3 in current std). Fast and
    /// sufficient for cache deduplication -- collisions produce a
    /// wrong-but-valid embedding, which is acceptable for a cache.
    fn hash_text(text: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for CachedProvider {
    /// Embed a single text string, checking the cache first.
    ///
    /// On cache hit: returns the cached vector immediately (no API call).
    /// On cache miss: calls the inner provider, inserts the result into
    /// the cache, and returns it.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let key = Self::hash_text(text);

        // Cache hit: return immediately.
        if let Some(cached) = self.cache.get(&key).await {
            return Ok(cached);
        }

        // Cache miss: call inner provider.
        let embedding = self.inner.embed(text).await?;

        // Insert into cache. Clone is necessary because moka takes
        // ownership and we return a separate owned Vec.
        self.cache.insert(key, embedding.clone()).await;

        Ok(embedding)
    }

    /// Embed a single text string in query context, checking the cache first.
    ///
    /// Delegates to `inner.embed_query()` on cache miss, which is
    /// important when the inner provider is a `PrefixedProvider` --
    /// it ensures the query prefix is used instead of the document prefix.
    ///
    /// When wrapping order is `PrefixedProvider(CachedProvider(inner))`,
    /// the text arriving here is already prefixed, so cache keys for
    /// document and query embeddings of the same raw text are naturally
    /// distinct (the prefixed strings differ).
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let key = Self::hash_text(text);

        if let Some(cached) = self.cache.get(&key).await {
            return Ok(cached);
        }

        let embedding = self.inner.embed_query(text).await?;
        self.cache.insert(key, embedding.clone()).await;

        Ok(embedding)
    }

    /// Embed multiple texts with cache-aware batching.
    ///
    /// # Algorithm
    ///
    /// 1. Compute hash for each input text.
    /// 2. Check cache for each hash. Partition into hits and misses.
    /// 3. Call `inner.embed_batch()` with only the miss texts.
    /// 4. Insert miss results into cache.
    /// 5. Reassemble the full result vector in input order.
    ///
    /// This is the primary optimization path: batch imports with
    /// duplicate texts (e.g., repeated boilerplate) only embed each
    /// unique text once.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let len = texts.len();

        // Step 1-2: Compute hashes and check cache.
        let mut results: Vec<Option<Vec<f32>>> = vec![None; len];
        let mut miss_indices = Vec::new();
        let mut miss_texts: Vec<&str> = Vec::new();

        for (i, text) in texts.iter().enumerate() {
            let key = Self::hash_text(text);
            if let Some(cached) = self.cache.get(&key).await {
                results[i] = Some(cached);
            } else {
                miss_indices.push(i);
                miss_texts.push(text);
            }
        }

        // Step 3: Embed only the misses.
        if !miss_texts.is_empty() {
            let miss_embeddings = self.inner.embed_batch(&miss_texts).await?;

            // Validate the provider returned exactly one embedding per input.
            if miss_embeddings.len() != miss_texts.len() {
                return Err(EmbeddingError::InvalidResponse(format!(
                    "provider returned {} embeddings for {} inputs",
                    miss_embeddings.len(),
                    miss_texts.len()
                )));
            }

            // Step 4: Insert into cache and place into results.
            for (j, embedding) in miss_embeddings.into_iter().enumerate() {
                let original_index = miss_indices[j];
                let key = Self::hash_text(texts[original_index]);
                self.cache.insert(key, embedding.clone()).await;
                results[original_index] = Some(embedding);
            }
        }

        // Step 5: Unwrap all results (every slot is now Some after
        // cache hits + validated miss embeddings filled all positions).
        Ok(results.into_iter().map(|r| r.unwrap()).collect())
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
}
