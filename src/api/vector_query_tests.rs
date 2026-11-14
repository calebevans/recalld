//! Real HTTP/pipeline regressions with isolated storage and fixed local vectors.
use super::adapters::*;
use crate::model::{AccessKind, MemoryId, NamespaceId};
use crate::search::adapters::*;
use crate::search::{AccessRecorder, QueryEngine, SearchQuery, VectorIndex as _, VectorMetadata};
use crate::test_support::{DIM, Fixture, MemorySpec};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Default)]
struct FixedEmbedding {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl crate::embedding::EmbeddingProvider for FixedEmbedding {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![0.0, 1.0, 0.0, 0.0])
    }
    fn dimensions(&self) -> usize {
        DIM
    }
    fn model_name(&self) -> &str {
        "test-fixed"
    }
}
struct ObservedAccess {
    inner: StorageAccessRecorder,
    completed: AtomicUsize,
    notify: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AccessRecorder for ObservedAccess {
    async fn record_access(
        &self,
        id: MemoryId,
        kind: AccessKind,
    ) -> Result<(), crate::search::SearchError> {
        self.inner.record_access(id, kind).await?;
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
    async fn record_access_batch(
        &self,
        values: &[(MemoryId, AccessKind)],
    ) -> Result<(), crate::search::SearchError> {
        self.inner.record_access_batch(values).await?;
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}
struct Harness {
    f: Fixture,
    engine: Arc<QueryEngine>,
    access: Arc<ObservedAccess>,
    ids: Vec<MemoryId>,
    embedding: Arc<FixedEmbedding>,
}
impl Harness {
    async fn new() -> Self {
        let f = Fixture::new();
        let access = Arc::new(ObservedAccess {
            inner: StorageAccessRecorder::new(f.storage.clone()),
            completed: AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        });
        let embedding = Arc::new(FixedEmbedding::default());
        let engine = Arc::new(QueryEngine::new(
            Arc::new(StorageNamespaceResolver::new(f.storage.clone())),
            Arc::new(SingleEmbeddingRegistry::new(embedding.clone())),
            Arc::new(SharedVectorIndexRegistry::new(f.vector_index.clone())),
            Arc::new(SharedFtsIndexRegistry::new(f.fts_index.clone())),
            Arc::new(SharedEntityIndexReader::new(f.entity_index.clone())),
            Arc::new(CacheManagerAdapter::new(f.cache.clone())),
            Arc::new(StorageMetadataAdapter::new(f.storage.clone())),
            Arc::new(SharedGraphReader::new(f.graph.clone())),
            Arc::new(RifProcessorAdapter::new(
                Arc::new(crate::rif::RifEngine::new(Default::default())),
                f.graph.clone(),
            )),
            access.clone(),
        ));
        let mut ids = Vec::new();
        for (summary, embedding) in [
            ("rarekeyword", [1.0, 0.0, 0.0, 0.0]),
            ("nearby", [0.8, 0.6, 0.0, 0.0]),
            ("other", [0.0, 1.0, 0.0, 0.0]),
        ] {
            let id = f
                .insert_memory_with(MemorySpec {
                    embedding: Some(&embedding),
                    summary,
                    tags: &["entity/fixture"],
                    ..MemorySpec::new(NamespaceId::new(1))
                })
                .await;
            f.vector_index
                .write()
                .await
                .add(
                    id,
                    &embedding,
                    VectorMetadata {
                        namespace_id: NamespaceId::new(1),
                        decay_phase: 1,
                        tags: vec![],
                    },
                )
                .unwrap();
            f.fts_index
                .lock()
                .await
                .add(NamespaceId::new(1), id, summary, None, &[])
                .unwrap();
            f.entity_index.write().await.add(id, &["fixture".into()]);
            ids.push(id);
        }
        Self {
            f,
            engine,
            access,
            ids,
            embedding,
        }
    }
    fn http(&self) -> axum::Router {
        let f = &self.f;
        let config = Arc::new(crate::config::RecalldConfig::default());
        crate::api::router(
            crate::api::AppState::new(
                Arc::new(SearchPipelineAdapter::new(
                    self.engine.clone(),
                    self.embedding.clone(),
                    f.vector_index.clone(),
                    f.fts_index.clone(),
                    f.entity_index.clone(),
                )),
                Arc::new(StorageEngineAdapter::new(
                    f.storage.clone(),
                    f.cache.clone(),
                    f.vector_index.clone(),
                    f.fts_index.clone(),
                    f.entity_index.clone(),
                    f.graph.clone(),
                )),
                Arc::new(RecordCacheAdapter::new(f.cache.clone())),
                Arc::new(RelationshipGraphAdapter::new(
                    f.graph.clone(),
                    f.vector_index.clone(),
                    f.entity_index.clone(),
                    f.storage.clone(),
                    f.cache.clone(),
                    config.clone(),
                )),
                Arc::new(FsrsEngineAdapter::new(
                    f.storage.clone(),
                    f.cache.clone(),
                    f.graph.clone(),
                    config,
                    false,
                )),
                Arc::new(NamespaceRegistryAdapter::new(f.storage.clone())),
                Arc::new(NoopMetricsCollector),
            ),
            &crate::api::ApiConfig::default(),
        )
    }
    fn snapshot(&self) -> String {
        format!(
            "{:?}",
            self.ids
                .iter()
                .map(|id| self.f.record(*id))
                .collect::<Vec<_>>()
        )
    }
    async fn assert_quiet(&self, before: &str) {
        // Bounded detection of accidental queued access; not a whole-store barrier.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), self.access.notify.notified())
                .await
                .is_err()
        );
        assert_eq!(self.access.completed.load(Ordering::SeqCst), 0);
        assert_eq!(self.snapshot(), before);
    }
    async fn observed(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.access.notify.notified())
            .await
            .expect("legacy access did not finish");
        assert!(
            self.ids
                .iter()
                .any(|id| !self.f.record(*id).unwrap().access_history.is_empty())
        );
    }
}

fn query(value: Value) -> SearchQuery {
    serde_json::from_value(value).unwrap()
}
async fn request(h: &Harness, uri: &str, body: String) -> (axum::http::StatusCode, Value) {
    use tower::ServiceExt;
    let response = h
        .http()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vector_query_http_search_and_similarity_succeed_without_embedding_provider() {
    let h = Harness::new().await;
    for scale in [12.0, f32::MAX, f32::from_bits(1)] {
        let (status, body) = request(
            &h,
            "/search",
            json!({"embedding":[scale,0,0,0], "limit":1}).to_string(),
        )
        .await;
        assert!(status.is_success(), "{body}");
        assert_eq!(body["hits"][0]["memory"]["id"], h.ids[0].to_string());
        assert!((body["hits"][0]["score"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        h.observed().await;
    }
    let (status, body) = request(
        &h,
        &format!("/similar/{}", h.ids[0]),
        json!({"limit":1}).to_string(),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["hits"].as_array().unwrap().len(), 1);
    assert_eq!(body["hits"][0]["memory"]["id"], h.ids[1].to_string());
    h.observed().await;
    assert_eq!(h.embedding.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vector_query_precedence_preserves_fts_and_text_only_defaults() {
    let h = Harness::new().await;
    // Both text and vector: explicit vector wins over the opposite provider
    // vector, and the text still matches the FTS index.
    let response = h
        .engine
        .search(query(
            json!({"text":"rarekeyword", "queryEmbedding":[7,0,0,0]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.results[0].memory_id, h.ids[0]);
    assert!(response.results[0].fts_score.is_some());
    h.observed().await;
    // Explicit keyword query retains its existing precedence over text.
    let response = h
        .engine
        .search(query(
            json!({"text":"notindexed", "ftsQuery":"rarekeyword", "queryEmbedding":[1,0,0,0]}),
        ))
        .await
        .unwrap();
    assert!(
        response
            .results
            .iter()
            .find(|r| r.memory_id == h.ids[0])
            .unwrap()
            .fts_score
            .is_some()
    );
    h.observed().await;
    let (status, body) = request(
        &h,
        "/search",
        json!({"query":"rarekeyword", "embedding":[1,0,0,0], "limit":1}).to_string(),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["hits"][0]["memory"]["id"], h.ids[0].to_string());
    h.observed().await;
    assert_eq!(h.embedding.calls.load(Ordering::SeqCst), 0);
    let (status, body) = request(
        &h,
        "/search",
        json!({"query":"notindexed", "limit":1}).to_string(),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["hits"][0]["memory"]["id"], h.ids[2].to_string());
    h.observed().await;
    assert_eq!(h.embedding.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vector_query_invalid_vectors_reject_before_access_work() {
    let h = Harness::new().await;
    let before = h.snapshot();
    for vector in [
        vec![],
        vec![1.0; 3],
        vec![1.0; 5],
        vec![0.0; 4],
        vec![-0.0; 4],
        vec![f32::NAN, 0.0, 0.0, 0.0],
        vec![f32::INFINITY, 0.0, 0.0, 0.0],
        vec![f32::NEG_INFINITY, 0.0, 0.0, 0.0],
    ] {
        let mut q = query(json!({"text":"rarekeyword"}));
        q.query_embedding = Some(vector);
        assert!(matches!(
            h.engine.search(q).await,
            Err(crate::search::SearchError::InvalidEmbedding(_))
        ));
    }
    for vector in [
        json!([]),
        json!([1, 0, 0]),
        json!([1, 0, 0, 0, 0]),
        json!([0, 0, 0, 0]),
    ] {
        let (status, body) = request(&h, "/search", json!({"embedding":vector}).to_string()).await;
        assert_eq!(
            status,
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        assert_eq!(body["field"], "embedding");
    }
    // JSON cannot encode NaN/Infinity. Overflowing JSON numbers must still
    // fail, whether rejected by deserialization or by the vector validator.
    let (status, _) = request(&h, "/search", r#"{"embedding":[1e39,0,0,0]}"#.into()).await;
    assert!(status.is_client_error());
    h.f.with_namespace("five", 5);
    let (status, _) = request(
        &h,
        "/search",
        json!({"namespace":"five", "embedding":[1,0,0,0]}).to_string(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    // Similarity uses an indexed vector; reject corrupt source vectors too.
    h.f.vector_index
        .write()
        .await
        .add(
            h.ids[0],
            &[0.0; DIM],
            VectorMetadata {
                namespace_id: NamespaceId::new(1),
                decay_phase: 1,
                tags: vec![],
            },
        )
        .unwrap();
    let (status, body) =
        request(&h, &format!("/similar/{}", h.ids[0]), json!({}).to_string()).await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "{body}"
    );
    assert_eq!(body["field"], "embedding");
    h.assert_quiet(&before).await;
    assert_eq!(h.embedding.calls.load(Ordering::SeqCst), 0);
}
