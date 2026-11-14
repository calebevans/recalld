use super::*;
use crate::model::{CachedRecord, MemoryId, NamespaceId};
use crate::search::{adapters::*, *};
use crate::test_support::{Fixture, MemorySpec};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
fn subscriber(capture: Capture) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_env_filter("off,recalld::performance=trace")
        .with_writer(capture)
        .finish()
}
fn events(capture: &Capture) -> Vec<serde_json::Value> {
    String::from_utf8(capture.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["fields"].clone())
        .collect()
}
struct NoCache;
impl RecordCache for NoCache {
    fn get(&self, _: &MemoryId) -> Option<CachedRecord> {
        panic!("read-only cache lookup")
    }
}
struct NoRif;
impl RifProcessor for NoRif {
    fn compute_suppressions(&self, _: &[MemoryId], _: &[MemoryId]) -> Vec<RifSuppression> {
        vec![]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn performance_trace_reconciles_real_reads_and_never_changes_false_mode_state() {
    use crate::storage::StorageEngine;
    let fx = Fixture::new();
    let ns = NamespaceId::new(1);
    let other = fx.create_namespace("private-namespace");
    let mut target_bytes = 0;
    let mut ids = Vec::new();
    for i in 0..4 {
        let namespace = if i == 3 { other } else { ns };
        let id = fx
            .insert_memory_with(MemorySpec {
                namespace,
                summary: "private-payload-needle",
                tags: if i == 2 {
                    &["project/reject"]
                } else {
                    &["project/allowed"]
                },
                embedding: Some(&[1.0, 0.0, 0.0, 0.0]),
                ..MemorySpec::new(ns)
            })
            .await;
        ids.push(id);
        let record = fx.record(id).unwrap();
        if namespace == ns {
            target_bytes += record.encode().unwrap().len();
        }
        fx.vector_index
            .write()
            .await
            .add(
                id,
                &[1.0, 0.0, 0.0, 0.0],
                VectorMetadata {
                    namespace_id: namespace,
                    decay_phase: 1,
                    tags: vec![],
                },
            )
            .unwrap();
        fx.fts_index
            .lock()
            .await
            .add(namespace, id, &record.summary, None, &[])
            .unwrap();
    }
    fx.graph
        .write()
        .await
        .add_edge(
            ids[0],
            ids[1],
            crate::model::EdgeType::Associative,
            1.0,
            false,
        )
        .unwrap();
    let clock = Arc::new(crate::time::ManualClock::new(100));
    let engine = QueryEngine::new(
        Arc::new(StorageNamespaceResolver::new(fx.storage.clone())),
        Arc::new(SingleEmbeddingRegistry::new(Arc::new(
            crate::embedding::PassthroughProvider::new(4),
        ))),
        Arc::new(SharedVectorIndexRegistry::new(fx.vector_index.clone())),
        Arc::new(SharedFtsIndexRegistry::new(fx.fts_index.clone())),
        Arc::new(SharedEntityIndexReader::new(fx.entity_index.clone())),
        Arc::new(NoCache),
        Arc::new(StorageMetadataAdapter::new(fx.storage.clone())),
        Arc::new(SharedGraphReader::new(fx.graph.clone())),
        Arc::new(NoRif),
        Arc::new(
            StorageAccessRecorder::new(fx.storage.clone(), fx.cache.clone())
                .with_clock(clock.clone()),
        ),
    )
    .with_lifecycle_inputs(clock, Default::default(), 1.0);
    let query: SearchQuery = serde_json::from_value(serde_json::json!({
        "text":"private-payload-needle", "queryEmbedding":[1.0,0.0,0.0,0.0],
        "namespace":"default", "limit":2, "recordAccess":false, "graphDepth":1,
        "filter":{"requireTags":["project/allowed"]}
    }))
    .unwrap();
    fx.storage.write().unwrap().sync().unwrap();
    // Read live canonical bytes directly. Offline validation deliberately refuses
    // a copied redb header while its writer is still open.
    let files = [
        fx.path().join("meta.db"),
        fx.path().join("fulltext.dat"),
        fx.namespace_dir("default").join("vectors.dat"),
        fx.namespace_dir("private-namespace").join("vectors.dat"),
    ];
    let before: Vec<_> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let off = engine
        .search_with_semantics(query.clone(), RetrievalSemantics::V2(VersionMode::History))
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await
        .unwrap();
    let capture = Capture::default();
    let on = engine
        .search_with_semantics(query.clone(), RetrievalSemantics::V2(VersionMode::History))
        .with_subscriber(subscriber(capture.clone()))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&off.response.results).unwrap(),
        serde_json::to_value(&on.response.results).unwrap()
    );
    assert_eq!(
        files
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect::<Vec<_>>(),
        before
    );
    let trace = events(&capture);
    let one = |kind: &str| trace.iter().find(|e| e["kind"] == kind).unwrap();
    assert_eq!(one("canonical_read")["examined"], 3);
    assert_eq!(one("canonical_read")["encoded_record_bytes"], target_bytes);
    assert_eq!(one("eligibility")["eligible"], 2);
    assert_eq!(one("eligibility")["rejected"], 1);
    assert_eq!(one("vector_scan")["examined"], 4);
    assert_eq!(one("vector_scan")["dot_products"], 2);
    assert_eq!(one("fts_rows")["examined"], 3);
    assert_eq!(one("fts_rows")["eligible"], 2);
    assert_eq!(one("graph_work")["examined_edges"], 2);
    assert_eq!(
        one("pipeline_accounting")["elapsed_ns"],
        one("pipeline_accounting")["accounted_ns"]
    );
    for stage in [
        "storage.read.queue",
        "storage.read.wait",
        "storage.read.hold",
        "vector.lock_wait",
        "fts.lock_hold",
    ] {
        assert!(trace.iter().any(|e| e["stage"] == stage), "{stage}");
    }
    let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for secret in [
        "private-payload-needle",
        "private-namespace",
        "project/allowed",
    ] {
        assert!(!raw.contains(secret));
    }
    for (id, _) in fx.storage.read().unwrap().scan_all().unwrap() {
        assert!(!raw.contains(&id.to_string()));
    }

    // Same real writer path emits commit/publication costs when explicitly true.
    let mut write_query = query;
    write_query.record_access = true;
    engine
        .search(write_query)
        .with_subscriber(subscriber(capture.clone()))
        .await
        .unwrap();
    let trace = events(&capture);
    assert!(
        trace
            .iter()
            .any(|e| e["kind"] == "exposure" && e["receipt_count"] == 1)
    );
    assert!(
        trace
            .iter()
            .any(|e| e["stage"] == "exposure.cache_publication")
    );
}

#[tokio::test]
async fn performance_errors_cancellation_and_disabled_collection_are_explicit() {
    let capture = Capture::default();
    async {
        measure("test.outer", false, async {
            measure::<(), ()>("test.inner", false, async { Ok(()) }).await
        })
        .await
        .unwrap();
        let error: Result<(), ()> = measure("test.error", false, async { Err(()) }).await;
        assert!(error.is_err());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                measure::<(), ()>("test.cancel", false, std::future::pending())
            )
            .await
            .is_err()
        );
    }
    .with_subscriber(subscriber(capture.clone()))
    .await;
    let raw: Vec<serde_json::Value> = String::from_utf8(capture.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let inner = raw
        .iter()
        .find(|e| e["fields"]["stage"] == "test.inner")
        .unwrap();
    let spans = inner["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0]["operation"], "test.outer");
    assert_eq!(spans[1]["operation"], "test.inner");
    assert_ne!(spans[0]["trace_id"], spans[1]["trace_id"]);
    let trace = events(&capture);
    assert!(
        trace
            .iter()
            .any(|e| e["stage"] == "test.error" && e["outcome"] == "error")
    );
    assert!(
        trace
            .iter()
            .any(|e| e["stage"] == "test.cancel" && e["outcome"] == "cancelled_error_or_unwound")
    );
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
        assert!(!enabled());
        assert!(Timer::new("disabled").start.is_none());
        assert!(Timeline::new().start.is_none());
        assert!(Worker::new("disabled").context.is_none());
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn performance_sink_io_failure_preserves_actual_committed_not_ready_receipts() {
    struct FailedSink;
    impl Write for FailedSink {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("fixture sink failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let fx = Fixture::new();
    let id = fx.insert_memory().await;
    let receipts = crate::decay::observation::record_exposures(
        fx.storage.clone(),
        fx.cache.clone(),
        vec![(id, crate::model::AccessKind::DirectRetrieval)],
        100,
    )
    .await
    .unwrap();
    assert_eq!(receipts.len(), 1);
    let operation = receipts[0].operation_id;
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("off,recalld::performance=trace")
        .with_writer(|| FailedSink)
        .finish();
    let error = tracing::subscriber::with_default(subscriber, || {
        crate::decay::observation::Observed {
            value: (),
            receipts,
        }
        .finish(Err::<(), _>(SearchError::Unavailable(
            "fixture final admission failure".into(),
        )))
        .unwrap_err()
    });
    let SearchError::Observation(error) = error else {
        panic!("lost committed classification")
    };
    assert_eq!(error.commit_state, "committed");
    assert!(!error.ready);
    assert_eq!(error.receipts[0].operation_id, operation);
}
