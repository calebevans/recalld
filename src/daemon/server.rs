use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::watch;
use uuid::Uuid;

use super::lifecycle;
use super::protocol::*;
use crate::Recalld;
use crate::mcp::bridge::{
    BridgeError, CreateNamespaceInput, DeleteNamespaceInput, HealthChecker, ListMemoriesInput,
    ListTagsInput, NamespaceRegistry, SearchInput, SearchPipeline,
    StorageEngine as BridgeStorageEngine, StoreInput,
};
use crate::mcp::bridge_adapters::*;
use crate::model::MemoryId;

/// The Recalld daemon server, listening on a Unix socket for RPC requests.
pub struct DaemonServer {
    _system: Recalld,
    search: Arc<dyn SearchPipeline>,
    storage: Arc<dyn BridgeStorageEngine>,
    namespaces: Arc<dyn NamespaceRegistry>,
    health: Arc<dyn HealthChecker>,
    connection_count: Arc<AtomicU32>,
    last_activity: Arc<AtomicI64>,
    shutdown_tx: watch::Sender<bool>,
}

impl DaemonServer {
    /// Creates a new daemon server backed by the given `Recalld` system.
    pub fn new(system: Recalld) -> Self {
        let tz = crate::time::resolve_timezone(&system.config().timezone);

        let search: Arc<dyn SearchPipeline> = Arc::new(McpSearchAdapter::new(
            system.query_engine().clone(),
            system.embedding().clone(),
            system.storage().clone(),
            system.graph().clone(),
            tz,
        ));
        let storage: Arc<dyn BridgeStorageEngine> = Arc::new(
            McpStorageAdapter::new(
                system.storage().clone(),
                system.cache().clone(),
                system.embedding().clone(),
                system.vector_index().clone(),
                system.fts_index().clone(),
                system.entity_index().clone(),
                system.graph().clone(),
                std::sync::Arc::new(system.config().clone()),
                tz,
            )
            .with_clock(system.clock().clone()),
        );
        let namespaces: Arc<dyn NamespaceRegistry> = Arc::new(McpNamespaceAdapter::new(
            system.storage().clone(),
            system.cache().clone(),
            system.vector_index().clone(),
            system.fts_index().clone(),
            system.entity_index().clone(),
            system.graph().clone(),
            tz,
        ));
        let health: Arc<dyn HealthChecker> =
            Arc::new(McpHealthAdapter::new(system.storage().clone()));

        let (shutdown_tx, _) = watch::channel(false);

        Self {
            _system: system,
            search,
            storage,
            namespaces,
            health,
            connection_count: Arc::new(AtomicU32::new(0)),
            last_activity: Arc::new(AtomicI64::new(lifecycle::now_millis())),
            shutdown_tx,
        }
    }

    /// Binds to `socket_path` and serves RPC requests until shutdown.
    pub async fn run(self, socket_path: &Path, idle_timeout: Duration) -> std::io::Result<()> {
        lifecycle::cleanup_stale_socket(socket_path)?;

        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(socket_path)?;
        lifecycle::write_pid_file(&lifecycle::pid_path())?;

        tracing::info!(path = %socket_path.display(), "daemon listening");

        tokio::spawn(lifecycle::idle_monitor(
            self.last_activity.clone(),
            self.connection_count.clone(),
            idle_timeout,
            self.shutdown_tx.clone(),
        ));

        let mut shutdown_rx = self.shutdown_tx.subscribe();

        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let current = self.connection_count.load(Ordering::Relaxed);
                            if current >= MAX_CONNECTIONS {
                                tracing::warn!(
                                    current,
                                    max = MAX_CONNECTIONS,
                                    "connection limit reached, dropping new connection"
                                );
                                drop(stream);
                                continue;
                            }

                            self.connection_count.fetch_add(1, Ordering::Relaxed);

                            tokio::spawn(handle_connection(
                                self.search.clone(),
                                self.storage.clone(),
                                self.namespaces.clone(),
                                self.health.clone(),
                                stream,
                                self.connection_count.clone(),
                                self.last_activity.clone(),
                                self.shutdown_tx.subscribe(),
                                self.shutdown_tx.clone(),
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(%e, "accept error");
                        }
                    }
                }

                _ = shutdown_rx.changed() => {
                    tracing::info!("shutdown signal received");
                    break;
                }

                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM, initiating shutdown");
                    let _ = self.shutdown_tx.send(true);
                    break;
                }

                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received SIGINT, initiating shutdown");
                    let _ = self.shutdown_tx.send(true);
                    break;
                }
            }
        }

        let _ = std::fs::remove_file(socket_path);
        let _ = std::fs::remove_file(lifecycle::pid_path());

        Ok(())
    }
}

async fn handle_connection(
    search: Arc<dyn SearchPipeline>,
    storage: Arc<dyn BridgeStorageEngine>,
    namespaces: Arc<dyn NamespaceRegistry>,
    health: Arc<dyn HealthChecker>,
    stream: tokio::net::UnixStream,
    connection_count: Arc<AtomicU32>,
    last_activity: Arc<AtomicI64>,
    mut shutdown_rx: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = writer;

    // Largest frame this peer is known to accept. Stays at the protocol v1
    // limit until the client announces v2 in its `ping`, so an old client
    // never receives a frame it would reject without draining.
    let mut peer_limit = LEGACY_MAX_MESSAGE_SIZE;

    // Consecutive recoverable frame errors, reset by every frame that parses.
    let mut frame_error_strikes: u32 = 0;

    loop {
        tokio::select! {
            msg = read_framed_message(&mut reader, MAX_MESSAGE_SIZE) => {
                match msg {
                    Ok(request) => {
                        frame_error_strikes = 0;
                        let method = request.method.clone();

                        if method == "ping" {
                            peer_limit = peer_frame_limit(&request.params);
                        }

                        let response = dispatch(
                            &*search, &*storage, &*namespaces, &*health,
                            &request.method, request.params,
                        ).await;

                        let resp = match response {
                            Ok(result) => DaemonResponse::success(request.id, result),
                            Err(e) => DaemonResponse::error(request.id, DaemonRpcError::from(&e)),
                        };

                        match write_framed_message(&mut writer, &resp, peer_limit).await {
                            Ok(()) => {}
                            // The result does not fit in a frame. Sending it anyway
                            // would desynchronize the client's stream, so send an
                            // error that does fit instead.
                            Err(FrameError::TooLarge { size, limit }) => {
                                tracing::warn!(
                                    method = %method, size, limit,
                                    "response exceeds the peer frame limit, replying with an error"
                                );
                                let fallback = DaemonResponse::error(
                                    request.id,
                                    DaemonRpcError {
                                        code: ERR_TOO_LARGE,
                                        message: format!(
                                            "response is {size} bytes, exceeding the {limit} byte \
                                             message limit. Reduce `limit`, or fetch large memories \
                                             individually with get_memory."
                                        ),
                                    },
                                );
                                if write_framed_message(&mut writer, &fallback, peer_limit)
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%e, "client write error, closing connection");
                                break;
                            }
                        }

                        if !matches!(method.as_str(), "ping" | "check_health" | "shutdown") {
                            last_activity.store(lifecycle::now_millis(), Ordering::Relaxed);
                        }

                        if method == "shutdown" {
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                    }
                    Err(e) => match action_for_frame_error(&e) {
                        // The stream is provably still on a frame boundary, so
                        // the connection survives; only this request is lost.
                        ConnectionAction::Reply(err) => {
                            frame_error_strikes += 1;
                            tracing::warn!(
                                %e,
                                strikes = frame_error_strikes,
                                max = MAX_CONSECUTIVE_FRAME_ERRORS,
                                "recoverable client frame error, replying and continuing"
                            );
                            let resp = DaemonResponse::error(UNATTRIBUTED_ID, err);
                            if write_framed_message(&mut writer, &resp, peer_limit).await.is_err() {
                                break;
                            }
                            // Answered, then hung up: a peer that only ever
                            // sends garbage is not worth a connection slot.
                            if frame_error_strikes >= MAX_CONSECUTIVE_FRAME_ERRORS {
                                tracing::warn!(
                                    strikes = frame_error_strikes,
                                    "client sent {MAX_CONSECUTIVE_FRAME_ERRORS} consecutive \
                                     unusable frames, closing connection"
                                );
                                break;
                            }
                        }
                        ConnectionAction::Close => {
                            tracing::warn!(%e, "client read error, closing connection");
                            break;
                        }
                    },
                }
            }
            _ = shutdown_rx.changed() => break,
        }
    }

    connection_count.fetch_sub(1, Ordering::Relaxed);
}

/// Response id used when a framing error leaves the request id unknown.
/// Client request ids start at 1, so 0 can never collide with a live call.
const UNATTRIBUTED_ID: u64 = 0;

/// Recoverable frame errors a connection may hit in a row before it is
/// closed.
///
/// Each recoverable error is answered and the connection kept, which is what
/// makes one oversized or malformed request survivable. Unbounded, that same
/// tolerance is a free denial of service: an oversized frame costs up to
/// [`DRAIN_TIMEOUT`] of a connection slot, and there are only
/// [`MAX_CONNECTIONS`] of them. A well-behaved client never trips this — it
/// takes a *run* of bad frames with no good frame in between, and any good
/// frame resets the count.
const MAX_CONSECUTIVE_FRAME_ERRORS: u32 = 8;

/// What a connection should do after a [`FrameError`].
#[derive(Debug)]
enum ConnectionAction {
    /// Report the error to the client and keep serving.
    Reply(DaemonRpcError),
    /// Close the connection; the stream cannot be trusted.
    Close,
}

/// Classifies a framing error into a connection-level action.
///
/// Extracted as a free function so the policy is unit-testable without a
/// live `Recalld` instance.
fn action_for_frame_error(e: &FrameError) -> ConnectionAction {
    match e {
        FrameError::OversizedDiscarded { size, limit } => ConnectionAction::Reply(DaemonRpcError {
            code: ERR_TOO_LARGE,
            message: format!(
                "request is {size} bytes, exceeding the {limit} byte daemon message limit; \
                 the frame was discarded. Reduce fullText (max 1 MiB), shorten \
                 tags/entities/topics/emotions, or split this into multiple memories."
            ),
        }),
        FrameError::Malformed(detail) => ConnectionAction::Reply(DaemonRpcError {
            code: ERR_INVALID_INPUT,
            message: format!("malformed request frame: {detail}"),
        }),
        // `TooLarge` is a write-side error and never reaches this path, but
        // closing is the safe default if it ever does.
        FrameError::Closed
        | FrameError::Desync { .. }
        | FrameError::DrainTimeout { .. }
        | FrameError::PayloadTimeout { .. }
        | FrameError::Io(_)
        | FrameError::TooLarge { .. } => ConnectionAction::Close,
    }
}

/// Frame limit to use when writing to the peer that sent these `ping` params.
///
/// A protocol v1 client sends no version field; it is held at the legacy
/// 1 MiB limit because it rejects an oversized length prefix without
/// draining the payload, which would corrupt its stream permanently.
fn peer_frame_limit(params: &serde_json::Value) -> usize {
    let version = params
        .get("protocolVersion")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    if version < PROTOCOL_VERSION as u64 {
        return LEGACY_MAX_MESSAGE_SIZE;
    }
    let announced = params
        .get("maxMessageSize")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(LEGACY_MAX_MESSAGE_SIZE as u64);
    (announced as usize).clamp(LEGACY_MAX_MESSAGE_SIZE, MAX_MESSAGE_SIZE)
}

fn parse_memory_id(s: &str) -> Result<MemoryId, BridgeError> {
    let uuid = Uuid::parse_str(s)
        .map_err(|e| BridgeError::InvalidInput(format!("invalid memory ID: {e}")))?;
    Ok(MemoryId::from_uuid(uuid))
}

async fn dispatch(
    search: &dyn SearchPipeline,
    storage: &dyn BridgeStorageEngine,
    namespaces: &dyn NamespaceRegistry,
    health: &dyn HealthChecker,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BridgeError> {
    match method {
        // Doubles as the protocol handshake. A v1 client ignores the extra
        // fields; a v2 client uses them to size its outgoing frames.
        "ping" => Ok(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "maxMessageSize": MAX_MESSAGE_SIZE,
        })),

        "shutdown" => Ok(serde_json::json!({})),

        "search" => {
            let input: SearchInput = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let response = search.search(input).await?;
            serde_json::to_value(response).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "find_similar" => {
            let p: FindSimilarParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let id = parse_memory_id(&p.id)?;
            let results = search
                .find_similar(id, p.limit, p.min_score, p.same_namespace)
                .await?;
            serde_json::to_value(results).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "scan_duplicates" => {
            let p: ScanDuplicatesParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let results = search
                .scan_duplicates(&p.namespace, p.threshold, p.max_memories)
                .await?;
            serde_json::to_value(results).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "mutate_batch" => {
            let input: crate::storage::batch::BatchInput = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(format!("batch parameters: {e}")))?;
            // Shared preparation rejects invalid outer shape before reservation.
            let result = storage.mutate_batch(input).await?;
            serde_json::to_value(result).map_err(|e| {
                BridgeError::PartiallyApplied(format!("batch result serialization failed: {e}"))
            })
        }
        "store_memory" => {
            let input: StoreInput = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            // The in-process callers validate before they send, but the
            // socket is its own entry point: without this, an over-long
            // tag arriving here would be dropped by the storage layer
            // and the call would still report success.
            crate::model::validate_memory_input(crate::model::MemoryInputRef {
                summary: &input.summary,
                full_text: input.full_text.as_deref(),
                tags: &input.tags,
                entities: &input.entities,
                topics: &input.topics,
                emotions: &input.emotions,
            })
            .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let result = storage.store_memory(input).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "get_memory" => {
            let p: GetMemoryParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let id = parse_memory_id(&p.id)?;
            let result = storage.get_memory(id).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "delete_memory" => {
            let p: DeleteMemoryParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let id = parse_memory_id(&p.id)?;
            let result = storage.delete_memory(id).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "delete_memories" => {
            let p: DeleteMemoriesParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            // The socket is its own entry point: the tool layer's cap is
            // not on this path. Bound the ordered independent child writes too.
            if p.ids.len() > crate::model::constants::MAX_BATCH_MEMORIES {
                return Err(BridgeError::InvalidInput(format!(
                    "Too many ids ({}); the maximum is {} per call",
                    p.ids.len(),
                    crate::model::constants::MAX_BATCH_MEMORIES
                )));
            }
            let mut ids = Vec::with_capacity(p.ids.len());
            for id in &p.ids {
                ids.push(parse_memory_id(id)?);
            }
            let result = storage.delete_memories(&ids).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "reinforce_memory" => {
            let p: ReinforceParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let id = parse_memory_id(&p.id)?;
            let result = storage.reinforce_memory(id, p.quality).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "list_memories" => {
            let input: ListMemoriesInput = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let result = storage.list_memories(input).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "list_tags" => {
            let input: ListTagsInput = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let result = storage.list_tags(input).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "list_namespaces" => {
            let result = namespaces.list_namespaces().await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "create_namespace" => {
            let input: CreateNamespaceInput = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let result = namespaces.create_namespace(input).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "delete_namespace" => {
            let input: DeleteNamespaceInput = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let result = namespaces.delete_namespace(input).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "namespace_stats" => {
            let p: NamespaceStatsParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let result = namespaces.namespace_stats(&p.name).await?;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        "check_health" => {
            let result = health.check_health().await;
            serde_json::to_value(result).map_err(|e| BridgeError::Internal(e.to_string()))
        }

        other => Err(BridgeError::InvalidInput(format!(
            "unknown method: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::bridge::{
        DeleteNamespaceResult, DuplicateCluster, HealthStatus, ListMemoriesResponse,
        ListTagsResponse, MemoryRecord, NamespaceInfo, NamespaceStats, ReinforceResult, SearchHit,
        SearchResponse, StoredMemory,
    };

    /// Stub subsystems for the dispatch tests. Only the unknown-method path
    /// is exercised, which never reaches any of these.
    struct Unused;

    #[tokio::test]
    async fn actual_batch_socket_roundtrip_preserves_partial_outcome_and_rejects_foreign_resume() {
        use crate::storage::batch::{BatchInput, BatchKind, BatchOutcome, BatchStatus};
        let fx = crate::test_support::Fixture::new();
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (tx, rx) = watch::channel(false);
        let count = Arc::new(AtomicU32::new(1));
        let task = tokio::spawn(handle_connection(
            Arc::new(Unused),
            Arc::new(fx.storage_adapter()),
            Arc::new(Unused),
            Arc::new(Unused),
            server,
            count.clone(),
            Arc::new(AtomicI64::new(0)),
            rx,
            tx.clone(),
        ));
        let mut input = BatchInput {
            result_version: 1,
            kind: BatchKind::Insert,
            items: vec![
                serde_json::json!(false),
                serde_json::json!({"summary":"socket batch","embedding":[1.,0.,0.,0.],"checkDuplicates":false}),
            ],
            default_namespace: "default".into(),
            store_id: None,
            idempotency_key: None,
            batch_operation_id: None,
        };
        let request = DaemonRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "mutate_batch".into(),
            params: serde_json::to_value(&input).unwrap(),
        };
        write_framed_request(&mut client, &request, LEGACY_MAX_MESSAGE_SIZE)
            .await
            .unwrap();
        let response = read_framed_response(&mut client, LEGACY_MAX_MESSAGE_SIZE)
            .await
            .unwrap();
        assert!(response.error.is_none());
        let outcome: BatchOutcome = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(
            (outcome.succeeded, outcome.failed, outcome.unresolved),
            (1, 1, 0)
        );
        assert_eq!(outcome.items[1].status, BatchStatus::Ok);
        assert!(outcome.items[1].receipt.is_some());
        input.batch_operation_id = Some(outcome.batch_operation_id);
        input.store_id = Some(Uuid::new_v4());
        let before = fx.storage.read().unwrap().control_state().unwrap();
        let request = DaemonRequest {
            id: 2,
            params: serde_json::to_value(input).unwrap(),
            ..request
        };
        write_framed_request(&mut client, &request, LEGACY_MAX_MESSAGE_SIZE)
            .await
            .unwrap();
        let rejected = read_framed_response(&mut client, LEGACY_MAX_MESSAGE_SIZE)
            .await
            .unwrap();
        assert!(rejected.result.is_none());
        assert!(rejected.error.is_some());
        assert_eq!(fx.storage.read().unwrap().control_state().unwrap(), before);
        tx.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn actual_batch_rpc_resume_only_loads_existing_parent_and_rejects_changed_identity_without_write()
     {
        use crate::storage::batch::{BatchInput, BatchKind, BatchOutcome};
        let fx = crate::test_support::Fixture::new();
        let adapter = fx.storage_adapter();
        let store_id = fx.storage.read().unwrap().control_state().unwrap().store_id;
        let mut input = BatchInput {
            result_version: 1,
            kind: BatchKind::Insert,
            items: vec![
                serde_json::json!({"summary":"RPC batch","embedding":[1.,0.,0.,0.],"checkDuplicates":false}),
            ],
            default_namespace: "default".into(),
            store_id: None,
            idempotency_key: None,
            batch_operation_id: None,
        };
        let outcome: BatchOutcome = serde_json::from_value(
            dispatch(
                &Unused,
                &adapter,
                &Unused,
                &Unused,
                "mutate_batch",
                serde_json::to_value(&input).unwrap(),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert!(outcome.all_ready());
        assert_ne!(
            Some(outcome.batch_operation_id),
            outcome.items[0].operation_id
        );
        input.store_id = Some(store_id);
        input.batch_operation_id = Some(outcome.batch_operation_id);
        let before = fx.storage.read().unwrap().control_state().unwrap();
        let replay: BatchOutcome = serde_json::from_value(
            dispatch(
                &Unused,
                &adapter,
                &Unused,
                &Unused,
                "mutate_batch",
                serde_json::to_value(&input).unwrap(),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert!(replay.reservation_receipt.unwrap().replayed);
        assert_eq!(
            replay.items[0].receipt.as_ref().unwrap().operation_id,
            outcome.items[0].receipt.as_ref().unwrap().operation_id
        );
        for invalid in 0..5 {
            let mut retry = input.clone();
            match invalid {
                0 => retry.store_id = Some(uuid::Uuid::new_v4()),
                1 => retry.batch_operation_id = Some(uuid::Uuid::new_v4()),
                2 => retry.items[0]["summary"] = serde_json::json!("changed"),
                3 => retry.idempotency_key = Some("new key".into()),
                _ => retry.batch_operation_id = outcome.items[0].operation_id,
            }
            assert!(
                dispatch(
                    &Unused,
                    &adapter,
                    &Unused,
                    &Unused,
                    "mutate_batch",
                    serde_json::to_value(retry).unwrap()
                )
                .await
                .is_err()
            );
            assert_eq!(fx.storage.read().unwrap().control_state().unwrap(), before);
        }
    }

    #[tokio::test]
    async fn actual_batch_rpc_keeps_reservation_and_current_unknown_child_separate() {
        use crate::storage::batch::{BatchInput, BatchKind, BatchOutcome, BatchStatus};
        let fx = crate::test_support::Fixture::new();
        let adapter = fx.storage_adapter();
        fx.storage
            .read()
            .unwrap()
            .meta_store()
            .fail_batch_commit
            .store(3, std::sync::atomic::Ordering::SeqCst);
        let input = BatchInput {
            result_version: 1,
            kind: BatchKind::Insert,
            items: vec![
                serde_json::json!({"summary":"RPC unknown","embedding":[1.,0.,0.,0.],"checkDuplicates":false}),
                serde_json::json!({"summary":"later","embedding":[1.,0.,0.,0.]}),
            ],
            default_namespace: "default".into(),
            store_id: None,
            idempotency_key: None,
            batch_operation_id: None,
        };
        let value = dispatch(
            &Unused,
            &adapter,
            &Unused,
            &Unused,
            "mutate_batch",
            serde_json::to_value(input).unwrap(),
        )
        .await
        .unwrap();
        let outcome: BatchOutcome = serde_json::from_value(value).unwrap();
        assert!(outcome.reservation_receipt.is_some());
        assert_eq!(outcome.items[0].status, BatchStatus::Unknown);
        assert_ne!(
            outcome.items[0].operation_id,
            Some(outcome.batch_operation_id)
        );
        assert_eq!(outcome.items[1].status, BatchStatus::NotAttempted);
        assert_eq!(
            outcome.total,
            outcome.succeeded + outcome.failed + outcome.unresolved
        );
    }

    #[tokio::test]
    async fn actual_daemon_insertion_dispatch_keeps_committed_failure_distinct_from_success() {
        use crate::storage::StorageEngine as _;
        let fx = crate::test_support::Fixture::new();
        *fx.vector_index.write().await =
            crate::search::FlatVectorIndex::new(crate::test_support::DIM + 1);
        let error = dispatch(
            &Unused,
            &fx.storage_adapter(),
            &Unused,
            &Unused,
            "store_memory",
            serde_json::json!({
                "summary":"daemon committed insertion", "namespace":"default", "tags":[],
                "embedding":[1.,0.,0.,0.], "checkDuplicates":false
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, BridgeError::PartiallyApplied(_)), "{error}");
        let message = error.to_string();
        assert!(message.contains("commitState=committed"));
        assert!(message.contains("ready=false"));
        let pending = fx
            .storage
            .read()
            .unwrap()
            .meta_store()
            .pending_effects()
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert!(message.contains(&pending[0].receipt.operation_id.to_string()));
        assert_eq!(fx.storage.read().unwrap().scan_all().unwrap().len(), 1);
        assert!(
            fx.storage
                .read()
                .unwrap()
                .meta_store()
                .effects_stamp()
                .is_err()
        );
    }

    #[async_trait::async_trait]
    impl SearchPipeline for Unused {
        async fn search(&self, _: SearchInput) -> Result<SearchResponse, BridgeError> {
            unreachable!()
        }
        async fn find_similar(
            &self,
            _: MemoryId,
            _: usize,
            _: Option<f32>,
            _: bool,
        ) -> Result<Vec<SearchHit>, BridgeError> {
            unreachable!()
        }
        async fn scan_duplicates(
            &self,
            _: &str,
            _: f32,
            _: usize,
        ) -> Result<Vec<DuplicateCluster>, BridgeError> {
            unreachable!()
        }
    }

    #[async_trait::async_trait]
    impl BridgeStorageEngine for Unused {
        async fn store_memory(&self, _: StoreInput) -> Result<StoredMemory, BridgeError> {
            unreachable!()
        }
        async fn get_memory(&self, _: MemoryId) -> Result<Option<MemoryRecord>, BridgeError> {
            unreachable!()
        }
        async fn delete_memory(&self, _: MemoryId) -> Result<bool, BridgeError> {
            unreachable!()
        }
        async fn delete_memories(&self, _: &[MemoryId]) -> Result<Vec<bool>, BridgeError> {
            unreachable!()
        }
        async fn reinforce_memory(
            &self,
            _: MemoryId,
            _: u8,
        ) -> Result<ReinforceResult, BridgeError> {
            unreachable!()
        }
        async fn list_memories(
            &self,
            _: ListMemoriesInput,
        ) -> Result<ListMemoriesResponse, BridgeError> {
            unreachable!()
        }
        async fn list_tags(&self, _: ListTagsInput) -> Result<ListTagsResponse, BridgeError> {
            unreachable!()
        }
    }

    #[async_trait::async_trait]
    impl NamespaceRegistry for Unused {
        async fn list_namespaces(&self) -> Result<Vec<NamespaceInfo>, BridgeError> {
            unreachable!()
        }
        async fn create_namespace(
            &self,
            _: CreateNamespaceInput,
        ) -> Result<NamespaceInfo, BridgeError> {
            unreachable!()
        }
        async fn namespace_stats(&self, _: &str) -> Result<NamespaceStats, BridgeError> {
            unreachable!()
        }
        async fn delete_namespace(
            &self,
            _: DeleteNamespaceInput,
        ) -> Result<DeleteNamespaceResult, BridgeError> {
            unreachable!()
        }
    }

    #[async_trait::async_trait]
    impl HealthChecker for Unused {
        async fn check_health(&self) -> HealthStatus {
            unreachable!()
        }
    }

    async fn dispatch_stub(
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        dispatch(&Unused, &Unused, &Unused, &Unused, method, params).await
    }

    /// Drives a real [`handle_connection`] over a socket pair, so the
    /// connection-level policies (strikes, replies, hangup) are exercised
    /// end to end rather than asserted in prose.
    fn spawn_connection() -> (tokio::net::UnixStream, Arc<AtomicU32>) {
        let (client, server) = tokio::net::UnixStream::pair().expect("socket pair");
        let count = Arc::new(AtomicU32::new(1));
        let (tx, rx) = watch::channel(false);
        tokio::spawn(handle_connection(
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(Unused),
            server,
            count.clone(),
            Arc::new(AtomicI64::new(lifecycle::now_millis())),
            rx,
            tx,
        ));
        (client, count)
    }

    fn ping_request(id: u64) -> DaemonRequest {
        DaemonRequest {
            jsonrpc: "2.0".into(),
            id,
            method: "ping".into(),
            params: serde_json::json!({}),
        }
    }

    /// Sends one unparseable frame and returns the daemon's reply.
    async fn send_junk_frame<W, R>(writer: &mut W, reader: &mut R) -> DaemonResponse
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncWriteExt;
        writer.write_u32(4).await.expect("write length");
        writer.write_all(b"junk").await.expect("write payload");
        read_framed_response(reader, MAX_MESSAGE_SIZE)
            .await
            .expect("the daemon answers a recoverable frame error")
    }

    // 25 — regression: an endless stream of unusable frames must not hold a
    // connection slot forever.
    #[tokio::test]
    async fn a_run_of_unusable_frames_closes_the_connection() {
        let (client, count) = spawn_connection();
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);

        for strike in 1..=MAX_CONSECUTIVE_FRAME_ERRORS {
            let resp = send_junk_frame(&mut writer, &mut reader).await;
            assert_eq!(resp.id, UNATTRIBUTED_ID, "strike {strike}");
            assert_eq!(
                resp.error.expect("error payload").code,
                ERR_INVALID_INPUT,
                "strike {strike}"
            );
        }

        // The last strike was still answered, and then the daemon hung up.
        // Bounded so an unbounded-tolerance regression fails instead of
        // hanging the suite.
        let after = tokio::time::timeout(
            Duration::from_secs(5),
            read_framed_response(&mut reader, MAX_MESSAGE_SIZE),
        )
        .await
        .expect("the daemon must close the connection, not wait for more garbage");
        assert!(
            matches!(after, Err(FrameError::Closed)),
            "expected the connection to be closed, got {after:?}"
        );
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "the connection slot must be released"
        );
    }

    // 25b — the limit counts *consecutive* errors: any frame that parses
    // clears the record, so a long-lived client that occasionally overruns
    // the frame limit is never penalized for it.
    #[tokio::test]
    async fn a_good_frame_resets_the_strike_count() {
        let (client, _count) = spawn_connection();
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);

        for _ in 0..MAX_CONSECUTIVE_FRAME_ERRORS - 1 {
            send_junk_frame(&mut writer, &mut reader).await;
        }

        write_framed_request(&mut writer, &ping_request(1), MAX_MESSAGE_SIZE)
            .await
            .expect("write ping");
        let ok = read_framed_response(&mut reader, MAX_MESSAGE_SIZE)
            .await
            .expect("ping is answered");
        assert_eq!(ok.id, 1);

        // A second run of the same length is tolerated, which it would not
        // be if the counter were cumulative.
        for _ in 0..MAX_CONSECUTIVE_FRAME_ERRORS - 1 {
            send_junk_frame(&mut writer, &mut reader).await;
        }
        write_framed_request(&mut writer, &ping_request(2), MAX_MESSAGE_SIZE)
            .await
            .expect("write second ping");
        let ok = read_framed_response(&mut reader, MAX_MESSAGE_SIZE)
            .await
            .expect("the connection is still alive");
        assert_eq!(ok.id, 2);
    }

    // 23
    #[test]
    fn action_for_frame_error_classification() {
        let recoverable = [
            (
                FrameError::OversizedDiscarded {
                    size: 9_000_000,
                    limit: 1_048_576,
                },
                ERR_TOO_LARGE,
            ),
            (
                FrameError::Malformed("expected value".into()),
                ERR_INVALID_INPUT,
            ),
        ];
        for (error, code) in recoverable {
            match action_for_frame_error(&error) {
                ConnectionAction::Reply(rpc) => {
                    assert_eq!(rpc.code, code, "for {error}");
                    assert!(!rpc.message.is_empty());
                }
                ConnectionAction::Close => panic!("{error} must not close the connection"),
            }
            assert!(error.is_recoverable(), "{error}");
        }

        let fatal = [
            FrameError::Closed,
            FrameError::Desync {
                size: u32::MAX as usize,
                limit: MAX_DRAIN_SIZE,
            },
            FrameError::DrainTimeout { size: 9_000_000 },
            FrameError::PayloadTimeout { size: 9_000_000 },
            FrameError::Io(std::io::Error::other("boom")),
            FrameError::TooLarge {
                size: 9_000_000,
                limit: 1_048_576,
            },
        ];
        for error in fatal {
            assert!(
                matches!(action_for_frame_error(&error), ConnectionAction::Close),
                "{error} must close the connection"
            );
            assert!(!error.is_recoverable(), "{error}");
        }
    }

    // 23b — the oversized-request reply must name the limit so the caller can act.
    #[test]
    fn oversized_request_reply_explains_the_limit() {
        let action = action_for_frame_error(&FrameError::OversizedDiscarded {
            size: 9_000_000,
            limit: 8_388_608,
        });
        let ConnectionAction::Reply(rpc) = action else {
            panic!("expected a reply");
        };
        assert!(rpc.message.contains("9000000"), "{}", rpc.message);
        assert!(rpc.message.contains("8388608"), "{}", rpc.message);
    }

    // 24
    /// The tool layer caps a `forget_memories` batch at
    /// `MAX_BATCH_MEMORIES`, but the socket is its own entry point and
    /// never runs the tool handler. Without this the frame goes straight
    /// to one write transaction over an unbounded id list — and the stub
    /// `delete_memories` is `unreachable!()`, so reaching it fails the
    /// test rather than passing it quietly.
    #[tokio::test]
    async fn w70_delete_memories_caps_the_batch_at_the_shared_constant() {
        let over_cap: Vec<String> = (0..crate::model::constants::MAX_BATCH_MEMORIES + 1)
            .map(|_| crate::model::MemoryId::new().to_string())
            .collect();

        let err = dispatch_stub("delete_memories", serde_json::json!({ "ids": over_cap }))
            .await
            .expect_err("an over-cap batch must be refused");

        assert!(
            matches!(err, BridgeError::InvalidInput(ref m) if m.contains("Too many ids")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_method_returns_invalid_input() {
        let err = dispatch_stub("no_such_method", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, BridgeError::InvalidInput(_)), "{err}");
        assert!(err.to_string().contains("no_such_method"), "{err}");
    }

    /// `list_tags` params have no serde default for `limit`, so an empty
    /// object is rejected at decode time -- which is also why the `Unused`
    /// stub's `list_tags` is never reached.
    #[tokio::test]
    async fn dispatch_list_tags_rejects_params_it_cannot_decode() {
        let err = dispatch_stub("list_tags", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, BridgeError::InvalidInput(_)), "{err}");
    }

    #[tokio::test]
    async fn dispatch_ping_announces_the_protocol_version() {
        let result = dispatch_stub("ping", serde_json::json!({})).await.unwrap();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["maxMessageSize"], MAX_MESSAGE_SIZE);
    }

    #[test]
    fn peer_frame_limit_holds_unannounced_peers_at_the_legacy_limit() {
        // A protocol v1 client sends no version field.
        assert_eq!(
            peer_frame_limit(&serde_json::json!({})),
            LEGACY_MAX_MESSAGE_SIZE
        );
        assert_eq!(
            peer_frame_limit(&serde_json::Value::Null),
            LEGACY_MAX_MESSAGE_SIZE
        );
        assert_eq!(
            peer_frame_limit(&serde_json::json!({ "protocolVersion": 2 })),
            LEGACY_MAX_MESSAGE_SIZE
        );
        assert_eq!(
            peer_frame_limit(
                &serde_json::json!({ "protocolVersion": 2, "maxMessageSize": MAX_MESSAGE_SIZE })
            ),
            MAX_MESSAGE_SIZE
        );
        // An absurd announcement is clamped rather than trusted.
        assert_eq!(
            peer_frame_limit(
                &serde_json::json!({ "protocolVersion": 2, "maxMessageSize": 1_u64 << 40 })
            ),
            MAX_MESSAGE_SIZE
        );
    }
}
