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
    BridgeError, CreateNamespaceInput, HealthChecker, NamespaceRegistry, SearchInput,
    SearchPipeline, StorageEngine as BridgeStorageEngine, StoreInput,
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
        let storage: Arc<dyn BridgeStorageEngine> = Arc::new(McpStorageAdapter::new(
            system.storage().clone(),
            system.cache().clone(),
            system.embedding().clone(),
            system.vector_index().clone(),
            system.fts_index().clone(),
            system.entity_index().clone(),
            system.graph().clone(),
            std::sync::Arc::new(system.config().clone()),
            tz,
        ));
        let namespaces: Arc<dyn NamespaceRegistry> =
            Arc::new(McpNamespaceAdapter::new(system.storage().clone(), tz));
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

    loop {
        tokio::select! {
            msg = read_framed_message(&mut reader) => {
                match msg {
                    Ok(None) => break,
                    Ok(Some(request)) => {
                        let method = request.method.clone();

                        let response = dispatch(
                            &*search, &*storage, &*namespaces, &*health,
                            &request.method, request.params,
                        ).await;

                        let resp = match response {
                            Ok(result) => DaemonResponse::success(request.id, result),
                            Err(e) => DaemonResponse::error(request.id, DaemonRpcError::from(&e)),
                        };

                        if write_framed_message(&mut writer, &resp).await.is_err() {
                            break;
                        }

                        if !matches!(method.as_str(), "ping" | "check_health" | "shutdown") {
                            last_activity.store(lifecycle::now_millis(), Ordering::Relaxed);
                        }

                        if method == "shutdown" {
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%e, "client read error");
                        break;
                    }
                }
            }
            _ = shutdown_rx.changed() => break,
        }
    }

    connection_count.fetch_sub(1, Ordering::Relaxed);
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
        "ping" => Ok(serde_json::json!({})),

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

        "store_memory" => {
            let input: StoreInput = serde_json::from_value(params)
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

        "reinforce_memory" => {
            let p: ReinforceParams = serde_json::from_value(params)
                .map_err(|e| BridgeError::InvalidInput(e.to_string()))?;
            let id = parse_memory_id(&p.id)?;
            let result = storage.reinforce_memory(id, p.quality).await?;
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
