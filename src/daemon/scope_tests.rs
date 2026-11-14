//! Actual remote adapters and framed sockets: assertions cannot fall back to legacy calls.
use super::*;
use crate::daemon::bridge_adapters::{RemoteSearchAdapter, RemoteStorageAdapter};
use crate::test_support::{Fixture, StubBridge};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_assertions_remote_round_trip_and_old_daemon_fail_closed() {
    for legacy in [false, true] {
        let f = Arc::new(Fixture::new());
        f.create_namespace("other");
        let id = f.insert_memory().await;
        let before = format!("{:?}", f.record(id));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scoped.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let fixture = f.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let search = fixture.search_adapter();
            let storage = fixture.storage_adapter();
            let unused = StubBridge::default();
            for i in 0..5 {
                let req = read_framed_message(&mut socket, MAX_MESSAGE_SIZE)
                    .await
                    .unwrap();
                assert_eq!(req.method.ends_with("_scoped"), i < 4);
                let result = if legacy && req.method.ends_with("_scoped") {
                    Err(BridgeError::InvalidInput(format!(
                        "unknown method: {}",
                        req.method
                    )))
                } else {
                    dispatch(&search, &storage, &unused, &unused, &req.method, req.params).await
                };
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
        let storage = RemoteStorageAdapter::new(client.clone());
        let search = RemoteSearchAdapter::new(client);
        let errors = [
            storage.get_memory(id, Some("other")).await.unwrap_err(),
            storage
                .reinforce_memory(id, 4, Some("other"))
                .await
                .unwrap_err(),
            storage.delete_memory(id, Some("other")).await.unwrap_err(),
            search
                .find_similar(id, 10, None, true, Some("other"))
                .await
                .unwrap_err(),
        ];
        for error in errors {
            if legacy {
                assert!(error.to_string().contains("unknown method"));
            } else {
                assert!(
                    matches!(error, BridgeError::NamespaceMismatch(_)),
                    "{error}"
                );
            }
        }
        assert_eq!(before, format!("{:?}", f.record(id)));
        assert!(storage.get_memory(id, None).await.unwrap().is_some());
        server.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_assertions_rpc_requires_assertion_and_legacy_rpc_rejects_ignored_fields() {
    let unused = StubBridge::default(); // Any dispatch to a subsystem would panic.
    for method in [
        "get_memory",
        "delete_memory",
        "reinforce_memory",
        "find_similar",
    ] {
        for params in [
            json!({}),
            json!({"namespace":null}),
            json!({"namespace":42}),
            json!({"namespace":""}),
        ] {
            let error = dispatch(
                &unused,
                &unused,
                &unused,
                &unused,
                &format!("{method}_scoped"),
                params,
            )
            .await
            .unwrap_err();
            assert!(matches!(error, BridgeError::InvalidInput(_)));
        }
        let error = dispatch(
            &unused,
            &unused,
            &unused,
            &unused,
            &format!("{method}_scoped"),
            json!({"namespace":"default", "recordAccess":false}),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, BridgeError::InvalidInput(_)));
        let error = dispatch(
            &unused,
            &unused,
            &unused,
            &unused,
            method,
            json!({"namespace":"default"}),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, BridgeError::InvalidInput(_)));
    }
}
