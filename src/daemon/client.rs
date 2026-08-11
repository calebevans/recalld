use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

use super::protocol::{
    DaemonRequest, FrameError, LEGACY_MAX_MESSAGE_SIZE, MAX_MESSAGE_SIZE, PROTOCOL_VERSION,
    read_framed_response, write_framed_request,
};
use crate::mcp::bridge::BridgeError;

/// Connect attempts made before a reconnect is declared hopeless.
const RECONNECT_ATTEMPTS: usize = 3;
/// Backoff between reconnect attempts. The final entry is never slept on.
const RECONNECT_BACKOFF_MS: [u64; RECONNECT_ATTEMPTS] = [50, 200, 500];
/// How many stale (already-abandoned) responses may be skipped before the
/// stream is treated as corrupt.
const MAX_STALE_SKIPS: usize = 8;

/// Whether a method may be replayed on a fresh connection after the original
/// attempt failed with an unknown outcome.
///
/// The bar is **no observable state change of any kind**, not "not a write".
/// Access and decay bookkeeping counts: recording an access advances
/// `last_accessed_at` and appends an `AccessEvent` that feeds the FSRS
/// schedule, so a transparent replay would silently age a memory twice off
/// one caller request. That is what keeps `search` and `find_similar` off
/// this list even though neither creates or edits a memory — both record
/// accesses for every result they return (`SearchPipeline::search`,
/// `SearchPipeline::similar`).
///
/// The methods that remain are pure reads of storage, the namespace table,
/// or process health. `get_memory` qualifies because the *daemon* path
/// (`McpStorageAdapter::get_memory`) reads the record straight out of
/// storage and records nothing; the access-recording `get_memory` lives on
/// the in-process query engine, which the socket never reaches. Should the
/// adapter ever start recording accesses, it has to come off this list.
///
/// Mutating methods are excluded for the usual reasons: they mint new
/// server-side state (`store_memory` allocates a fresh id, so a replay
/// silently creates a duplicate the caller can never reconcile), advance
/// state on each call (`reinforce_memory` moves the FSRS schedule), or have
/// a return value that is not idempotent even where the effect is
/// (`delete_memory` returns whether the memory existed).
fn is_retry_safe(method: &str) -> bool {
    matches!(
        method,
        "ping"
            | "check_health"
            | "scan_duplicates"
            | "get_memory"
            | "list_namespaces"
            | "namespace_stats"
            | "list_memories"
    )
}

/// Whether a method is barred from the automatic replay *only* because it
/// records accesses.
///
/// These write no memory content, so the honest report to the caller is
/// "ask again", not the "may or may not have been applied" warning that a
/// half-applied mutation deserves.
fn records_accesses_only(method: &str) -> bool {
    matches!(method, "search" | "find_similar")
}

/// An established socket connection to the daemon.
struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }
}

/// Mutable client state, serialized by the client's mutex.
struct ClientState {
    /// `None` once the connection has been poisoned; re-established lazily.
    conn: Option<Connection>,
    /// Monotonic *across reconnects*, so a frame left over from a dropped
    /// connection can never alias the id of a live call.
    next_id: u64,
    /// Largest frame the daemon is known to accept. Held at the protocol v1
    /// limit until a `ping` negotiates something larger.
    frame_limit: usize,
}

/// A failed exchange, plus whether the connection survived it.
struct ExchangeError {
    error: BridgeError,
    /// The connection was poisoned and must be re-established. When set, the
    /// request may or may not have been applied by the daemon: neither a
    /// failed write nor a failed read proves the daemon did not act.
    lost: bool,
}

impl ExchangeError {
    /// The call failed but the connection is still usable and the outcome is
    /// known (nothing was sent, or the daemon answered definitively).
    fn settled(error: BridgeError) -> Self {
        Self { error, lost: false }
    }

    /// The connection was poisoned; the outcome of the call is unknown.
    fn lost(error: BridgeError) -> Self {
        Self { error, lost: true }
    }
}

/// Client for communicating with the Recalld daemon over a Unix socket.
///
/// The connection is self-healing: a transport failure poisons it, and the
/// next call transparently re-establishes it. An oversized request is
/// rejected before a single byte reaches the socket, so it cannot corrupt
/// the stream for subsequent calls.
pub struct DaemonClient {
    socket_path: PathBuf,
    state: Mutex<ClientState>,
    /// Mirrors `state.conn.is_some()` so [`DaemonClient::is_connected`] can
    /// stay synchronous.
    connected: AtomicBool,
}

impl DaemonClient {
    /// Connects to the daemon at the given Unix socket path.
    ///
    /// The frame limit starts at the conservative protocol v1 value; call
    /// [`DaemonClient::ping`] to negotiate a larger one.
    pub async fn connect(socket_path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            state: Mutex::new(ClientState {
                conn: Some(Connection::new(stream)),
                next_id: 1,
                frame_limit: LEGACY_MAX_MESSAGE_SIZE,
            }),
            connected: AtomicBool::new(true),
        })
    }

    /// Whether the client currently holds a live connection.
    ///
    /// `false` only means the next call will reconnect first, not that the
    /// daemon is gone.
    pub fn is_connected(&self) -> bool {
        self.connected.load(AtomicOrdering::Relaxed)
    }

    /// Sends a request and awaits its response.
    ///
    /// The mutex is held for the whole exchange, so calls are serialized on
    /// the connection.
    ///
    /// **Not cancel-safe.** Dropping the returned future between the write
    /// and the read abandons a response that a later call must then skip;
    /// the client tolerates a bounded number of such strays but does not
    /// treat cancellation as a supported pattern.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        let mut state = self.state.lock().await;
        let outcome = self.call_locked(&mut state, method, params).await;
        self.connected
            .store(state.conn.is_some(), AtomicOrdering::Relaxed);
        outcome
    }

    async fn call_locked(
        &self,
        state: &mut ClientState,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        if state.conn.is_none() {
            self.reconnect(state).await?;
        }

        let mut request = DaemonRequest {
            jsonrpc: "2.0".into(),
            id: 0,
            method: method.into(),
            params,
        };

        let failure = match state.exchange(&mut request).await {
            Ok(result) => return Ok(result),
            Err(e) => e,
        };

        // The connection is intact: the answer, good or bad, is final.
        if !failure.lost {
            return Err(failure.error);
        }

        let reconnected = self.reconnect(state).await;

        if !is_retry_safe(method) {
            let mut message = format!(
                "daemon connection lost while awaiting the response to `{method}` ({}); ",
                failure.error
            );
            // A read that is withheld from the automatic replay only because
            // it records accesses must not be described as a write that may
            // have half-happened: the caller can simply ask again.
            if records_accesses_only(method) {
                message.push_str(
                    "no memory was created, changed, or deleted, and reissuing the call is safe.",
                );
                if reconnected.is_ok() {
                    message.push_str(" The connection has been re-established.");
                } else {
                    message.push_str(" The daemon is currently unreachable.");
                }
            } else {
                message.push_str("the operation may or may not have been applied.");
                if reconnected.is_ok() {
                    message.push_str(
                        " The connection has been re-established - verify with recall_memories \
                         before retrying.",
                    );
                } else {
                    message.push_str(
                        " The daemon is currently unreachable - verify with recall_memories \
                         once it is back before retrying.",
                    );
                }
            }
            return Err(BridgeError::Internal(message));
        }

        reconnected?;
        state.exchange(&mut request).await.map_err(|e| e.error)
    }

    /// Sends a ping to verify the daemon is responsive and negotiate the
    /// frame limit for this connection.
    ///
    /// The mutex is held across the whole exchange *and* the write that
    /// records the answer. Releasing it in between would make this a
    /// read-modify-write on shared state: a reconnect landing in the gap
    /// negotiates its own (possibly legacy 1 MiB) limit, and this method
    /// would then stamp a stale 8 MiB over it. The client would go on to
    /// send frames a v1 daemon rejects *without draining*, permanently
    /// desynchronizing the stream — the precise failure the negotiation
    /// exists to prevent.
    pub async fn ping(&self) -> Result<(), BridgeError> {
        let mut state = self.state.lock().await;
        let outcome = self
            .call_locked(&mut state, "ping", handshake_params())
            .await;
        self.connected
            .store(state.conn.is_some(), AtomicOrdering::Relaxed);
        // The answer describes the connection the lock is still protecting.
        state.frame_limit = negotiated_limit(&outcome?);
        Ok(())
    }

    /// Re-establishes the connection, negotiating the frame limit on the way.
    ///
    /// Bounded: at most [`RECONNECT_ATTEMPTS`] attempts with a fixed backoff,
    /// so this can never spin.
    async fn reconnect(&self, state: &mut ClientState) -> Result<(), BridgeError> {
        state.conn = None;
        state.frame_limit = LEGACY_MAX_MESSAGE_SIZE;
        let mut last: Option<String> = None;

        for (attempt, backoff_ms) in RECONNECT_BACKOFF_MS.iter().enumerate() {
            match UnixStream::connect(&self.socket_path).await {
                Ok(stream) => {
                    state.conn = Some(Connection::new(stream));
                    state.frame_limit = LEGACY_MAX_MESSAGE_SIZE;
                    match state.negotiate().await {
                        Ok(()) => return Ok(()),
                        Err(e) => {
                            state.conn = None;
                            last = Some(e.to_string());
                        }
                    }
                }
                Err(e) => last = Some(e.to_string()),
            }

            // No point sleeping after the final attempt.
            if attempt + 1 < RECONNECT_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(*backoff_ms)).await;
            }
        }

        Err(BridgeError::Internal(format!(
            "daemon unavailable at {} after {RECONNECT_ATTEMPTS} attempts: {}",
            self.socket_path.display(),
            last.unwrap_or_else(|| "unknown error".into())
        )))
    }

    /// Test hook: the frame limit currently in force.
    #[cfg(test)]
    async fn frame_limit(&self) -> usize {
        self.state.lock().await.frame_limit
    }
}

impl ClientState {
    /// Performs the handshake on a freshly established connection.
    ///
    /// Never recurses into reconnect: it drives [`ClientState::exchange`]
    /// directly.
    async fn negotiate(&mut self) -> Result<(), BridgeError> {
        let mut request = DaemonRequest {
            jsonrpc: "2.0".into(),
            id: 0,
            method: "ping".into(),
            params: handshake_params(),
        };
        let result = self.exchange(&mut request).await.map_err(|e| e.error)?;
        self.frame_limit = negotiated_limit(&result);
        Ok(())
    }

    /// One request/response round trip on the current connection.
    ///
    /// Assigns `request.id` so a retry gets a fresh id while reusing the
    /// (potentially large) params without cloning them.
    async fn exchange(
        &mut self,
        request: &mut DaemonRequest,
    ) -> Result<serde_json::Value, ExchangeError> {
        let method = request.method.clone();
        let limit = self.frame_limit;
        request.id = self.next_id;
        self.next_id += 1;
        let id = request.id;

        let written = match self.conn.as_mut() {
            Some(conn) => write_framed_request(&mut conn.writer, request, limit).await,
            None => {
                return Err(ExchangeError::lost(BridgeError::Internal(
                    "daemon connection is not established".into(),
                )));
            }
        };

        if let Err(e) = written {
            return Err(match e {
                // Rejected before anything reached the socket: the connection
                // is untouched and stays usable. This is what keeps one
                // oversized request from breaking every call after it.
                FrameError::TooLarge { size, limit } => {
                    ExchangeError::settled(BridgeError::too_large(size, limit))
                }
                FrameError::Malformed(detail) => ExchangeError::settled(BridgeError::InvalidInput(
                    format!("could not serialize `{method}` request: {detail}"),
                )),
                other => {
                    self.conn = None;
                    ExchangeError::lost(BridgeError::Internal(format!(
                        "daemon write error on `{method}`: {other}"
                    )))
                }
            });
        }

        for _ in 0..=MAX_STALE_SKIPS {
            let read = match self.conn.as_mut() {
                Some(conn) => read_framed_response(&mut conn.reader, limit).await,
                None => {
                    return Err(ExchangeError::lost(BridgeError::Internal(
                        "daemon connection is not established".into(),
                    )));
                }
            };

            let response = match read {
                Ok(response) => response,
                // Both arms below leave the stream on a frame boundary, so
                // only this call is lost - but they are different failures
                // and must not be reported as the same one. `TooLarge` sends
                // the caller off to shrink their input, which is exactly the
                // wrong move when the daemon simply emitted a frame we could
                // not parse.
                Err(e @ FrameError::OversizedDiscarded { .. }) => {
                    return Err(ExchangeError::settled(BridgeError::TooLarge(format!(
                        "the daemon's response to `{method}` could not be delivered: {e}"
                    ))));
                }
                Err(FrameError::Malformed(detail)) => {
                    return Err(ExchangeError::settled(BridgeError::Internal(format!(
                        "the daemon's response to `{method}` could not be parsed: {detail}; \
                         the connection is still usable and the call can be retried"
                    ))));
                }
                Err(e) => {
                    self.conn = None;
                    return Err(ExchangeError::lost(BridgeError::Internal(format!(
                        "daemon connection lost while awaiting the response to `{method}`: {e}"
                    ))));
                }
            };

            // The daemon uses id 0 when a framing error hid the request id.
            if response.id == 0 {
                let error = response
                    .error
                    .map(|e| e.into_bridge_error())
                    .unwrap_or_else(|| {
                        BridgeError::Internal(
                            "daemon reported an unattributable protocol error".into(),
                        )
                    });
                return Err(ExchangeError::settled(error));
            }

            match response.id.cmp(&id) {
                Ordering::Equal => {
                    if let Some(err) = response.error {
                        return Err(ExchangeError::settled(err.into_bridge_error()));
                    }
                    return match response.result {
                        Some(result) => Ok(result),
                        None => Err(ExchangeError::settled(BridgeError::Internal(
                            "empty daemon response".into(),
                        ))),
                    };
                }
                // Ids are monotonic, so a lower id belongs to a call that was
                // already abandoned. Provably not ours; discard it.
                Ordering::Less => {
                    tracing::debug!(
                        got = response.id,
                        expected = id,
                        "discarding stale daemon response"
                    );
                }
                // Impossible under monotonic ids: the stream is corrupt.
                Ordering::Greater => {
                    self.conn = None;
                    return Err(ExchangeError::lost(BridgeError::Internal(format!(
                        "daemon response id mismatch: expected {id}, got {}; \
                         connection reset to resynchronize",
                        response.id
                    ))));
                }
            }
        }

        self.conn = None;
        Err(ExchangeError::lost(BridgeError::Internal(format!(
            "daemon sent more than {MAX_STALE_SKIPS} stale responses while awaiting \
             `{method}`; connection reset to resynchronize"
        ))))
    }
}

/// Params announcing this build's protocol capabilities on `ping`.
fn handshake_params() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "maxMessageSize": MAX_MESSAGE_SIZE,
    })
}

/// Frame limit implied by a `ping` result.
///
/// A daemon that says nothing is protocol v1 and is held at 1 MiB, which
/// keeps us from ever handing it a frame it would reject without draining.
fn negotiated_limit(result: &serde_json::Value) -> usize {
    let version = result
        .get("protocolVersion")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    if version < PROTOCOL_VERSION as u64 {
        return LEGACY_MAX_MESSAGE_SIZE;
    }
    let announced = result
        .get("maxMessageSize")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(LEGACY_MAX_MESSAGE_SIZE as u64);
    (announced as usize).clamp(LEGACY_MAX_MESSAGE_SIZE, MAX_MESSAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;
    use tokio::net::UnixListener;

    use super::super::protocol::{DaemonResponse, read_framed_message, write_framed_message};
    use super::*;

    /// Scripted daemon behaviours used to drive the client through each
    /// failure mode. All counters below are global across connections, so a
    /// behaviour that fires "on the first call" does not fire again on the
    /// connection the client reconnects with.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Behavior {
        /// Answer every call successfully; negotiate protocol v2 on ping.
        Healthy,
        /// Answer ping with `{}`, as recalld <= 0.1.10 does.
        LegacyPing,
        /// Answer the first call with a response larger than the client's limit.
        HugeResponse,
        /// Read the first call, then hang up without replying.
        CloseAfterFirstCall,
        /// Prefix the second call's response with a stale one.
        StaleThenCorrect,
        /// Answer the first call with an id from the future.
        FutureId,
        /// Answer the first call with a well-framed but unparseable payload.
        MalformedResponse,
        /// Answer the first connection's `ping` slowly and hang up on its
        /// first real call; negotiate as a v1 daemon on every connection
        /// after that. Drives the ping/reconnect interleaving.
        SlowPingThenLegacyReconnect,
    }

    struct Stub {
        path: PathBuf,
        received: Arc<StdMutex<Vec<DaemonRequest>>>,
        _dir: tempfile::TempDir,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Stub {
        fn start(behavior: Behavior) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("t.sock");
            let listener = UnixListener::bind(&path).expect("bind");
            let received = Arc::new(StdMutex::new(Vec::new()));
            let handle = tokio::spawn(serve(listener, behavior, received.clone()));
            Self {
                path,
                received,
                _dir: dir,
                handle,
            }
        }

        fn methods(&self) -> Vec<String> {
            self.received
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.method.clone())
                .collect()
        }

        fn count(&self, method: &str) -> usize {
            self.methods().iter().filter(|m| *m == method).count()
        }

        /// Stops listening and removes the socket so reconnects fail.
        async fn shutdown(&self) {
            self.handle.abort();
            let _ = std::fs::remove_file(&self.path);
            // Give the abort a chance to land so the live connection drops.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn serve(
        listener: UnixListener,
        behavior: Behavior,
        received: Arc<StdMutex<Vec<DaemonRequest>>>,
    ) {
        use tokio::io::AsyncWriteExt;

        let mut calls = 0usize;
        let mut connections = 0usize;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            connections += 1;
            let (reader, writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut writer = writer;

            loop {
                let Ok(request) = read_framed_message(&mut reader, MAX_MESSAGE_SIZE).await else {
                    break;
                };
                received.lock().unwrap().push(request.clone());

                if request.method == "ping" {
                    let legacy = behavior == Behavior::LegacyPing
                        || (behavior == Behavior::SlowPingThenLegacyReconnect && connections > 1);
                    let result = if legacy {
                        json!({})
                    } else {
                        json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "maxMessageSize": MAX_MESSAGE_SIZE,
                        })
                    };
                    // A slow answer keeps the client's mutex held long enough
                    // for a second caller to queue behind it.
                    if behavior == Behavior::SlowPingThenLegacyReconnect && connections == 1 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    let response = DaemonResponse::success(request.id, result);
                    if write_framed_message(&mut writer, &response, MAX_MESSAGE_SIZE)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }

                calls += 1;

                // A well-framed reply the client cannot parse. Written raw,
                // because the framing helpers only emit valid messages.
                if behavior == Behavior::MalformedResponse && calls == 1 {
                    if writer.write_u32(4).await.is_err()
                        || writer.write_all(b"junk").await.is_err()
                    {
                        break;
                    }
                    continue;
                }

                let replies: Vec<DaemonResponse> = match behavior {
                    Behavior::SlowPingThenLegacyReconnect if connections == 1 => break,
                    Behavior::CloseAfterFirstCall if calls == 1 => break,
                    Behavior::HugeResponse if calls == 1 => vec![DaemonResponse::success(
                        request.id,
                        json!({ "blob": "x".repeat(2 * 1024 * 1024) }),
                    )],
                    Behavior::StaleThenCorrect if calls == 2 => vec![
                        DaemonResponse::success(request.id - 1, json!({ "stale": true })),
                        DaemonResponse::success(request.id, json!({ "ok": true })),
                    ],
                    Behavior::FutureId if calls == 1 => {
                        vec![DaemonResponse::success(request.id + 5, json!({}))]
                    }
                    _ => vec![DaemonResponse::success(request.id, json!({ "ok": true }))],
                };

                let mut failed = false;
                for reply in &replies {
                    if write_framed_message(&mut writer, reply, MAX_MESSAGE_SIZE)
                        .await
                        .is_err()
                    {
                        failed = true;
                        break;
                    }
                }
                if failed {
                    break;
                }
            }
        }
    }

    /// A params blob whose serialized form comfortably exceeds `bytes`.
    fn params_of_bytes(bytes: usize) -> serde_json::Value {
        json!({ "fullText": "y".repeat(bytes) })
    }

    // 14 — direct regression for the reported bug.
    #[tokio::test]
    async fn oversize_request_fails_fast_and_connection_survives() {
        let stub = Stub::start(Behavior::Healthy);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap();

        let err = client
            .call("store_memory", params_of_bytes(2 * 1024 * 1024))
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, BridgeError::TooLarge(_)), "got {text}");
        assert!(
            text.contains(&LEGACY_MAX_MESSAGE_SIZE.to_string()),
            "{text}"
        );
        assert!(text.contains("bytes"), "{text}");

        // The socket was never touched, so the next call still works.
        assert!(client.is_connected());
        client
            .call("get_memory", json!({ "id": "b" }))
            .await
            .unwrap();

        // Exactly the two small calls reached the wire.
        assert_eq!(stub.methods().len(), 2);
        assert_eq!(stub.count("get_memory"), 2);
        assert_eq!(stub.count("store_memory"), 0);
    }

    // 15
    #[tokio::test]
    async fn reconnects_after_server_closes_connection() {
        let stub = Stub::start(Behavior::CloseAfterFirstCall);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        // The first call is swallowed; the client reconnects and retries.
        client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap();
        assert!(client.is_connected());

        client
            .call("get_memory", json!({ "id": "b" }))
            .await
            .unwrap();
        assert_eq!(stub.count("ping"), 1, "reconnect renegotiates exactly once");
    }

    // 16
    #[tokio::test]
    async fn read_only_method_retries_transparently_after_mid_call_close() {
        let stub = Stub::start(Behavior::CloseAfterFirstCall);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        let result = client.call("list_memories", json!({})).await.unwrap();
        assert_eq!(result, json!({ "ok": true }));
        assert_eq!(
            stub.count("list_memories"),
            2,
            "the side-effect-free call was replayed"
        );
    }

    // 16b — regression: `search` and `find_similar` look read-only but
    // record an access for every result, which advances `last_accessed_at`
    // and the FSRS schedule. A transparent replay would age those memories
    // twice off one caller request, so the call is reported instead.
    #[tokio::test]
    async fn access_recording_reads_are_not_replayed() {
        for method in ["search", "find_similar"] {
            let stub = Stub::start(Behavior::CloseAfterFirstCall);
            let client = DaemonClient::connect(&stub.path).await.unwrap();

            let err = client.call(method, json!({})).await.unwrap_err();
            assert_eq!(
                stub.count(method),
                1,
                "{method} must reach the daemon exactly once"
            );

            // ...and the report must not describe a read as a half-applied
            // write, which is the other way to get this wrong.
            let text = err.to_string();
            assert!(text.contains("safe"), "{text}");
            assert!(!text.contains("may or may not have been applied"), "{text}");
            assert!(client.is_connected());
        }
    }

    // 17
    #[tokio::test]
    async fn mutating_method_is_not_retried() {
        let stub = Stub::start(Behavior::CloseAfterFirstCall);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        let err = client
            .call("store_memory", json!({ "summary": "s" }))
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("may or may not have been applied"), "{text}");
        assert!(text.contains("re-established"), "{text}");

        assert_eq!(
            stub.count("store_memory"),
            1,
            "the mutating call must reach the daemon exactly once"
        );
        // The client is healthy again for the next call.
        assert!(client.is_connected());
        client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap();
    }

    // 18
    #[tokio::test]
    async fn reconnect_gives_up_after_bounded_attempts() {
        let stub = Stub::start(Behavior::Healthy);
        let client = DaemonClient::connect(&stub.path).await.unwrap();
        client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap();

        stub.shutdown().await;

        let err = tokio::time::timeout(Duration::from_secs(5), async {
            // The first call discovers the dead peer; the second starts with
            // no connection at all. Both must terminate.
            let _ = client.call("get_memory", json!({ "id": "b" })).await;
            client.call("get_memory", json!({ "id": "c" })).await
        })
        .await
        .expect("reconnect must not loop forever")
        .unwrap_err();

        let text = err.to_string();
        assert!(text.contains(&stub.path.display().to_string()), "{text}");
        assert!(text.contains("3 attempts"), "{text}");
        assert!(!client.is_connected());
    }

    // 19
    #[tokio::test]
    async fn stale_response_id_is_skipped() {
        let stub = Stub::start(Behavior::StaleThenCorrect);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap();
        let result = client
            .call("get_memory", json!({ "id": "b" }))
            .await
            .unwrap();

        assert_eq!(result, json!({ "ok": true }), "the stale reply was skipped");
        assert!(client.is_connected());
        assert_eq!(stub.count("ping"), 0, "no reconnect was needed");
    }

    // 20
    #[tokio::test]
    async fn future_response_id_poisons_and_next_call_recovers() {
        let stub = Stub::start(Behavior::FutureId);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        // A mutating method, so the mismatch surfaces instead of being
        // silently replayed.
        let err = client.call("store_memory", json!({})).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("id mismatch"), "{text}");

        let result = client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap();
        assert_eq!(result, json!({ "ok": true }));
    }

    // 21 — regression for the silently-desyncing response path.
    #[tokio::test]
    async fn oversized_response_is_drained_and_connection_survives() {
        let stub = Stub::start(Behavior::HugeResponse);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        let err = client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap_err();
        assert!(matches!(err, BridgeError::TooLarge(_)), "{err}");

        // The oversized frame was drained, so the stream is still aligned.
        assert!(client.is_connected());
        let result = client
            .call("get_memory", json!({ "id": "b" }))
            .await
            .unwrap();
        assert_eq!(result, json!({ "ok": true }));
        assert_eq!(stub.count("ping"), 0, "no reconnect was needed");
    }

    // 21b — regression: a response we cannot parse is not a size problem.
    //
    // Both failures leave the stream on a frame boundary, and the client
    // used to collapse them into `TooLarge`, telling the caller to shrink
    // an input that was never the problem.
    #[tokio::test]
    async fn malformed_response_is_reported_as_a_protocol_error_not_a_size_error() {
        let stub = Stub::start(Behavior::MalformedResponse);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        let err = client
            .call("get_memory", json!({ "id": "a" }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BridgeError::Internal(_)),
            "a malformed response is a protocol failure, got {err}"
        );
        let text = err.to_string();
        assert!(text.contains("could not be parsed"), "{text}");
        assert!(!text.contains("Too large"), "{text}");

        // The stream is still aligned, so the next call goes through.
        assert!(client.is_connected());
        let result = client
            .call("get_memory", json!({ "id": "b" }))
            .await
            .unwrap();
        assert_eq!(result, json!({ "ok": true }));
        assert_eq!(stub.count("ping"), 0, "no reconnect was needed");
    }

    // 22
    #[tokio::test]
    async fn ping_negotiates_frame_limit() {
        let stub = Stub::start(Behavior::Healthy);
        let client = DaemonClient::connect(&stub.path).await.unwrap();
        assert_eq!(client.frame_limit().await, LEGACY_MAX_MESSAGE_SIZE);

        client.ping().await.unwrap();
        assert_eq!(client.frame_limit().await, MAX_MESSAGE_SIZE);
    }

    // 22c — regression: `ping` must not be a read-modify-write.
    //
    // The pinger holds the client mutex while the (slow) first connection
    // answers, so the concurrent `get_memory` queues behind it. When that
    // call finds the connection dead it reconnects to what is now a v1
    // daemon and correctly clamps the limit to 1 MiB. If `ping` released the
    // lock between its exchange and its write, tokio's FIFO mutex hands the
    // waiter in first and the stale 8 MiB lands on top of that clamp -
    // after which the client sends frames a v1 daemon rejects *without*
    // draining, desynchronizing the stream for good.
    #[tokio::test]
    async fn concurrent_reconnect_cannot_have_its_clamp_overwritten_by_ping() {
        let stub = Stub::start(Behavior::SlowPingThenLegacyReconnect);
        let client = Arc::new(DaemonClient::connect(&stub.path).await.unwrap());

        let pinger = {
            let client = client.clone();
            tokio::spawn(async move { client.ping().await })
        };

        // Long enough for the ping to own the lock and block on the answer.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let reader = {
            let client = client.clone();
            tokio::spawn(async move { client.call("get_memory", json!({ "id": "a" })).await })
        };

        pinger.await.unwrap().unwrap();
        reader.await.unwrap().unwrap();

        assert_eq!(
            client.frame_limit().await,
            LEGACY_MAX_MESSAGE_SIZE,
            "the live connection is a v1 daemon; its clamp must be the one in force"
        );
    }

    // 22b
    #[tokio::test]
    async fn legacy_daemon_returning_empty_ping_clamps_to_1mib() {
        let stub = Stub::start(Behavior::LegacyPing);
        let client = DaemonClient::connect(&stub.path).await.unwrap();

        client.ping().await.unwrap();
        assert_eq!(client.frame_limit().await, LEGACY_MAX_MESSAGE_SIZE);
    }

    #[test]
    fn only_methods_without_observable_effects_are_retry_safe() {
        for method in [
            "store_memory",
            "create_namespace",
            "reinforce_memory",
            "delete_memory",
            "shutdown",
            // Not mutations, but not effect-free either: both record an
            // access per result, which moves the decay schedule.
            "search",
            "find_similar",
        ] {
            assert!(!is_retry_safe(method), "{method} must not be replayed");
        }
        for method in [
            "ping",
            "check_health",
            "scan_duplicates",
            "get_memory",
            "list_namespaces",
            "namespace_stats",
            "list_memories",
        ] {
            assert!(is_retry_safe(method), "{method} should be replayable");
        }
    }

    #[test]
    fn negotiated_limit_ignores_unknown_and_absurd_announcements() {
        assert_eq!(negotiated_limit(&json!({})), LEGACY_MAX_MESSAGE_SIZE);
        assert_eq!(
            negotiated_limit(&json!({ "protocolVersion": 1, "maxMessageSize": 99_999_999 })),
            LEGACY_MAX_MESSAGE_SIZE
        );
        assert_eq!(
            negotiated_limit(&json!({ "protocolVersion": 2, "maxMessageSize": 99_999_999 })),
            MAX_MESSAGE_SIZE
        );
        assert_eq!(
            negotiated_limit(&json!({ "protocolVersion": 2, "maxMessageSize": 1024 })),
            LEGACY_MAX_MESSAGE_SIZE
        );
    }
}
