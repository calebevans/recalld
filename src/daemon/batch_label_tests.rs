//! Synthetic actual MCP and framed daemon regressions over isolated temp stores.
use super::*;
use crate::storage::StorageEngine as _;
use crate::storage::batch::{BatchInput, BatchKind, BatchOutcome, BatchStatus};
use crate::test_support::{Fixture, StubBridge};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Provider(AtomicUsize);
#[async_trait::async_trait]
impl crate::embedding::EmbeddingProvider for Provider {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1., 0., 0., 0.])
    }
    fn dimensions(&self) -> usize {
        4
    }
    fn model_name(&self) -> &str {
        "synthetic-batch-labels"
    }
}
fn adapter(f: &Fixture, p: Arc<Provider>) -> McpStorageAdapter {
    McpStorageAdapter::new(
        f.storage.clone(),
        f.cache.clone(),
        p,
        f.vector_index.clone(),
        f.fts_index.clone(),
        f.entity_index.clone(),
        f.graph.clone(),
        Arc::new(Default::default()),
        chrono_tz::UTC,
    )
}
fn cases() -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for field in ["entities", "topics", "emotions", "tags"] {
        let labels: &[&str] = if field == "tags" {
            &["", "bad tag"]
        } else {
            &["John Smith", "José", "Alice's", ""]
        };
        for label in labels {
            let mut item = json!({"summary":"unchanged input","fullText":"must not stage","checkDuplicates":false});
            item[field] = json!([label]);
            out.push((field, item));
        }
    }
    out
}
fn payloads(f: &Fixture) -> (Vec<u8>, Vec<u8>) {
    let store = f.storage.read().unwrap();
    let text = store
        .meta_store()
        .text_selection_for_recovery()
        .unwrap()
        .unwrap();
    (
        std::fs::read(f.path().join(text.filename)).unwrap(),
        std::fs::read(f.path().join("default/vectors.dat")).unwrap(),
    )
}
fn assert_rejected(out: &BatchOutcome, fields: &[(&str, Value)]) {
    assert_eq!(
        (out.total, out.failed, out.succeeded, out.unresolved),
        (fields.len(), fields.len(), 0, 0)
    );
    assert!(out.reservation_receipt.is_some());
    assert!(out.bookkeeping_receipts.is_empty());
    for (i, ((field, _), item)) in fields.iter().zip(&out.items).enumerate() {
        assert_eq!(item.index, i as u32);
        assert_eq!(item.status, BatchStatus::Error);
        assert!(item.receipt.is_none());
        assert_eq!(item.error.as_ref().unwrap().code, "INVALID_INPUT");
        assert_eq!(item.error.as_ref().unwrap().field.as_deref(), Some(*field));
    }
}
async fn socket_call(
    client: &mut tokio::net::UnixStream,
    id: u64,
    input: &BatchInput,
) -> DaemonResponse {
    write_framed_request(
        client,
        &DaemonRequest {
            jsonrpc: "2.0".into(),
            id,
            method: "mutate_batch".into(),
            params: serde_json::to_value(input).unwrap(),
        },
        LEGACY_MAX_MESSAGE_SIZE,
    )
    .await
    .unwrap();
    read_framed_response(client, LEGACY_MAX_MESSAGE_SIZE)
        .await
        .unwrap()
}
#[tokio::test]
async fn batch_labels_actual_mcp_and_daemon_reject_before_provider_and_child_payload() {
    for socket in [false, true] {
        let f = Fixture::new();
        let p = Arc::new(Provider::default());
        let concrete = Arc::new(adapter(&f, p.clone()));
        let fields = cases();
        let before = payloads(&f);
        let state = f.storage.read().unwrap().control_state().unwrap();
        let input = BatchInput {
            result_version: 1,
            kind: BatchKind::Insert,
            items: fields.iter().map(|(_, v)| v.clone()).collect(),
            default_namespace: "default".into(),
            store_id: Some(state.store_id),
            idempotency_key: Some("invalid-labels".into()),
            batch_operation_id: None,
        };
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let stub = Arc::new(StubBridge::default());
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(handle_connection(
            stub.clone(),
            concrete.clone(),
            stub.clone(),
            stub,
            server,
            Arc::new(AtomicU32::new(1)),
            Arc::new(AtomicI64::new(0)),
            rx,
            tx.clone(),
        ));
        let mut bridge = StubBridge::default().into_bridge();
        bridge.storage = concrete;
        let mut original = None;
        for attempt in 0..3 {
            let mut request = input.clone();
            if attempt == 2 {
                request.items[0]["entities"] = json!(["changed invalid label"]);
            }
            let result = if socket {
                let reply = socket_call(&mut client, attempt + 1, &request).await;
                if attempt == 2 {
                    assert!(reply.error.is_some());
                    None
                } else {
                    assert!(reply.error.is_none());
                    Some(serde_json::from_value::<BatchOutcome>(reply.result.unwrap()).unwrap())
                }
            } else {
                let reply=crate::mcp::tools::dispatch_tool(&bridge,"store_memories",json!({"memories":request.items,"storeId":request.store_id,"idempotencyKey":request.idempotency_key,"batchResultVersion":1})).await;
                assert_eq!(reply.is_error, Some(true));
                let crate::mcp::protocol::ContentBlock::Text { text } = &reply.content[0];
                if attempt == 2 {
                    assert!(text.contains("IDEMPOTENCY_CONFLICT"), "{text}");
                    None
                } else {
                    let value: Value = serde_json::from_str(text).unwrap();
                    Some(serde_json::from_value::<BatchOutcome>(value["batch"].clone()).unwrap())
                }
            };
            if let Some(out) = result {
                assert_rejected(&out, &fields);
                if let Some(parent) = original {
                    assert_eq!(parent, out.batch_operation_id);
                    assert!(out.reservation_receipt.as_ref().unwrap().replayed);
                }
                original = Some(out.batch_operation_id);
            }
            let store = f.storage.read().unwrap();
            assert_eq!(
                store.control_state().unwrap().committed_generation.get(),
                state.committed_generation.get() + 1
            );
            assert!(store.scan_all().unwrap().is_empty());
            assert!(store.load_all_edges().unwrap().is_empty());
            assert!(store.meta_store().pending_effects().unwrap().is_empty());
            drop(store);
            assert_eq!(payloads(&f), before);
            assert_eq!(p.0.load(Ordering::SeqCst), 0);
        }
        tx.send(true).unwrap();
        task.await.unwrap();
        drop(bridge);
        let dir = f.close();
        let engine = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
        let manifest = engine
            .meta_store()
            .lookup_batch(state.store_id, original.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(manifest.unresolved(), 0);
        assert!(engine.scan_all().unwrap().is_empty());
    }
}

#[tokio::test]
async fn batch_labels_positive_normalization_survives_rebuild_and_reopen() {
    let f = Fixture::new();
    let adapter = adapter(&f, Arc::new(Provider::default()));
    let input = BatchInput {
        result_version: 1,
        kind: BatchKind::Insert,
        items: vec![
            json!({"summary":"canonical labels","tags":["USER/Chosen"],"entities":["Alice-1"],"topics":["RUST/lang"],"emotions":["Curious"],"checkDuplicates":false}),
        ],
        default_namespace: "default".into(),
        store_id: Some(f.storage.read().unwrap().control_state().unwrap().store_id),
        idempotency_key: Some("positive-labels".into()),
        batch_operation_id: None,
    };
    let result = adapter.mutate_batch(input.clone()).await.unwrap();
    assert!(result.all_ready());
    let replay = adapter.mutate_batch(input).await.unwrap();
    assert!(replay.all_ready());
    assert_eq!(result.items[0].operation_id, replay.items[0].operation_id);
    drop(adapter);
    let dir = f.close();
    let engine = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
    let records = engine.scan_all().unwrap();
    assert_eq!(records.len(), 1);
    let tags: Vec<_> = records[0].1.tags.iter().map(ToString::to_string).collect();
    for tag in [
        "user/chosen",
        "entity/alice-1",
        "topic/rust/lang",
        "emotion/curious",
    ] {
        assert!(tags.iter().any(|t| t == tag), "{tag}: {tags:?}");
    }
}
