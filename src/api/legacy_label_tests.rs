use super::{adapters::*, handlers, models::*, state::AppState};
use crate::embedding::{EmbeddingError, EmbeddingProvider};
use crate::mcp::bridge::StorageEngine as _;
use crate::search::adapters::*;
use crate::storage::StorageEngine as _;
use crate::test_support::{DIM, Fixture};
use axum::{Json, extract::State, response::IntoResponse};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct CountingProvider(AtomicUsize);
#[async_trait::async_trait]
impl EmbeddingProvider for CountingProvider {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1., 0., 0., 0.])
    }
    fn dimensions(&self) -> usize {
        DIM
    }
    fn model_name(&self) -> &str {
        "synthetic-label-validation"
    }
}
fn state(f: &Fixture, provider: Arc<CountingProvider>) -> AppState {
    let config = Arc::new(crate::config::RecalldConfig::default());
    let query = Arc::new(crate::search::QueryEngine::new(
        Arc::new(StorageNamespaceResolver::new(f.storage.clone())),
        Arc::new(SingleEmbeddingRegistry::new(provider.clone())),
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
        Arc::new(StorageAccessRecorder::new(f.storage.clone(), f.cache.clone())),
    ));
    AppState::new(
        Arc::new(SearchPipelineAdapter::new(
            query,
            provider,
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
    )
}
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for item in std::fs::read_dir(dir).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_string_lossy().into(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}
#[tokio::test]
async fn legacy_labels_reject_before_provider_or_mutation_in_actual_http_batch_and_mcp() {
    let f = Fixture::new();
    let provider = Arc::new(CountingProvider::default());
    let state = state(&f, provider.clone());
    let adapter = crate::mcp::bridge_adapters::McpStorageAdapter::new(
        f.storage.clone(),
        f.cache.clone(),
        provider.clone(),
        f.vector_index.clone(),
        f.fts_index.clone(),
        f.entity_index.clone(),
        f.graph.clone(),
        Arc::new(Default::default()),
        chrono_tz::UTC,
    );
    let before = files(f.path());
    let generation = f.storage.read().unwrap().control_state().unwrap();
    for field in ["tags", "entities", "topics", "emotions"] {
        for label in ["John Smith", "José", "Alice's", "", " "] {
            let mut request =
                serde_json::json!({"summary":"retained input", "namespace":"default"});
            request[field] = serde_json::json!([label]);
            let error = handlers::create_memory(
                State(state.clone()),
                Json(serde_json::from_value(request.clone()).unwrap()),
            )
            .await
            .unwrap_err();
            let response = error.into_response();
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["field"], field);
            let (_, Json(batch)) = handlers::batch_store(
                State(state.clone()),
                Json(BatchStoreRequest {
                    memories: vec![serde_json::from_value(request.clone()).unwrap()],
                }),
            )
            .await
            .unwrap();
            assert_eq!(batch.data.total, 0);
            assert_eq!(batch.data.failed.len(), 1);
            assert_eq!(batch.data.failed[0].field.as_deref(), Some(field));
            assert_eq!(batch.data.failed[0].index, 0);
            let mut input = crate::mcp::bridge::StoreInput {
                summary: "retained input".into(),
                full_text: None,
                tags: vec![],
                entities: vec![],
                topics: vec![],
                emotions: vec![],
                namespace: "default".into(),
                embedding: None,
                initial_stability: None,
                parent_id: None,
                supersedes: None,
                check_duplicates: None,
            };
            match field {
                "tags" => input.tags.push(label.into()),
                "entities" => input.entities.push(label.into()),
                "topics" => input.topics.push(label.into()),
                _ => input.emotions.push(label.into()),
            }
            let error = adapter.store_memory(input).await.unwrap_err();
            assert!(matches!(
                error,
                crate::mcp::bridge::BridgeError::InvalidInput(_)
            ));
            assert!(error.to_string().contains(&format!("{field}[0]")));
            assert!(crate::mcp::args::parse_store_input(&request, "default").is_err());
            assert_eq!(provider.0.load(Ordering::SeqCst), 0);
            assert_eq!(
                f.storage.read().unwrap().control_state().unwrap(),
                generation
            );
            assert_eq!(files(f.path()), before, "{field}: {label}");
        }
    }
}

#[tokio::test]
async fn legacy_labels_normalized_positive_survive_receipted_rebuild_and_restart() {
    let f = Fixture::new();
    let adapter = f.storage_adapter();
    for _ in 0..2 {
        let mut input = crate::mcp::args::parse_store_input(&serde_json::json!({
            "summary":"persisted labels", "tags":["USER/Chosen"], "entities":["Alice-1"], "topics":["RUST/lang"], "emotions":["Curious"], "embedding":[1.,0.,0.,0.]
        }), "default").unwrap();
        input.embedding = Some(vec![1., 0., 0., 0.]);
        adapter.store_memory(input).await.unwrap();
    }
    drop(adapter);
    let dir = f.close();
    let engine = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
    let records = engine.scan_all().unwrap();
    assert_eq!(records.len(), 2);
    for (_, r) in records {
        let tags: Vec<_> = r.tags.iter().map(ToString::to_string).collect();
        assert!(tags.contains(&"user/chosen".into()));
        assert!(tags.contains(&"entity/alice-1".into()));
        assert!(tags.contains(&"topic/rust/lang".into()));
        assert!(tags.contains(&"emotion/curious".into()));
        assert!(r.schema.is_none());
    }
}
