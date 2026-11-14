//! Actual routers/tools/framed RPC against temporary canonical storage.
use super::*;
use crate::storage::control::{MutationOutcome, MutationScope, WriteReceipt};

async fn rpc(h: &Harness, method: &str, params: Value) -> crate::daemon::protocol::DaemonResponse {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observation.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let bridge = h.bridge();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_framed_message(&mut socket, MAX_MESSAGE_SIZE)
            .await
            .unwrap();
        let result = dispatch(
            bridge.search.as_ref(),
            bridge.storage.as_ref(),
            bridge.namespaces.as_ref(),
            bridge.health.as_ref(),
            &request.method,
            request.params,
        )
        .await;
        let response = match result {
            Ok(value) => DaemonResponse::success(request.id, value),
            Err(error) => DaemonResponse::error(request.id, (&error).into()),
        };
        write_framed_message(&mut socket, &response, MAX_MESSAGE_SIZE)
            .await
            .unwrap();
    });
    let mut socket = tokio::net::UnixStream::connect(&path).await.unwrap();
    crate::daemon::protocol::write_framed_request(
        &mut socket,
        &crate::daemon::protocol::DaemonRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: method.into(),
            params,
        },
        MAX_MESSAGE_SIZE,
    )
    .await
    .unwrap();
    let response = crate::daemon::protocol::read_framed_response(&mut socket, MAX_MESSAGE_SIZE)
        .await
        .unwrap();
    server.await.unwrap();
    response
}

async fn call(
    h: &Harness,
    transport: &str,
    operation: &str,
    access: bool,
    empty: bool,
) -> (bool, Value) {
    let id = h.ids[0];
    let mut params = json!({"recordAccess": access, "namespace": "default"});
    match operation {
        "get" => {
            params["id"] = json!(id.to_string());
        }
        "search" => {
            params["query"] = json!("fixture");
            params["limit"] = json!(1);
            params["depth"] = json!(0);
            if empty {
                params["minStrength"] = json!(1.1);
            }
        }
        "similar" => {
            params["id"] = json!(id.to_string());
            params["limit"] = json!(1);
            params["minScore"] = json!(if empty { 0.99 } else { 0.1 });
            params["sameNamespace"] = json!(true);
        }
        _ => unreachable!(),
    }
    match transport {
        "http" => {
            let (status, body) = match operation {
                "get" => {
                    http_call(
                        h.http(),
                        "GET",
                        &format!("/memories/{id}?recordAccess={access}&namespace=default"),
                        Value::Null,
                    )
                    .await
                }
                "search" => http_call(h.http(), "POST", "/search", params).await,
                _ => http_call(h.http(), "POST", &format!("/similar/{id}"), params).await,
            };
            (status.is_success(), body)
        }
        "mcp" => {
            let tool = match operation {
                "get" => "get_memory",
                "search" => "recall_memories",
                _ => "find_similar_memories",
            };
            let result = crate::mcp::tools::dispatch_tool(&h.bridge(), tool, params).await;
            let crate::mcp::protocol::ContentBlock::Text { text } = &result.content[0];
            (result.is_error != Some(true), json!(text))
        }
        _ => {
            let method = match (operation, access) {
                ("get", true) => "get_memory_scoped",
                ("get", false) => "get_memory_scoped_read_only",
                ("search", _) => "search",
                (_, true) => "find_similar_scoped",
                (_, false) => "find_similar_scoped_read_only",
            };
            if access && operation != "search" {
                params.as_object_mut().unwrap().remove("recordAccess");
            }
            if operation == "search" {
                let mut search = input(Some(access));
                search.limit = 1;
                search.depth = 0;
                search.include_neighbors = false;
                params = serde_json::to_value(search).unwrap();
            }
            let response = rpc(h, method, params).await;
            assert!(response.error.is_none() || response.result.is_none());
            (
                response.error.is_none(),
                json!(response.error.map(|e| e.message).unwrap_or_default()),
            )
        }
    }
}

fn observation(body: &Value) -> Value {
    if let Some(value) = body.get("observation") {
        return value.clone();
    }
    let text = body.as_str().unwrap();
    let start = text.find("{\"commitState\"").expect(text);
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_postcommit_admission_faults_retain_receipts_and_exactly_once_history() {
    for fault in [1, 2] {
        // admission error and a successfully read but changed stamp
        for transport in ["http", "mcp", "daemon"] {
            for operation in ["get", "search", "similar"] {
                let h = Harness::new_with_distinct_vectors(true).await;
                let ids = h.ids.clone();
                let counts = event_counts(&h, &ids);
                let generation = control_generation(&h);
                let before = h.snapshot().await;
                h.f.storage
                    .read()
                    .unwrap()
                    .meta_store()
                    .observation_fault
                    .store(fault, Ordering::SeqCst);
                // Original false mode must bypass the actual writer and the hook.
                let (ok, body) = call(&h, transport, operation, false, false).await;
                assert!(ok, "{transport}/{operation}: {body}");
                h.assert_quiet(&before).await;
                assert_eq!(control_generation(&h), generation);
                assert_eq!(
                    h.f.storage
                        .read()
                        .unwrap()
                        .meta_store()
                        .observation_fault
                        .load(Ordering::SeqCst),
                    fault
                );
                if operation == "similar" {
                    let (ok, body) = call(&h, transport, operation, true, true).await;
                    assert!(ok, "empty {transport}: {body}");
                    assert_eq!(event_counts(&h, &ids), counts);
                    assert_eq!(control_generation(&h), generation);
                }
                let (ok, body) = call(&h, transport, operation, true, false).await;
                assert!(!ok, "{fault}/{transport}/{operation}: {body}");
                println!(
                    "postcommit wire transport={transport} operation={operation} fault={fault}: {body}"
                );
                let error = observation(&body);
                assert_eq!(error["commitState"], "committed", "{body}");
                assert_eq!(error["ready"], false);
                assert!(
                    error["message"]
                        .as_str()
                        .unwrap()
                        .contains("INDEX_NOT_READY")
                );
                assert!(
                    !body.to_string().contains("fixture"),
                    "result text escaped: {body}"
                );
                let receipts: Vec<WriteReceipt> =
                    serde_json::from_value(error["receipts"].clone()).unwrap();
                assert_eq!(receipts.len(), 1);
                assert_eq!(receipts[0].committed_generation.get(), generation + 1);
                let MutationOutcome::ExposuresRecorded { accesses } = &receipts[0].outcome else {
                    panic!("{receipts:?}")
                };
                assert_eq!(accesses.len(), 1);
                if operation == "similar" {
                    assert_ne!(accesses[0].memory_id, ids[0]);
                }
                let exposed = accesses[0].memory_id;
                // Reopen, rather than clear the failed admission gate, then verify the
                // original receipt and event persisted exactly once.
                let Harness {
                    f, engine, access, ..
                } = h;
                drop(engine);
                drop(access);
                let dir = f.close();
                let storage = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
                let receipt = storage
                    .meta_store()
                    .lookup_operation(
                        receipts[0].store_id,
                        MutationScope::Namespace(NamespaceId::new(1)),
                        receipts[0].operation_id,
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(receipt, receipts[0]);
                for (index, id) in ids.iter().enumerate() {
                    assert_eq!(
                        storage
                            .get_record(*id)
                            .unwrap()
                            .unwrap()
                            .access_history
                            .len(),
                        counts[index] + usize::from(*id == exposed)
                    );
                }
                println!(
                    "postcommit fault={fault} transport={transport} operation={operation} receipt={} generation={}",
                    receipt.operation_id,
                    receipt.committed_generation.get()
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_hydration_failure_never_observes_or_loses_text_error() {
    for transport in ["http", "mcp", "daemon"] {
        for operation in ["get", "search"] {
            let h = Harness::new_with_distinct_vectors(true).await;
            let records: Vec<_> = h
                .ids
                .iter()
                .map(|id| {
                    let mut record = h.f.record(*id).unwrap();
                    record.text_offset = 1_000_000;
                    record.text_length = 16;
                    (*id, record)
                })
                .collect();
            h.f.storage
                .read()
                .unwrap()
                .meta_store()
                .batch_update_records(&records)
                .unwrap();
            let before = h.snapshot().await;
            for access in [false, true] {
                let (ok, body) = call(&h, transport, operation, access, false).await;
                assert!(!ok, "{transport}/{operation}/{access}: {body}");
                assert!(!body.to_string().contains("fixture"));
                h.assert_quiet(&before).await;
                println!(
                    "precommit hydration transport={transport} operation={operation} access={access} wire={body}"
                );
            }
        }
    }
}
