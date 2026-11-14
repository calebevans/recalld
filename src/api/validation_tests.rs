//! HTTP regressions over the same temporary storage used by the exported MCP probes.
use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::api::adapters::*;
use crate::storage::StorageEngine as _;
use crate::test_support::Fixture;

fn fixture_clients(f: &Fixture) -> (Router, crate::mcp::bridge::McpBridge) {
    use crate::search::adapters::*;
    let embedding = Arc::new(crate::embedding::PassthroughProvider::new(
        crate::test_support::DIM,
    ));
    let engine = Arc::new(crate::search::QueryEngine::new(
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
        Arc::new(StorageAccessRecorder::new(f.storage.clone())),
    ));
    let mut bridge = crate::test_support::StubBridge::default().into_bridge();
    bridge.search = Arc::new(crate::mcp::bridge_adapters::McpSearchAdapter::new(
        engine.clone(),
        embedding.clone(),
        f.storage.clone(),
        f.graph.clone(),
        chrono_tz::UTC,
    ));
    let config = Arc::new(crate::config::RecalldConfig::default());
    let state = AppState::new(
        Arc::new(SearchPipelineAdapter::new(
            engine,
            embedding,
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
    );
    (
        crate::api::routes::router(state, &Default::default()),
        bridge,
    )
}

async fn post(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_quality_rejects_exported_probe_values_without_mutation() {
    let f = Fixture::new();
    let (app, _) = fixture_clients(&f);
    let id = f.insert_memory().await;
    let path = format!("/memories/{id}/reinforce");
    let before = format!("{:?}", f.record(id).unwrap());
    for value in [
        json!(-1),
        json!(1.5),
        json!(3.0),
        json!("3"),
        json!(true),
        Value::Null,
        json!([]),
        json!({}),
        json!(0),
        json!(5),
        json!(256),
        json!(259),
        json!(u64::MAX),
    ] {
        let (status, body) = post(&app, &path, json!({"quality": value})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}: {body}");
        assert_eq!(body["field"], "quality", "{body}");
        assert_eq!(before, format!("{:?}", f.record(id).unwrap()));
    }
    for value in [
        json!({}),
        json!({"quality": 1}),
        json!({"quality": 2}),
        json!({"quality": 3}),
        json!({"quality": 4}),
    ] {
        let before = format!("{:?}", f.record(id).unwrap());
        let (status, body) = post(&app, &path, value).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_ne!(before, format!("{:?}", f.record(id).unwrap()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_namespace_rejects_exported_probe_values_without_creation() {
    let f = Fixture::new();
    let (app, _) = fixture_clients(&f);
    for value in [
        json!(-1),
        json!(1.5),
        json!(4.0),
        json!("4"),
        json!(true),
        Value::Null,
        json!([]),
        json!({}),
        json!(0),
        json!(8193),
        json!(65535),
        json!(65536),
        json!(65537),
        json!(u64::MAX),
    ] {
        let (status, body) = post(
            &app,
            "/namespaces",
            json!({"name": "invalid", "embeddingDim": value}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}: {body}");
        assert_eq!(body["field"], "embeddingDim", "{body}");
        assert!(
            f.storage
                .read()
                .unwrap()
                .get_namespace_by_name("invalid")
                .unwrap()
                .is_none()
        );
    }
    for (name, dim) in [("one", 1), ("maximum", 8192)] {
        let (status, body) = post(
            &app,
            "/namespaces",
            json!({"name": name, "embeddingDim": dim}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            f.storage
                .read()
                .unwrap()
                .get_namespace_by_name(name)
                .unwrap()
                .unwrap()
                .embedding_dim,
            dim
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_creation_typed_validation_and_unavailable_errors() {
    use crate::storage::StorageError;
    use axum::response::IntoResponse;
    let operation_id = uuid::Uuid::new_v4();
    for (error, status) in [
        (
            StorageError::InvalidMutation("invalid policy".into()),
            StatusCode::BAD_REQUEST,
        ),
        (
            StorageError::RecoveryRequired("reopen".into()),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            StorageError::CommittedEffectsPending {
                operation_id,
                source: Box::new(StorageError::RecoveryRequired("publish".into())),
            },
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            StorageError::CommitOutcomeUnknown {
                operation_id,
                source: redb::CommitError::Storage(redb::StorageError::Io(std::io::Error::other(
                    "lost commit acknowledgement",
                ))),
            },
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let carries_operation = matches!(
            &error,
            StorageError::CommittedEffectsPending { .. }
                | StorageError::CommitOutcomeUnknown { .. }
        );
        let response = namespace_create_error(Box::new(error)).into_response();
        assert_eq!(response.status(), status);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains("already exists"));
        if carries_operation {
            assert!(body.contains(&operation_id.to_string()));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_creation_storage_failure_is_not_a_duplicate() {
    let f = Fixture::new();
    let (app, _) = fixture_clients(&f);
    let (status, _) = post(&app, "/namespaces", json!({"name":"duplicate"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = post(&app, "/namespaces", json!({"name":"duplicate"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let before = f.storage.read().unwrap().control_state().unwrap();

    // Force a real catalog filesystem failure while keeping its database handles
    // open. Restore the synthetic root before checking the response or dropping it.
    let parked = tempfile::tempdir().unwrap();
    let store = parked.path().join("store");
    std::fs::rename(f.path(), &store).unwrap();
    std::fs::write(f.path(), b"not a directory").unwrap();
    let (status, body) = post(&app, "/namespaces", json!({"name":"io-failure"})).await;
    std::fs::remove_file(f.path()).unwrap();
    std::fs::rename(&store, f.path()).unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(!body.to_string().contains("already exists"), "{body}");
    let storage = f.storage.read().unwrap();
    assert_eq!(storage.control_state().unwrap(), before);
    assert!(
        storage
            .get_namespace_by_name("io-failure")
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_option_parity_and_no_creation_on_invalid_input() {
    let f = Fixture::new();
    let (app, _) = fixture_clients(&f);
    let mut bridge = crate::test_support::StubBridge::default().into_bridge();
    bridge.namespaces = Arc::new(f.namespace_adapter());
    for field in [
        "initialStability",
        "desiredRetention",
        "decayRateMultiplier",
    ] {
        let mut bad_values = vec![
            json!(-1),
            json!("1"),
            json!(true),
            json!([]),
            json!({}),
            json!(1e100),
        ];
        if field == "initialStability" {
            bad_values.extend([json!(0), json!(1e-100)]);
        }
        if field == "desiredRetention" {
            bad_values.push(json!(1.01));
        }
        for value in bad_values {
            let mut args = json!({"name": "invalid"});
            args[field] = value;
            let (status, body) = post(&app, "/namespaces", args.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{args}: {body}");
            assert_eq!(body["field"], field, "{body}");
            let result =
                crate::mcp::tools::dispatch_tool(&bridge, "create_namespace", args.clone()).await;
            assert_eq!(result.is_error, Some(true), "{args}");
            assert!(serde_json::to_string(&result).unwrap().contains(field));
            assert!(
                f.storage
                    .read()
                    .unwrap()
                    .get_namespace_by_name("invalid")
                    .unwrap()
                    .is_none()
            );
        }
    }
    // Null floats retain v1 omission semantics; dimension omission inherits 4.
    for (index, options) in [
        json!({"initialStability": null, "desiredRetention": null, "decayRateMultiplier": null}),
        json!({"initialStability": 1, "desiredRetention": 0, "decayRateMultiplier": 0}),
        json!({"initialStability": 3e38, "desiredRetention": 1, "decayRateMultiplier": 3e38}),
    ]
    .into_iter()
    .enumerate()
    {
        for transport in ["http", "mcp"] {
            let name = format!("{transport}-{index}");
            let mut args = options.clone();
            args["name"] = json!(name);
            if transport == "http" {
                let (status, body) = post(&app, "/namespaces", args).await;
                assert_eq!(status, StatusCode::CREATED, "{body}");
            } else {
                let result =
                    crate::mcp::tools::dispatch_tool(&bridge, "create_namespace", args).await;
                assert_ne!(result.is_error, Some(true), "{result:?}");
            }
            assert_eq!(
                f.storage
                    .read()
                    .unwrap()
                    .get_namespace_by_name(&name)
                    .unwrap()
                    .unwrap()
                    .embedding_dim,
                crate::test_support::DIM as u32
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_duplicate_coverage_reports_caps_missing_vectors_and_empty_samples() {
    let f = Fixture::new();
    let (app, bridge) = fixture_clients(&f);
    // Even an empty cluster response must not claim an exhaustive scan.
    let result =
        crate::mcp::tools::dispatch_tool(&bridge, "find_similar_memories", json!({"mode": "scan"}))
            .await;
    assert_ne!(result.is_error, Some(true));
    let response: serde_json::Value = serde_json::from_str(
        serde_json::to_value(&result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let coverage = &response["coverage"];
    assert_eq!(coverage["complete"], false);
    assert_eq!(coverage["sampleCap"], 200);
    assert_eq!(coverage["neighborCap"], 11);
    assert_eq!(coverage["countsAvailable"], false);
    for key in [
        "sampledCount",
        "eligibleCount",
        "skippedVectorCount",
        "truncated",
    ] {
        assert!(coverage[key].is_null(), "{key}");
    }
    for count in 0..=2 {
        if count > 0 {
            f.insert_memory().await;
        }
        let (status, body) = post(
            &app,
            "/namespaces/default/duplicates",
            json!({"maxMemories": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let coverage = &body["data"]["coverage"];
        assert_eq!(coverage["eligibleCount"], count);
        assert_eq!(coverage["sampledCount"], count.min(1));
        assert_eq!(coverage["skippedVectorCount"], count.min(1));
        assert_eq!(coverage["truncated"], count > 1);
        assert_eq!(coverage["complete"], false);
        assert_eq!(coverage["neighborCap"], Value::Null);
    }
    for args in [
        json!({"threshold": -0.1}),
        json!({"threshold": 1.1}),
        json!({"maxMemories": 0}),
        json!({"maxMemories": 10001}),
    ] {
        assert_eq!(
            post(&app, "/namespaces/default/duplicates", args).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}
