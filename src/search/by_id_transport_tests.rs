//! Real transport dispatch and search adapters; read-only canonical hydration
//! uses OfflineSnapshot without opening an ordinary schema-v2 serving engine.
use super::*;
use crate::api::NamespaceStats;
use crate::storage::StorageError;
use serde_json::Value;
use std::path::PathBuf;

struct OfflineHttpStorage(Arc<OfflineSnapshot>);
#[async_trait::async_trait]
#[allow(unused_variables)]
impl crate::api::StorageEngine for OfflineHttpStorage {
    async fn insert_memory(
        &self,
        _: NamespaceId,
        _: crate::storage::insertion::InsertMemory,
        _: crate::embedding::runtime::GeneratedEmbedding,
    ) -> std::result::Result<crate::storage::insertion::InsertResult, StorageError> {
        panic!("unexpected offline HTTP insertion")
    }
    async fn complete_insertion(
        &self,
        _: &crate::storage::control::WriteReceipt,
    ) -> std::result::Result<(), StorageError> {
        panic!("unexpected offline HTTP insertion completion")
    }
    async fn create_memory(
        &self,
        namespace_id: NamespaceId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
        embedding: &[f32],
        initial_stability: Option<f32>,
        created_at: Option<i64>,
    ) -> std::result::Result<CachedRecord, StorageError> {
        panic!("unexpected offline HTTP storage call: create_memory")
    }
    async fn delete_memory(
        &self,
        id: MemoryId,
        namespace: Option<&str>,
    ) -> std::result::Result<bool, StorageError> {
        panic!("unexpected offline HTTP storage call: delete_memory")
    }
    async fn assert_memory_namespace(
        &self,
        _: MemoryId,
        _: &str,
    ) -> std::result::Result<(), StorageError> {
        panic!("unexpected offline HTTP namespace assertion")
    }
    async fn get_record(&self, id: MemoryId) -> Option<DiskRecord> {
        self.0.record(id).unwrap().map(|item| item.record)
    }
    async fn namespace_stats(
        &self,
        namespace_id: NamespaceId,
    ) -> std::result::Result<NamespaceStats, StorageError> {
        panic!("unexpected offline HTTP storage call: namespace_stats")
    }
    async fn list_memories(
        &self,
        filter: &crate::api::models::ListFilter,
    ) -> std::result::Result<Vec<CachedRecord>, StorageError> {
        panic!("unexpected offline HTTP storage call: list_memories")
    }
    async fn list_memories_filtered(
        &self,
        namespace_id: NamespaceId,
        require_tags: &[crate::model::tag::Tag],
        time_range_start: Option<i64>,
        time_range_end: Option<i64>,
        offset: usize,
        limit: usize,
    ) -> std::result::Result<(Vec<(MemoryId, DiskRecord)>, u64), StorageError> {
        panic!("unexpected offline HTTP storage call: list_memories_filtered")
    }
    async fn ping(&self) -> bool {
        panic!("unexpected offline HTTP storage call: ping")
    }
    async fn get_full_text(
        &self,
        id: MemoryId,
    ) -> std::result::Result<Option<String>, StorageError> {
        Ok(self.0.record(id)?.and_then(|item| item.full_text))
    }
    async fn scan_all(&self) -> std::result::Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        panic!("unexpected offline HTTP storage call: scan_all")
    }
    async fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> std::result::Result<Vec<(MemoryId, DiskRecord)>, StorageError> {
        panic!("unexpected offline HTTP storage call: scan_phase_records")
    }
    async fn list_tags(&self) -> std::result::Result<Vec<(String, u64)>, StorageError> {
        panic!("unexpected offline HTTP storage call: list_tags")
    }
    fn storage_path(&self) -> std::result::Result<PathBuf, StorageError> {
        panic!("unexpected offline HTTP storage call: storage_path")
    }
}

struct FixtureEmbedding;
#[async_trait::async_trait]
impl crate::embedding::EmbeddingProvider for FixtureEmbedding {
    async fn embed(
        &self,
        _: &str,
    ) -> std::result::Result<Vec<f32>, crate::embedding::EmbeddingError> {
        panic!("by-ID transport must use the stored embedding")
    }
    fn dimensions(&self) -> usize {
        4
    }
    fn model_name(&self) -> &str {
        "fixture-only-no-profile-attestation"
    }
}
struct CountSelection {
    inner: Arc<dyn VectorIndexRegistry>,
    calls: AtomicUsize,
}
impl VectorIndexRegistry for CountSelection {
    fn search(
        &self,
        ns: NamespaceId,
        vector: &[f32],
        k: usize,
        eligible: Option<&HashSet<MemoryId>>,
    ) -> Result<Vec<ScoredResult>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.search(ns, vector, k, eligible)
    }
    fn get_vector(&self, id: MemoryId) -> Option<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get_vector(id)
    }
}
fn router(f: &Fixture, snapshot: Arc<OfflineSnapshot>, engine: Arc<QueryEngine>) -> axum::Router {
    use crate::api::adapters::*;
    let config = Arc::new(crate::config::RecalldConfig::default());
    crate::api::router(
        crate::api::AppState::new(
            Arc::new(SearchPipelineAdapter::new(
                engine,
                Arc::new(FixtureEmbedding),
                f.vector_index.clone(),
                f.fts_index.clone(),
                f.entity_index.clone(),
            )),
            Arc::new(OfflineHttpStorage(snapshot)),
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
async fn http(router: axum::Router, id: MemoryId, access: bool) -> (axum::http::StatusCode, Value) {
    use tower::ServiceExt;
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/similar/{id}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"recordAccess":access,"limit":10}).to_string(),
                ))
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
async fn by_id_actual_rest_and_mcp_preserve_requested_access_intent_and_offline_bytes() {
    let f = offline(2, LifecycleVersion::ExplicitFeedbackV2, |_, r| {
        r.difficulty = 7.25;
        r.strength = 0.23;
        r.decay_strength = 0.67;
    })
    .await;
    let bytes = file_bytes(&f.files);
    let clock = CountingClock::new(DAY);
    let mut engine = engine(&f, clock.clone(), &[(0, 1, EdgeType::Associative)]);
    let namespaces = Arc::new(ByIdNamespace {
        view: f.view.clone(),
        calls: AtomicUsize::new(0),
        wrong_scope: false,
    });
    engine.namespace_resolver = namespaces.clone();
    let selection = Arc::new(CountSelection {
        inner: engine.vector_indexes.clone(),
        calls: AtomicUsize::new(0),
    });
    engine.vector_indexes = selection.clone();
    let engine = Arc::new(engine);
    // Placeholder subsystems are isolated legacy fixtures; neither transport
    // opens the offline-v2 store through RedbStorageEngine or runs a daemon.
    let placeholder = Fixture::new();
    let api = router(&placeholder, f.view.clone(), engine.clone());
    let stub = Arc::new(crate::test_support::StubBridge::default());
    let bridge = crate::mcp::bridge::McpBridge {
        search: Arc::new(crate::mcp::bridge_adapters::McpSearchAdapter::new(
            engine,
            Arc::new(FixtureEmbedding),
            placeholder.storage.clone(),
            placeholder.graph.clone(),
            chrono_tz::UTC,
        )),
        storage: stub.clone(),
        namespaces: stub.clone(),
        health: stub,
        default_namespace: "foreign".into(),
        timezone: chrono_tz::UTC,
    };
    let (status, body) = http(api.clone(), f.ids[0], true).await;
    assert!(!status.is_success(), "{body}");
    let mcp = crate::mcp::tools::dispatch_tool(
        &bridge,
        "find_similar_memories",
        json!({"id":f.ids[0].to_string(),"recordAccess":true}),
    )
    .await;
    assert_eq!(mcp.is_error, Some(true));
    assert_eq!(
        selection.calls.load(Ordering::SeqCst),
        0,
        "true intent must fail before vector selection"
    );
    assert_eq!(
        clock.calls.load(Ordering::SeqCst),
        0,
        "rejected true operations do not project"
    );
    assert_eq!(namespaces.calls.load(Ordering::SeqCst), 2);

    let (status, first) = http(api.clone(), f.ids[0], false).await;
    assert!(status.is_success(), "{first}");
    assert_eq!(first["hits"].as_array().unwrap().len(), 1);
    let memory = &first["hits"][0]["memory"];
    assert_eq!(memory["id"], json!(f.ids[1].to_string()));
    assert_eq!(memory["namespace"], "default");
    assert_eq!(memory["difficulty"], 7.25);
    assert!(memory["strength"].as_f64().unwrap() > 0.8);
    assert!(memory["decayStrength"].as_f64().unwrap() > memory["strength"].as_f64().unwrap());
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(namespaces.calls.load(Ordering::SeqCst), 3);
    let mcp = crate::mcp::tools::dispatch_tool(
        &bridge,
        "find_similar_memories",
        json!({"id":f.ids[0].to_string(),"recordAccess":false}),
    )
    .await;
    assert_ne!(mcp.is_error, Some(true));
    let serialized = serde_json::to_value(mcp).unwrap();
    let value: Value =
        serde_json::from_str(serialized["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["memories"][0]["namespace"], "default");
    assert!(value["memories"][0]["strength"].as_f64().unwrap() > 0.8);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 2);
    assert_eq!(namespaces.calls.load(Ordering::SeqCst), 4);
    clock.now.store(30 * DAY, Ordering::SeqCst);
    let (status, later) = http(api, f.ids[0], false).await;
    assert!(status.is_success(), "{later}");
    assert_eq!(first["hits"][0]["score"], later["hits"][0]["score"]);
    assert!(
        later["hits"][0]["memory"]["strength"].as_f64().unwrap()
            < memory["strength"].as_f64().unwrap()
    );
    assert_eq!(clock.calls.load(Ordering::SeqCst), 3);
    assert_eq!(namespaces.calls.load(Ordering::SeqCst), 5);
    tokio::task::yield_now().await;
    assert_eq!(file_bytes(&f.files), bytes);
    assert!(matches!(
        f.view.namespaces()[0]
            .schema
            .as_ref()
            .unwrap()
            .embedding_binding,
        crate::model::schema::EmbeddingBinding::UnknownLegacy
    ));
}
