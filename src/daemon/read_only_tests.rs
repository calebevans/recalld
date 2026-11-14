//! Isolated adapters/transport tests. No daemon lifecycle or external provider.
use super::*;
use crate::api::adapters::*;
use crate::mcp::bridge::{self, McpBridge};
use crate::model::{AccessKind, CachedRecord, NamespaceId};
use crate::search::adapters::*;
use crate::search::{AccessRecorder, QueryEngine, SearchQuery, VectorIndex as _, VectorMetadata};
use crate::test_support::{DIM, Fixture, MemorySpec, StubBridge};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

const NOW: i64 = 86_400_000;
const VECTOR: [f32; DIM] = [1.0, 0.0, 0.0, 0.0];
struct FixedEmbedding;
#[async_trait::async_trait]
impl crate::embedding::EmbeddingProvider for FixedEmbedding {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
        Ok(VECTOR.to_vec())
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
}
impl Harness {
    async fn new() -> Self {
        let f = Fixture::new();
        let access = Arc::new(ObservedAccess {
            inner: StorageAccessRecorder::new(f.storage.clone())
                .with_clock(Arc::new(crate::time::ManualClock::new(NOW))),
            completed: AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        });
        let engine = Arc::new(QueryEngine::new(
            Arc::new(StorageNamespaceResolver::new(f.storage.clone())),
            Arc::new(SingleEmbeddingRegistry::new(Arc::new(FixedEmbedding))),
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
        for _ in 0..3 {
            let id = f
                .insert_memory_with(MemorySpec {
                    embedding: Some(&VECTOR),
                    tags: &["entity/fixture"],
                    ..MemorySpec::new(NamespaceId::new(1))
                })
                .await;
            f.vector_index
                .write()
                .await
                .add(
                    id,
                    &VECTOR,
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
                .add(NamespaceId::new(1), id, "fixture", None, &[])
                .unwrap();
            f.entity_index.write().await.add(id, &["fixture".into()]);
            ids.push(id);
        }
        f.graph
            .write()
            .await
            .add_edge(
                ids[0],
                ids[1],
                crate::model::EdgeType::Associative,
                0.8,
                false,
            )
            .unwrap();
        Self {
            f,
            engine,
            access,
            ids,
        }
    }
    fn bridge(&self) -> McpBridge {
        McpBridge {
            search: Arc::new(crate::mcp::bridge_adapters::McpSearchAdapter::new(
                self.engine.clone(),
                Arc::new(FixedEmbedding),
                self.f.storage.clone(),
                self.f.graph.clone(),
                chrono_tz::UTC,
            )),
            storage: Arc::new(self.f.storage_adapter()),
            namespaces: Arc::new(self.f.namespace_adapter()),
            health: Arc::new(StubBridge::default()),
            default_namespace: "default".into(),
            timezone: chrono_tz::UTC,
        }
    }
    fn http(&self) -> axum::Router {
        let f = &self.f;
        let config = Arc::new(crate::config::RecalldConfig::default());
        crate::api::router(
            crate::api::AppState::new(
                Arc::new(SearchPipelineAdapter::new(
                    self.engine.clone(),
                    Arc::new(FixedEmbedding),
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
                Arc::new(
                    FsrsEngineAdapter::new(
                        f.storage.clone(),
                        f.cache.clone(),
                        f.graph.clone(),
                        config,
                        false,
                    )
                    .with_clock(Arc::new(crate::time::ManualClock::new(NOW))),
                ),
                Arc::new(NamespaceRegistryAdapter::new(f.storage.clone())),
                Arc::new(NoopMetricsCollector),
            ),
            &crate::api::ApiConfig::default(),
        )
    }
    async fn snapshot(&self) -> String {
        let records: Vec<_> = self.ids.iter().map(|id| self.f.record(*id)).collect();
        let nodes: Vec<_> = {
            let graph = self.f.graph.read().await;
            self.ids
                .iter()
                .map(|id| format!("{:?}", graph.get_node(id)))
                .collect()
        };
        let vectors: Vec<_> = {
            let index = self.f.vector_index.read().await;
            self.ids
                .iter()
                .map(|id| index.get_vector(*id).map(|v| v.to_vec()))
                .collect()
        };
        let fts = self
            .f
            .fts_index
            .lock()
            .await
            .search(NamespaceId::new(1), "fixture", 10)
            .unwrap();
        format!(
            "{records:?}{nodes:?}{vectors:?}{fts:?}{:?}{}",
            self.f.persisted_edges(),
            self.f.entity_index.read().await.len()
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
        assert_eq!(self.snapshot().await, before);
    }
    async fn observed(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.access.notify.notified())
            .await
            .expect("legacy access did not finish");
        assert!(
            self.ids
                .iter()
                .any(|id| self.f.record(*id).unwrap().last_accessed_at == NOW)
        );
    }
}
fn query(record_access: bool) -> SearchQuery {
    serde_json::from_value(json!({"text":"fixture", "recordAccess":record_access, "graphDepth":1}))
        .unwrap()
}

fn input(record_access: Option<bool>) -> bridge::SearchInput {
    let mut value = json!({"query":"fixture", "namespace":"default", "limit":10, "minScore":null,
        "includeGhosts":false, "tags":[], "entities":[], "topics":[], "emotions":[], "minStrength":null,
        "depth":1, "timeRangeStart":null, "timeRangeEnd":null});
    if let Some(value_bool) = record_access {
        value["recordAccess"] = json!(value_bool);
    }
    serde_json::from_value(value).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_pipeline_bypasses_stale_cache_and_preserves_state() {
    let h = Harness::new().await;
    let id = h.ids[0];
    let mut cached = CachedRecord::from(&h.f.record(id).unwrap());
    cached.summary = "stale cache".into();
    h.f.cache.insert(id, cached).await;
    let before = h.snapshot().await;
    let response = h.engine.search(query(false)).await.unwrap();
    assert_eq!(response.results.len(), 3);
    assert!(
        response
            .results
            .iter()
            .all(|r| r.summary.as_deref() == Some("fixture"))
    );
    assert_eq!(
        h.engine
            .get_memory_with_access(id, false)
            .await
            .unwrap()
            .unwrap()
            .summary
            .as_deref(),
        Some("fixture")
    );
    assert_eq!(
        h.engine
            .similar_with_access(id, 10, false)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(h.f.cache.get(id).await.unwrap().summary, "stale cache");
    assert!(
        h.f.cache.get(h.ids[1]).await.is_none(),
        "false mode must not fill record cache"
    );
    h.assert_quiet(&before).await;
    // Explicit true and old wrappers retain async exposure without FSRS feedback.
    for mode in 0..3 {
        match mode {
            0 => {
                h.engine.search(query(true)).await.unwrap();
            }
            1 => {
                h.engine.get_memory(id).await.unwrap();
            }
            _ => {
                h.engine.similar(id, 10).await.unwrap();
            }
        }
        h.observed().await;
    }
    assert_eq!(h.f.record(id).unwrap().stability, 1.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_mcp_tools_and_legacy_get_contract() {
    let h = Harness::new().await;
    let bridge = h.bridge();
    let before = h.snapshot().await;
    for (name, args) in [
        (
            "recall_memories",
            json!({"query":"fixture", "recordAccess":false}),
        ),
        (
            "get_memory",
            json!({"id":h.ids[0].to_string(), "recordAccess":false}),
        ),
        (
            "find_similar_memories",
            json!({"id":h.ids[0].to_string(), "recordAccess":false}),
        ),
        ("get_memory", json!({"id":h.ids[0].to_string()})),
        (
            "get_memory",
            json!({"id":h.ids[0].to_string(), "recordAccess":true}),
        ),
    ] {
        let response = crate::mcp::tools::dispatch_tool(&bridge, name, args).await;
        assert_ne!(response.is_error, Some(true), "{response:?}");
    }
    for name in ["recall_memories", "get_memory", "find_similar_memories"] {
        let response = crate::mcp::tools::dispatch_tool(
            &bridge,
            name,
            json!({"id":h.ids[0].to_string(), "query":"fixture", "recordAccess":"false"}),
        )
        .await;
        assert_eq!(response.is_error, Some(true));
    }
    h.assert_quiet(&before).await;
    for name in ["recall_memories", "find_similar_memories"] {
        for flag in [None, Some(true)] {
            let mut args = json!({"id":h.ids[0].to_string(), "query":"fixture"});
            if let Some(flag) = flag {
                args["recordAccess"] = json!(flag);
            }
            assert_ne!(
                crate::mcp::tools::dispatch_tool(&bridge, name, args)
                    .await
                    .is_error,
                Some(true)
            );
            h.observed().await;
        }
    }
}

async fn http_call(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Value,
) -> (axum::http::StatusCode, Value) {
    use tower::ServiceExt;
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_http_get_search_and_existing_similarity_error() {
    let h = Harness::new().await;
    let before = h.snapshot().await;
    let get = format!("/memories/{}", h.ids[0]);
    let (status, _) = http_call(
        h.http(),
        "GET",
        &format!("{get}?recordAccess=false"),
        Value::Null,
    )
    .await;
    assert!(status.is_success());
    assert!(h.f.cache.get(h.ids[0]).await.is_none());
    let (status, body) = http_call(
        h.http(),
        "POST",
        "/search",
        json!({"query":"fixture", "recordAccess":false}),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["hits"].as_array().unwrap().len(), 3);
    // Existing HTTP vector-only adapter rejects EmptyQuery. Verify false mode
    // also bypasses the source cache before this failure; do not claim success.
    let (status, _) = http_call(
        h.http(),
        "POST",
        &format!("/similar/{}", h.ids[0]),
        json!({"recordAccess":false}),
    )
    .await;
    assert!(status.is_client_error());
    assert!(h.f.cache.get(h.ids[0]).await.is_none());
    h.assert_quiet(&before).await;
    for suffix in ["", "?recordAccess=true"] {
        assert!(
            http_call(h.http(), "GET", &format!("{get}{suffix}"), Value::Null)
                .await
                .0
                .is_success()
        );
        assert_eq!(h.f.record(h.ids[0]).unwrap().last_accessed_at, NOW);
    }
    for flag in [None, Some(true)] {
        let mut body = json!({"query":"fixture"});
        if let Some(flag) = flag {
            body["recordAccess"] = json!(flag);
        }
        assert!(
            http_call(h.http(), "POST", "/search", body)
                .await
                .0
                .is_success()
        );
        h.observed().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_daemon_aliases_force_false_and_legacy_methods_keep_defaults() {
    use crate::daemon::bridge_adapters::{RemoteSearchAdapter, RemoteStorageAdapter};
    use crate::mcp::bridge::{SearchPipeline as _, StorageEngine as _};
    let h = Harness::new().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let b = h.bridge();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let tx = shutdown_tx.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        handle_connection(
            b.search,
            b.storage,
            b.namespaces,
            b.health,
            stream,
            Arc::new(AtomicU32::new(1)),
            Arc::new(AtomicI64::new(0)),
            shutdown_rx,
            tx,
        )
        .await;
    });
    let client = Arc::new(
        crate::daemon::client::DaemonClient::connect(&path)
            .await
            .unwrap(),
    );
    let search = RemoteSearchAdapter::new(client.clone());
    let storage = RemoteStorageAdapter::new(client.clone());
    let before = h.snapshot().await;
    assert_eq!(
        search.search(input(Some(false))).await.unwrap().hits.len(),
        3
    );
    assert!(
        storage
            .get_memory(h.ids[0], false, None)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        search
            .find_similar(h.ids[0], 10, None, true, false, None)
            .await
            .unwrap()
            .len(),
        2
    );
    // Even conflicting true payloads cannot make a private read-only alias observe.
    client
        .call(
            "search_read_only",
            serde_json::to_value(input(Some(true))).unwrap(),
        )
        .await
        .unwrap();
    client
        .call(
            "get_memory_read_only",
            json!({"id":h.ids[0].to_string(), "recordAccess":true}),
        )
        .await
        .unwrap();
    client
        .call(
            "find_similar_read_only",
            json!({"id":h.ids[0].to_string(), "limit":10,
        "minScore":null, "sameNamespace":true, "recordAccess":true}),
        )
        .await
        .unwrap();
    // Legacy daemon get was always nonobserving, with or without the new flag.
    storage.get_memory(h.ids[0], true, None).await.unwrap();
    client
        .call("get_memory", json!({"id":h.ids[0].to_string()}))
        .await
        .unwrap();
    h.assert_quiet(&before).await;
    search.search(input(Some(true))).await.unwrap();
    h.observed().await;
    let mut omitted = serde_json::to_value(input(None)).unwrap();
    omitted.as_object_mut().unwrap().remove("recordAccess");
    client.call("search", omitted).await.unwrap();
    h.observed().await;
    search
        .find_similar(h.ids[0], 10, None, true, true, None)
        .await
        .unwrap();
    h.observed().await;
    client
        .call(
            "find_similar",
            json!({"id":h.ids[0].to_string(), "limit":10,
        "minScore":null, "sameNamespace":true}),
        )
        .await
        .unwrap();
    h.observed().await;
    shutdown_tx.send(true).unwrap();
    server.await.unwrap();
}

/// A v1 dispatch simulator over the actual framed socket client. Its method
/// table deliberately predates the aliases; this is not an old daemon binary.
#[tokio::test]
async fn read_only_old_handler_unknown_method_never_falls_back_to_mutating_rpc() {
    use crate::daemon::bridge_adapters::{RemoteSearchAdapter, RemoteStorageAdapter};
    use crate::mcp::bridge::{SearchPipeline as _, StorageEngine as _};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut methods = Vec::new();
        for _ in 0..6 {
            let request = read_framed_message(&mut stream, MAX_MESSAGE_SIZE)
                .await
                .unwrap();
            methods.push(request.method.clone());
            let response = match request.method.as_str() {
                "search" => DaemonResponse::success(request.id, json!({"hits":[], "neighbors":[]})),
                "get_memory" => DaemonResponse::error(
                    request.id,
                    DaemonRpcError::from(&bridge::BridgeError::NotFound("fixture absent".into())),
                ),
                "find_similar" => DaemonResponse::success(request.id, json!([])),
                other => DaemonResponse::error(
                    request.id,
                    DaemonRpcError::from(&bridge::BridgeError::InvalidInput(format!(
                        "unknown method: {other}"
                    ))),
                ),
            };
            write_framed_message(&mut stream, &response, MAX_MESSAGE_SIZE)
                .await
                .unwrap();
        }
        methods
    });
    let client = Arc::new(
        crate::daemon::client::DaemonClient::connect(&path)
            .await
            .unwrap(),
    );
    let search = RemoteSearchAdapter::new(client.clone());
    let storage = RemoteStorageAdapter::new(client);
    let id = MemoryId::new();
    assert!(
        search
            .search(input(Some(false)))
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown method: search_read_only")
    );
    assert!(
        storage
            .get_memory(id, false, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown method: get_memory_read_only")
    );
    assert!(
        search
            .find_similar(id, 10, None, true, false, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown method: find_similar_read_only")
    );
    search.search(input(None)).await.unwrap();
    assert!(
        storage
            .get_memory(id, true, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("fixture absent")
    );
    search
        .find_similar(id, 10, None, true, true, None)
        .await
        .unwrap();
    assert_eq!(
        server.await.unwrap(),
        [
            "search_read_only",
            "get_memory_read_only",
            "find_similar_read_only",
            "search",
            "get_memory",
            "find_similar"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_graph_expansion_and_supersedes_hydrate_canonical_records() {
    let mut h = Harness::new().await;
    let replacement = h.f.insert_memory().await;
    h.ids.push(replacement);
    let mut stale = CachedRecord::from(&h.f.record(replacement).unwrap());
    stale.summary = "stale replacement".into();
    h.f.cache.insert(replacement, stale).await;
    h.f.graph
        .write()
        .await
        .add_edge(
            replacement,
            h.ids[0],
            crate::model::EdgeType::Supersedes,
            1.0,
            false,
        )
        .unwrap();
    let before = h.snapshot().await;
    // With depth zero, the unindexed replacement loads only in resolve_supersedes.
    // With depth one, graph expansion hydrates it before supersedes resolution.
    for depth in [0, 1] {
        let mut q = query(false);
        q.graph_depth = depth;
        let response = h.engine.search(q).await.unwrap();
        let found = response
            .results
            .iter()
            .find(|r| r.memory_id == replacement)
            .expect("replacement hydrated");
        assert_eq!(found.summary.as_deref(), Some("fixture"));
        assert!(!response.results.iter().any(|r| r.memory_id == h.ids[0]));
    }
    h.assert_quiet(&before).await;
    assert_eq!(
        h.f.cache.get(replacement).await.unwrap().summary,
        "stale replacement"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_read_only_controls_survive_http_mcp_and_framed_daemon() {
    use crate::daemon::bridge_adapters::{RemoteSearchAdapter, RemoteStorageAdapter};
    let h = Harness::new().await;
    h.f.create_namespace("other");
    let before = h.snapshot().await;
    let mut local = h.bridge();
    local.default_namespace = "other".into();
    for namespace in ["default", "other"] {
        for tool in ["get_memory", "find_similar_memories"] {
            let result = crate::mcp::tools::dispatch_tool(
                &local,
                tool,
                json!({"id":h.ids[0].to_string(), "namespace":namespace,"recordAccess":false}),
            )
            .await;
            assert_eq!(
                result.is_error == Some(true),
                namespace == "other",
                "{result:?}"
            );
        }
        let (status, _) = http_call(
            h.http(),
            "GET",
            &format!(
                "/memories/{}?namespace={namespace}&recordAccess=false",
                h.ids[0]
            ),
            Value::Null,
        )
        .await;
        assert_eq!(
            status.as_u16(),
            if namespace == "other" { 409 } else { 200 }
        );
        let (status, body) = http_call(
            h.http(),
            "POST",
            &format!("/similar/{}", h.ids[0]),
            json!({"namespace":namespace,"recordAccess":false}),
        )
        .await;
        assert_eq!(
            status.as_u16(),
            if namespace == "other" { 409 } else { 400 },
            "{body}"
        );
    }
    // A true-mode mismatch also fails before queued observation work.
    let (status, _) = http_call(
        h.http(),
        "POST",
        &format!("/similar/{}", h.ids[0]),
        json!({"namespace":"other","recordAccess":true}),
    )
    .await;
    assert_eq!(status.as_u16(), 409);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("combined.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let handlers = h.bridge();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for i in 0..7 {
            let req = read_framed_message(&mut socket, MAX_MESSAGE_SIZE)
                .await
                .unwrap();
            assert_eq!(req.method.ends_with("_scoped_read_only"), i < 6);
            let result = dispatch(
                handlers.search.as_ref(),
                handlers.storage.as_ref(),
                handlers.namespaces.as_ref(),
                handlers.health.as_ref(),
                &req.method,
                req.params,
            )
            .await;
            let response = match result {
                Ok(result) => DaemonResponse::success(req.id, result),
                Err(error) => DaemonResponse::error(req.id, (&error).into()),
            };
            write_framed_message(&mut socket, &response, MAX_MESSAGE_SIZE)
                .await
                .unwrap();
        }
    });
    let client = Arc::new(crate::daemon::DaemonClient::connect(&path).await.unwrap());
    let mut remote = StubBridge::default().into_bridge();
    remote.search = Arc::new(RemoteSearchAdapter::new(client.clone()));
    remote.storage = Arc::new(RemoteStorageAdapter::new(client.clone()));
    remote.default_namespace = "other".into();
    for namespace in ["default", "other"] {
        for tool in ["get_memory", "find_similar_memories"] {
            let result = crate::mcp::tools::dispatch_tool(
                &remote,
                tool,
                json!({"id":h.ids[0].to_string(), "namespace":namespace,"recordAccess":false}),
            )
            .await;
            assert_eq!(
                result.is_error == Some(true),
                namespace == "other",
                "{result:?}"
            );
        }
    }
    // Private combined aliases force false even with a contradictory true payload.
    for method in [
        "get_memory_scoped_read_only",
        "find_similar_scoped_read_only",
    ] {
        client
            .call(
                method,
                json!({"id":h.ids[0].to_string(),"namespace":"default",
            "recordAccess":true,"limit":10,"minScore":null,"sameNamespace":true}),
            )
            .await
            .unwrap();
    }
    h.assert_quiet(&before).await;
    assert!(
        h.f.cache.get(h.ids[1]).await.is_none(),
        "false mode must not warm record caches"
    );
    // Assertion-only mode still records accesses; helper must strip the wire flag
    // to retain compatibility with the original *_scoped protocol.
    let result = crate::mcp::tools::dispatch_tool(
        &remote,
        "find_similar_memories",
        json!({"id":h.ids[0].to_string(),"namespace":"default","recordAccess":true}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    h.observed().await;
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_read_only_aliases_fail_closed_on_older_single_control_daemons() {
    use crate::daemon::bridge_adapters::{RemoteSearchAdapter, RemoteStorageAdapter};
    for older_alias in ["get_memory_read_only", "get_memory_scoped"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("older.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            for i in 0..3 {
                let req = read_framed_message(&mut socket, MAX_MESSAGE_SIZE)
                    .await
                    .unwrap();
                let response = if i < 2 {
                    assert!(
                        req.method.ends_with("_scoped_read_only"),
                        "no fallback to {older_alias}"
                    );
                    DaemonResponse::error(
                        req.id,
                        (&BridgeError::InvalidInput("unknown method".into())).into(),
                    )
                } else {
                    assert_eq!(req.method, "get_memory");
                    DaemonResponse::success(
                        req.id,
                        json!({
                            "id":req.params["id"], "namespace":"default", "summary":"legacy fixture",
                            "fullText":null, "tags":[], "phase":"Full", "strength":1.0,
                            "stability":1.0, "createdAt":"1970-01-01T00:00:00Z", "lastAccessedAt":"1970-01-01T00:00:00Z",
                            "isPermastore":false, "edgeCount":0
                        }),
                    )
                };
                write_framed_message(&mut socket, &response, MAX_MESSAGE_SIZE)
                    .await
                    .unwrap();
            }
        });
        let client = Arc::new(crate::daemon::DaemonClient::connect(&path).await.unwrap());
        let storage = RemoteStorageAdapter::new(client.clone());
        let search = RemoteSearchAdapter::new(client);
        let id = MemoryId::new();
        assert!(
            storage
                .get_memory(id, false, Some("default"))
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown method")
        );
        assert!(
            search
                .find_similar(id, 10, None, true, false, Some("default"))
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown method")
        );
        assert!(storage.get_memory(id, true, None).await.unwrap().is_some());
        server.await.unwrap();
    }
}

// Real adapters and production handlers: threshold is cosine, not strength;
// only final returned IDs may receive legacy associative exposure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn similarity_threshold_namespace_and_returned_only_exposure() {
    for rest in [false, true] {
        let mut h = Harness::new().await;
        let namespace = h.f.create_namespace("similarity-scope");
        let mut members = Vec::new();
        for (vector, strength) in [
            ([1.0, 0.0, 0.0, 0.0], 1.0),
            ([0.8, 0.6, 0.0, 0.0], 0.01),
            ([0.0, 1.0, 0.0, 0.0], 1.0),
            ([0.6, 0.8, 0.0, 0.0], 1.0),
        ] {
            let id =
                h.f.insert_memory_with(MemorySpec {
                    embedding: Some(&vector),
                    decay_strength: strength,
                    ..MemorySpec::new(namespace)
                })
                .await;
            h.f.vector_index
                .write()
                .await
                .add(
                    id,
                    &vector,
                    VectorMetadata {
                        namespace_id: namespace,
                        decay_phase: 1,
                        tags: vec![],
                    },
                )
                .unwrap();
            members.push(id);
            h.ids.push(id);
        }
        let bridge = h.bridge(); // default differs from canonical source scope
        let before = h.snapshot().await;
        // /search must still filter strength: the low-strength closest neighbor
        // is excluded even though its cosine is .8.
        let (status, search) = http_call(
            h.http(),
            "POST",
            "/search",
            json!({
                "embedding": VECTOR, "namespace":"similarity-scope",
                "recordAccess":false, "minStrength":0.5, "limit":20,
            }),
        )
        .await;
        assert!(status.is_success(), "{search}");
        let hits = search["hits"].as_array().unwrap();
        assert!(
            hits.iter()
                .any(|v| v["memory"]["id"] == members[2].to_string())
        );
        assert!(
            !hits
                .iter()
                .any(|v| v["memory"]["id"] == members[1].to_string())
        );
        // Default/explicit scope, no score threshold, threshold, and empty results.
        for (minimum, expected) in [(None, 3), (Some(0.1), 2), (Some(0.9), 0)] {
            for explicit_scope in [false, true] {
                let mut args =
                    json!({"id":members[0].to_string(), "recordAccess":false, "limit":20});
                if let Some(minimum) = minimum {
                    args["minScore"] = json!(minimum);
                }
                if explicit_scope {
                    args["namespace"] = json!("similarity-scope");
                }
                let hits = similarity_call(&h, &bridge, rest, args).await;
                assert_eq!(hits.len(), expected);
                for hit in &hits {
                    let memory = if rest { &hit["memory"] } else { hit };
                    assert_eq!(memory["namespace"], "similarity-scope");
                    assert_ne!(memory["id"], members[0].to_string());
                    if let Some(minimum) = minimum {
                        assert!(hit["score"].as_f64().unwrap() >= minimum);
                    }
                }
            }
        }
        h.assert_quiet(&before).await;
        // No hits in true mode means no event, not exposure of discarded candidates.
        let empty = similarity_call(
            &h,
            &bridge,
            rest,
            json!({
                "id":members[0].to_string(), "minScore":0.9, "recordAccess":true,
            }),
        )
        .await;
        assert!(empty.is_empty());
        h.assert_quiet(&before).await;
        // Both implicit true and explicit true: source, below-threshold, foreign,
        // and above-threshold but beyond-limit candidates remain unchanged.
        for explicit_true in [false, true] {
            let mut args = json!({"id":members[0].to_string(), "minScore":0.1, "limit":1});
            if explicit_true {
                args["recordAccess"] = json!(true);
            }
            let records: Vec<_> = h.ids.iter().map(|id| h.f.record(*id).unwrap()).collect();
            let hits = similarity_call(&h, &bridge, rest, args).await;
            assert_eq!(hits.len(), 1);
            let returned = if rest {
                &hits[0]["memory"]["id"]
            } else {
                &hits[0]["id"]
            };
            assert_eq!(returned, &json!(members[1].to_string()));
            h.observed().await;
            for (id, old) in h.ids.iter().zip(records) {
                let new = h.f.record(*id).unwrap();
                if *id == members[1] {
                    assert_eq!(new.access_history.len(), old.access_history.len() + 1);
                    assert_eq!(new.last_accessed_at, NOW);
                    assert_eq!(new.stability, old.stability);
                    assert_eq!(
                        new.access_history.last().unwrap().kind,
                        AccessKind::AssociativeRetrieval
                    );
                } else {
                    assert_eq!(
                        new.to_bytes(),
                        old.to_bytes(),
                        "excluded candidate {id} changed"
                    );
                }
            }
        }
    }
}

async fn similarity_call(h: &Harness, bridge: &McpBridge, rest: bool, args: Value) -> Vec<Value> {
    if rest {
        let (status, body) = http_call(
            h.http(),
            "POST",
            &format!("/similar/{}", args["id"].as_str().unwrap()),
            args,
        )
        .await;
        assert!(status.is_success(), "{body}");
        body["hits"].as_array().unwrap().clone()
    } else {
        let result = crate::mcp::tools::dispatch_tool(bridge, "find_similar_memories", args).await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let wire = serde_json::to_value(result).unwrap();
        let body: Value =
            serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap();
        body["memories"].as_array().unwrap().clone()
    }
}
