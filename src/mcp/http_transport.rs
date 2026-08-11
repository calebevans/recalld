//! Streamable HTTP transport for MCP.
//!
//! Implements the MCP HTTP transport as an axum router mountable
//! alongside the REST API, serving BOTH protocol eras on `/mcp`:
//!
//! * **Modern** (MCP 2026-07-28): stateless. Identity and protocol version
//!   travel in `params._meta` and are mirrored into the SEP-2243 headers.
//!   No session is minted, any inbound `Mcp-Session-Id` is ignored, and
//!   requests are dispatched with NO lock -- so concurrent clients do not
//!   serialize behind one mutex.
//! * **Legacy** (MCP 2025-06-18 and earlier): `initialize` mints an
//!   `Mcp-Session-Id`, and each session gets its own [`McpServer`] over the
//!   shared [`McpDispatcher`].
//!
//! The router carries its own state and intentionally applies none of the
//! REST API's tower stack -- in particular no `TimeoutLayer`, since MCP tool
//! calls may legitimately outrun the REST request timeout. The body limit
//! and Origin validation are therefore applied here rather than inherited.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::Engine as _;
use dashmap::DashMap;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::mcp::protocol::*;
use crate::mcp::server::{McpDispatcher, McpHandler, McpServer, RequestContext};

// ── Headers ─────────────────────────────────────────────────────────
// Header NAMES are case-insensitive (hyper lowercases them); header
// VALUES are case-sensitive.

const HDR_SESSION_ID: &str = "mcp-session-id";
const HDR_MCP_METHOD: &str = "mcp-method";
const HDR_MCP_NAME: &str = "mcp-name";
const HDR_MCP_PROTOCOL_VER: &str = "mcp-protocol-version";

/// SEP-2243 sentinel wrapping a header value that is not safe plain ASCII.
/// The markers are lowercase and exact.
const B64_PREFIX: &str = "=?base64?";
const B64_SUFFIX: &str = "?=";

/// How often the opportunistic session sweep may actually run.
const SWEEP_THROTTLE_MS: u64 = 60_000;

/// Methods allowed on `/mcp`, echoed in `Allow` on a 405.
const ALLOW_METHODS: &str = "POST, DELETE";

// ── Configuration ───────────────────────────────────────────────────

/// Tuning knobs for the MCP HTTP transport.
#[derive(Debug, Clone)]
pub struct McpHttpConfig {
    /// Maximum request body size in bytes. Mirrors the REST API default.
    pub max_body_bytes: usize,
    /// Extra origins allowed beyond loopback, matched case-insensitively
    /// and exactly (scheme, host and port all significant).
    pub allowed_origins: Vec<String>,
    /// How long a legacy session may sit idle before it is swept.
    pub session_idle_timeout: Duration,
    /// Hard cap on concurrent legacy sessions.
    pub max_sessions: usize,
}

impl Default for McpHttpConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 10 * 1024 * 1024,
            allowed_origins: Vec::new(),
            session_idle_timeout: Duration::from_secs(30 * 60),
            max_sessions: 1024,
        }
    }
}

// ── Origin policy ───────────────────────────────────────────────────

/// Origin validation for `/mcp`.
///
/// The spec makes this a MUST ("Servers MUST validate the Origin header on
/// all incoming connections to prevent DNS rebinding attacks"), and it
/// matters here because recalld binds to loopback by default.
#[derive(Debug)]
struct OriginPolicy {
    allowed: Vec<String>,
}

impl OriginPolicy {
    fn new(allowed: &[String]) -> Self {
        Self {
            allowed: allowed.iter().map(|o| o.to_ascii_lowercase()).collect(),
        }
    }

    /// Decide whether a request bearing `origin` may proceed.
    fn allows(&self, origin: Option<&str>) -> bool {
        let Some(origin) = origin else {
            // ABSENT -> allow. Browsers always send Origin on POST; a
            // missing one means a non-browser client (curl, an SDK), which
            // is not the DNS-rebinding threat model.
            return true;
        };
        if origin.eq_ignore_ascii_case("null") {
            // JUDGMENT CALL (flagged): `Origin: null` is the opaque origin
            // used by `file://` documents and sandboxed iframes. It is not
            // attributable, so it is denied.
            return false;
        }
        if is_loopback_origin(origin) {
            // Port- and scheme-agnostic on purpose: DNS rebinding needs an
            // attacker-controlled HOSTNAME, and a rebound name is never
            // literally `localhost`. Matching on the port too would break
            // every dev tool for no security gain.
            return true;
        }
        let lower = origin.to_ascii_lowercase();
        self.allowed.contains(&lower)
    }
}

/// Whether an origin's host is a loopback name or address.
fn is_loopback_origin(origin: &str) -> bool {
    let rest = origin.split_once("://").map_or(origin, |(_, r)| r);
    let host = if let Some(end) = rest.find(']') {
        &rest[..=end]
    } else {
        rest.split(':').next().unwrap_or(rest)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

// ── State ───────────────────────────────────────────────────────────

/// One legacy session: its server plus the last time it was touched.
struct SessionEntry {
    server: Arc<Mutex<McpServer>>,
    last_seen: Arc<AtomicU64>,
}

#[derive(Clone)]
struct McpHttpState {
    dispatcher: Arc<McpDispatcher>,
    sessions: Arc<DashMap<String, SessionEntry>>,
    last_sweep: Arc<AtomicU64>,
    origins: Arc<OriginPolicy>,
    config: Arc<McpHttpConfig>,
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Router ──────────────────────────────────────────────────────────

/// Build a self-contained `Router` for the MCP HTTP transport.
///
/// Handles POST, DELETE and GET at `/mcp`. Applies a body limit (the REST
/// API's limit is not inherited, because `/mcp` is merged in outside that
/// tower stack) but deliberately no timeout.
pub fn mcp_router(handler: Arc<dyn McpHandler>, config: McpHttpConfig) -> Router {
    build_router(handler, config).0
}

fn build_router(handler: Arc<dyn McpHandler>, config: McpHttpConfig) -> (Router, McpHttpState) {
    let state = McpHttpState {
        dispatcher: Arc::new(McpDispatcher::new(handler)),
        sessions: Arc::new(DashMap::new()),
        last_sweep: Arc::new(AtomicU64::new(0)),
        origins: Arc::new(OriginPolicy::new(&config.allowed_origins)),
        config: Arc::new(config),
    };

    let max_body = state.config.max_body_bytes;
    let router = Router::new()
        .route(
            "/mcp",
            post(handle_post).delete(handle_delete).get(handle_get),
        )
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state.clone());

    (router, state)
}

// ── Response helpers ────────────────────────────────────────────────

fn jsonrpc_error(
    status: StatusCode,
    id: JsonRpcId,
    code: i32,
    message: impl Into<String>,
    data: Option<serde_json::Value>,
) -> Response {
    (
        status,
        Json(JsonRpcResponse::error_with_data(id, code, message, data)),
    )
        .into_response()
}

fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, ALLOW_METHODS)],
    )
        .into_response()
}

fn origin_rejected(origin: &str) -> Response {
    tracing::warn!(origin = %origin, "rejected /mcp request: Origin not allowed");
    // Deliberately a plain JSON body, not JSON-RPC: the rejection happens
    // before the body is parsed, so there is no request id to answer with.
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": "Origin not allowed" })),
    )
        .into_response()
}

fn dispatch_response(response: Option<JsonRpcResponse>) -> Response {
    match response {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

// ── POST /mcp ───────────────────────────────────────────────────────

async fn handle_post(
    State(state): State<McpHttpState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(origin) = header_str(&headers, header::ORIGIN.as_str())
        && !state.origins.allows(Some(origin))
    {
        return origin_rejected(origin);
    }

    maybe_sweep(&state);

    let message: JsonRpcMessage = match serde_json::from_slice(&body) {
        Ok(msg) => msg,
        Err(e) => {
            // No usable id: answer with an explicit null rather than
            // fabricating `0` and colliding with a real request.
            return jsonrpc_error(
                StatusCode::BAD_REQUEST,
                JsonRpcId::Null,
                PARSE_ERROR,
                format!("Parse error: {e}"),
                None,
            );
        }
    };

    match era_hint(&message) {
        EraHint::Modern => handle_modern(state, headers, message).await,
        EraHint::LegacyHandshake if message.method == "initialize" => {
            handle_legacy_initialize(state, headers, message).await
        }
        EraHint::LegacyHandshake => handle_legacy_session(state, headers, message).await,
        EraHint::Ambiguous => {
            if header_str(&headers, HDR_SESSION_ID).is_some() {
                handle_legacy_session(state, headers, message).await
            } else if is_era_neutral_method(&message.method) {
                // `server/discover` and `ping` are answered without a
                // handshake and without a session -- the stdio-style era
                // probe works over HTTP too.
                let response = state
                    .dispatcher
                    .dispatch(&RequestContext::legacy(), message)
                    .await;
                dispatch_response(response)
            } else {
                let id = message.id.clone().unwrap_or(JsonRpcId::Null);
                jsonrpc_error(
                    StatusCode::BAD_REQUEST,
                    id,
                    INVALID_PARAMS,
                    MetaError::MissingField(META_PROTOCOL_VERSION).to_string(),
                    None,
                )
            }
        }
    }
}

/// Stateless MCP 2026-07-28 request: no session, no lock.
async fn handle_modern(
    state: McpHttpState,
    headers: HeaderMap,
    message: JsonRpcMessage,
) -> Response {
    let id = message.id.clone().unwrap_or(JsonRpcId::Null);
    let is_notification = message.id.is_none();

    let meta = match parse_request_meta(message.params.as_ref()) {
        Ok(meta) => Some(meta),
        Err(e) if is_notification => {
            tracing::debug!(error = %e, "modern notification carried invalid _meta; ignoring");
            None
        }
        Err(e) => {
            return jsonrpc_error(
                StatusCode::BAD_REQUEST,
                id,
                e.code(),
                e.to_string(),
                e.data(),
            );
        }
    };

    if let Some(meta) = meta.as_ref()
        && !is_notification
        && let Err(detail) = validate_modern_headers(&headers, &message, meta)
    {
        return jsonrpc_error(StatusCode::BAD_REQUEST, id, HEADER_MISMATCH, detail, None);
    }

    if header_str(&headers, HDR_SESSION_ID).is_some() {
        // Spec MUST: a modern-era request's session id is ignored, and no
        // session id is minted or echoed back.
        tracing::debug!("ignoring Mcp-Session-Id on a stateless 2026-07-28 request");
    }

    let ctx = match meta {
        Some(meta) => RequestContext::modern(meta),
        None => RequestContext::legacy(),
    };
    dispatch_response(state.dispatcher.dispatch(&ctx, message).await)
}

/// SEP-2243 header validation. Returns the error detail on mismatch.
fn validate_modern_headers(
    headers: &HeaderMap,
    message: &JsonRpcMessage,
    meta: &RequestMeta,
) -> Result<(), String> {
    // NOTIFICATION POSTs are never routed here: whether these headers are
    // required on notifications is explicitly "not defined by this
    // revision", so we skip validation for them. Skipping is the
    // non-breaking choice; revisit if a later revision defines it.
    debug_assert!(message.id.is_some());

    let raw_method = header_str(headers, HDR_MCP_METHOD)
        .ok_or_else(|| "Header mismatch: missing required Mcp-Method header".to_string())?;
    let method = decode_header_value(raw_method, "Mcp-Method")?;
    if method != message.method {
        return Err(format!(
            "Header mismatch: Mcp-Method header \"{method}\" does not match body method \"{}\"",
            message.method
        ));
    }

    let raw_version = header_str(headers, HDR_MCP_PROTOCOL_VER).ok_or_else(|| {
        "Header mismatch: missing required MCP-Protocol-Version header".to_string()
    })?;
    let version = decode_header_value(raw_version, "MCP-Protocol-Version")?;
    if version != meta.protocol_version {
        return Err(format!(
            "Header mismatch: MCP-Protocol-Version header \"{version}\" does not match \
             _meta protocol version \"{}\"",
            meta.protocol_version
        ));
    }

    // Mcp-Name is required only for the three designated methods. Present
    // but unexpected on any other method is ignored, not an error.
    let expected = match message.method.as_str() {
        "tools/call" | "prompts/get" => Some(("name", "params.name")),
        "resources/read" => Some(("uri", "params.uri")),
        _ => None,
    };
    if let Some((field, label)) = expected {
        let want = message
            .params
            .as_ref()
            .and_then(|p| p.get(field))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let raw_name = header_str(headers, HDR_MCP_NAME)
            .ok_or_else(|| "Header mismatch: missing required Mcp-Name header".to_string())?;
        let name = decode_header_value(raw_name, "Mcp-Name")?;
        if name != want {
            return Err(format!(
                "Header mismatch: Mcp-Name header does not match {label}"
            ));
        }
    }

    Ok(())
}

/// Decode the SEP-2243 `=?base64?{value}?=` sentinel, if present.
///
/// FLAGGED: the sentinel's base64 alphabet and padding are not pinned down
/// by the material available here. We accept standard base64 and fall back
/// to unpadded standard base64; verify against SEP-2243 or an SDK before
/// relying on anything more exotic.
fn decode_header_value(raw: &str, header_name: &str) -> Result<String, String> {
    let Some(inner) = raw
        .strip_prefix(B64_PREFIX)
        .and_then(|r| r.strip_suffix(B64_SUFFIX))
    else {
        return Ok(raw.to_string());
    };
    let malformed =
        || format!("Header mismatch: malformed base64 sentinel in {header_name} header");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(inner)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(inner))
        .map_err(|_| malformed())?;
    String::from_utf8(bytes).map_err(|_| malformed())
}

// ── Legacy era ──────────────────────────────────────────────────────

async fn handle_legacy_initialize(
    state: McpHttpState,
    headers: HeaderMap,
    message: JsonRpcMessage,
) -> Response {
    // Reuse a live session if the client already has one, instead of
    // minting a second and orphaning the first.
    let existing = header_str(&headers, HDR_SESSION_ID)
        .map(str::to_string)
        .filter(|id| state.sessions.contains_key(id));

    let (session_id, server, is_new) = match existing {
        Some(id) => {
            let server = match state.sessions.get(&id) {
                Some(entry) => {
                    entry.last_seen.store(now_ms(), Ordering::Relaxed);
                    entry.server.clone()
                }
                None => return unknown_session(&message),
            };
            (id, server, false)
        }
        None => {
            sweep_sessions(
                &state.sessions,
                now_ms(),
                state.config.session_idle_timeout.as_millis() as u64,
            );
            if state.sessions.len() >= state.config.max_sessions {
                let id = message.id.clone().unwrap_or(JsonRpcId::Null);
                return jsonrpc_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    id,
                    INTERNAL_ERROR,
                    "Too many active MCP sessions",
                    None,
                );
            }
            (
                Uuid::new_v4().to_string(),
                Arc::new(Mutex::new(McpServer::from_dispatcher(
                    state.dispatcher.clone(),
                ))),
                true,
            )
        }
    };

    let response = {
        let mut srv = server.lock().await;
        srv.handle_message(message).await
    };

    let succeeded = response.as_ref().is_some_and(|resp| resp.error.is_none());

    // Only a SUCCESSFUL initialize creates a session. A failed one used to
    // leave behind an entry that could never become initialized.
    if is_new && succeeded {
        state.sessions.insert(
            session_id.clone(),
            SessionEntry {
                server,
                last_seen: Arc::new(AtomicU64::new(now_ms())),
            },
        );
        tracing::info!(session_id = %session_id, "MCP HTTP session created");
    }

    let mut http_resp = dispatch_response(response);
    if succeeded && let Ok(value) = HeaderValue::from_str(&session_id) {
        http_resp.headers_mut().insert(HDR_SESSION_ID, value);
    }
    http_resp
}

async fn handle_legacy_session(
    state: McpHttpState,
    headers: HeaderMap,
    message: JsonRpcMessage,
) -> Response {
    let session_id = match header_str(&headers, HDR_SESSION_ID) {
        Some(id) => id.to_string(),
        None => {
            return jsonrpc_error(
                StatusCode::BAD_REQUEST,
                message.id.clone().unwrap_or(JsonRpcId::Null),
                INVALID_REQUEST,
                "Missing Mcp-Session-Id header",
                None,
            );
        }
    };

    let server = match state.sessions.get(&session_id) {
        Some(entry) => {
            entry.last_seen.store(now_ms(), Ordering::Relaxed);
            entry.server.clone()
        }
        None => return unknown_session(&message),
    };

    let response = {
        let mut srv = server.lock().await;
        srv.handle_message(message).await
    };
    dispatch_response(response)
}

/// A 404 that a client can actually act on -- what makes idle expiry
/// recoverable rather than a silent hang.
fn unknown_session(message: &JsonRpcMessage) -> Response {
    jsonrpc_error(
        StatusCode::NOT_FOUND,
        message.id.clone().unwrap_or(JsonRpcId::Null),
        INVALID_REQUEST,
        "Unknown or expired MCP session; send 'initialize' again",
        None,
    )
}

// ── Session lifetime ────────────────────────────────────────────────

/// Evict sessions idle for at least `idle_ms`. Returns how many were
/// removed.
///
/// Pure in `now_ms` so it is unit-testable without a clock abstraction.
fn sweep_sessions(sessions: &DashMap<String, SessionEntry>, now_ms: u64, idle_ms: u64) -> usize {
    let before = sessions.len();
    sessions.retain(|_, entry| {
        let last = entry.last_seen.load(Ordering::Relaxed);
        now_ms.saturating_sub(last) < idle_ms
    });
    let removed = before - sessions.len();
    if removed > 0 {
        tracing::info!(removed, "swept idle MCP HTTP sessions");
    }
    removed
}

/// Run the sweep at most once per [`SWEEP_THROTTLE_MS`].
///
/// Lazy rather than a background task: the router constructor has no
/// shutdown handle with which to cancel one, and a lazy sweep is
/// deterministic and testable.
fn maybe_sweep(state: &McpHttpState) {
    let now = now_ms();
    let last = state.last_sweep.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SWEEP_THROTTLE_MS {
        return;
    }
    if state
        .last_sweep
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    sweep_sessions(
        &state.sessions,
        now,
        state.config.session_idle_timeout.as_millis() as u64,
    );
}

// ── DELETE / GET ────────────────────────────────────────────────────

async fn handle_delete(State(state): State<McpHttpState>, headers: HeaderMap) -> Response {
    if let Some(origin) = header_str(&headers, header::ORIGIN.as_str())
        && !state.origins.allows(Some(origin))
    {
        return origin_rejected(origin);
    }

    // Without a session id there is nothing to delete: DELETE is a
    // legacy-only affordance, so the modern-era answer (405) applies.
    let Some(session_id) = header_str(&headers, HDR_SESSION_ID) else {
        return method_not_allowed();
    };

    match state.sessions.remove(session_id) {
        Some(_) => {
            tracing::info!(session_id = %session_id, "MCP HTTP session terminated");
            StatusCode::OK.into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The 2026-07-28 transport removed the GET stream endpoint; recalld never
/// had one, so 405 is both compliant and non-regressive.
async fn handle_get() -> Response {
    method_not_allowed()
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::server::tests::{
        initialize_request, modern_request, plain_request, stub_handler,
    };
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state(config: McpHttpConfig) -> (Router, McpHttpState) {
        build_router(stub_handler(), config)
    }

    fn router() -> Router {
        test_state(McpHttpConfig::default()).0
    }

    struct Req {
        builder: axum::http::request::Builder,
        body: Vec<u8>,
    }

    impl Req {
        fn post(message: &JsonRpcMessage) -> Self {
            Self {
                builder: Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json"),
                body: serde_json::to_vec(message).unwrap(),
            }
        }

        fn raw(body: Vec<u8>) -> Self {
            Self {
                builder: Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json"),
                body,
            }
        }

        fn header(mut self, name: &str, value: &str) -> Self {
            self.builder = self.builder.header(name, value);
            self
        }

        /// Attach the three SEP-2243 headers a modern client must send.
        fn sep2243(self, method: &str, name: Option<&str>) -> Self {
            let mut req = self
                .header(HDR_MCP_METHOD, method)
                .header(HDR_MCP_PROTOCOL_VER, PROTOCOL_VERSION);
            if let Some(name) = name {
                req = req.header(HDR_MCP_NAME, name);
            }
            req
        }

        fn build(self) -> Request<Body> {
            self.builder.body(Body::from(self.body)).unwrap()
        }
    }

    async fn send(router: &Router, req: Req) -> (StatusCode, HeaderMap, serde_json::Value) {
        let resp = router.clone().oneshot(req.build()).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, headers, json)
    }

    fn tools_list() -> JsonRpcMessage {
        modern_request(1, "tools/list", serde_json::json!({}))
    }

    // ── Modern path ─────────────────────────────────────────────────

    #[tokio::test]
    async fn modern_post_succeeds_without_a_session_header() {
        let r = router();
        let (status, _, body) =
            send(&r, Req::post(&tools_list()).sep2243("tools/list", None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["tools"][0]["name"], "store_memory");
        assert_eq!(body["result"]["resultType"], "complete");
    }

    #[tokio::test]
    async fn modern_response_carries_no_session_header() {
        let r = router();
        let (_, headers, _) = send(&r, Req::post(&tools_list()).sep2243("tools/list", None)).await;
        assert!(headers.get(HDR_SESSION_ID).is_none());
    }

    #[tokio::test]
    async fn modern_request_ignores_an_inbound_session_id() {
        let (r, state) = test_state(McpHttpConfig::default());
        let (status, headers, _) = send(
            &r,
            Req::post(&tools_list())
                .sep2243("tools/list", None)
                .header(HDR_SESSION_ID, "does-not-exist"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(HDR_SESSION_ID).is_none());
        assert_eq!(state.sessions.len(), 0);
    }

    #[tokio::test]
    async fn missing_mcp_method_header_is_header_mismatch() {
        let r = router();
        let (status, _, body) = send(
            &r,
            Req::post(&tools_list()).header(HDR_MCP_PROTOCOL_VER, PROTOCOL_VERSION),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], HEADER_MISMATCH);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Mcp-Method")
        );
    }

    #[tokio::test]
    async fn mismatched_mcp_method_header_is_header_mismatch() {
        let r = router();
        let (status, _, body) =
            send(&r, Req::post(&tools_list()).sep2243("tools/call", None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], HEADER_MISMATCH);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not match body method")
        );
    }

    #[tokio::test]
    async fn missing_protocol_version_header_is_header_mismatch() {
        let r = router();
        let (status, _, body) = send(
            &r,
            Req::post(&tools_list()).header(HDR_MCP_METHOD, "tools/list"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], HEADER_MISMATCH);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("MCP-Protocol-Version")
        );
    }

    #[tokio::test]
    async fn mcp_name_mismatch_on_tools_call_is_header_mismatch() {
        let r = router();
        let msg = modern_request(
            1,
            "tools/call",
            serde_json::json!({ "name": "store_memory", "arguments": {} }),
        );
        let (status, _, body) = send(
            &r,
            Req::post(&msg).sep2243("tools/call", Some("recall_memories")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], HEADER_MISMATCH);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("params.name")
        );
    }

    #[tokio::test]
    async fn base64_sentinel_is_decoded_before_comparison() {
        let r = router();
        let msg = modern_request(
            1,
            "tools/call",
            serde_json::json!({ "name": "store_memory", "arguments": {} }),
        );
        let encoded = format!(
            "{B64_PREFIX}{}{B64_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("store_memory")
        );
        let (status, _, body) =
            send(&r, Req::post(&msg).sep2243("tools/call", Some(&encoded))).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["result"]["content"][0]["text"], "called store_memory");

        // A malformed sentinel is a mismatch, not a silent pass.
        let bad = format!("{B64_PREFIX}!!!not-base64!!!{B64_SUFFIX}");
        let (status, _, body) = send(&r, Req::post(&msg).sep2243("tools/call", Some(&bad))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], HEADER_MISMATCH);
    }

    #[tokio::test]
    async fn modern_notification_skips_header_validation() {
        let r = router();
        let mut msg = modern_request(1, "notifications/cancelled", serde_json::json!({}));
        msg.id = None;
        // No SEP-2243 headers at all.
        let (status, _, _) = send(&r, Req::post(&msg)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn unsupported_protocol_version_is_rejected_with_supported_list() {
        let r = router();
        let msg = plain_request(
            1,
            "tools/list",
            Some(serde_json::json!({
                "_meta": {
                    META_PROTOCOL_VERSION: "2027-01-01",
                    META_CLIENT_CAPABILITIES: {},
                }
            })),
        );
        let (status, _, body) = send(
            &r,
            Req::post(&msg)
                .header(HDR_MCP_METHOD, "tools/list")
                .header(HDR_MCP_PROTOCOL_VER, "2027-01-01"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(body["error"]["data"]["requested"], "2027-01-01");
        assert_eq!(
            body["error"]["data"]["supported"],
            serde_json::json!([PROTOCOL_VERSION])
        );
    }

    // ── Legacy path ─────────────────────────────────────────────────

    #[tokio::test]
    async fn legacy_initialize_mints_a_session_id() {
        let (r, state) = test_state(McpHttpConfig::default());
        let (status, headers, body) =
            send(&r, Req::post(&initialize_request(1, "2025-06-18"))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(HDR_SESSION_ID).is_some());
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(state.sessions.len(), 1);
    }

    #[tokio::test]
    async fn failed_initialize_does_not_insert_a_session() {
        let (r, state) = test_state(McpHttpConfig::default());
        // `protocolVersion` as a number fails InitializeParams parsing.
        let msg = plain_request(
            1,
            "initialize",
            Some(serde_json::json!({ "protocolVersion": 7 })),
        );
        let (_, headers, body) = send(&r, Req::post(&msg)).await;
        assert!(body["error"].is_object(), "expected an error: {body}");
        assert!(headers.get(HDR_SESSION_ID).is_none());
        assert_eq!(state.sessions.len(), 0);
    }

    #[tokio::test]
    async fn repeat_initialize_with_a_live_session_reuses_the_same_id() {
        let (r, state) = test_state(McpHttpConfig::default());
        let (_, headers, _) = send(&r, Req::post(&initialize_request(1, "2025-06-18"))).await;
        let first = headers
            .get(HDR_SESSION_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let (_, headers, _) = send(
            &r,
            Req::post(&initialize_request(2, "2025-06-18")).header(HDR_SESSION_ID, &first),
        )
        .await;
        let second = headers.get(HDR_SESSION_ID).unwrap().to_str().unwrap();
        assert_eq!(first, second);
        assert_eq!(state.sessions.len(), 1, "no orphaned session");
    }

    #[tokio::test]
    async fn legacy_message_without_session_header_echoes_the_real_id() {
        let r = router();
        // `ping` is era-neutral, so use a method that requires a session.
        let msg = plain_request(4242, "tools/call", Some(serde_json::json!({ "name": "x" })));
        let (status, _, body) = send(&r, Req::post(&msg)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["id"], 4242, "must not fabricate id 0");
        assert_eq!(body["error"]["code"], INVALID_PARAMS);

        // With a session header but no such session, the id is echoed too.
        let (status, _, body) = send(&r, Req::post(&msg).header(HDR_SESSION_ID, "nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["id"], 4242);
    }

    #[tokio::test]
    async fn unknown_session_returns_404_with_a_jsonrpc_body() {
        let r = router();
        let msg = plain_request(9, "tools/list", None);
        let (status, _, body) = send(&r, Req::post(&msg).header(HDR_SESSION_ID, "ghost")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], INVALID_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("send 'initialize' again")
        );
    }

    #[tokio::test]
    async fn parse_error_uses_a_null_id() {
        let r = router();
        let (status, _, body) = send(&r, Req::raw(b"{not json".to_vec())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], PARSE_ERROR);
        assert!(body["id"].is_null(), "expected null id, got {}", body["id"]);
    }

    // ── DELETE / GET ────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_with_a_session_id_terminates_it() {
        let (r, state) = test_state(McpHttpConfig::default());
        let (_, headers, _) = send(&r, Req::post(&initialize_request(1, "2025-06-18"))).await;
        let sid = headers
            .get(HDR_SESSION_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let resp = r
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/mcp")
                    .header(HDR_SESSION_ID, &sid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(state.sessions.len(), 0);
    }

    #[tokio::test]
    async fn delete_without_a_session_id_is_405() {
        let r = router();
        let resp = r
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(resp.headers()[header::ALLOW], ALLOW_METHODS);
    }

    #[tokio::test]
    async fn get_is_405_with_allow_header() {
        let r = router();
        let resp = r
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(resp.headers()[header::ALLOW], ALLOW_METHODS);
    }

    // ── Session lifetime ────────────────────────────────────────────

    #[tokio::test]
    async fn sweep_evicts_idle_sessions_and_keeps_recent_ones() {
        let sessions: DashMap<String, SessionEntry> = DashMap::new();
        let make = |last: u64| SessionEntry {
            server: Arc::new(Mutex::new(McpServer::new(stub_handler()))),
            last_seen: Arc::new(AtomicU64::new(last)),
        };
        sessions.insert("stale".to_string(), make(0));
        sessions.insert("fresh".to_string(), make(9_000));

        let removed = sweep_sessions(&sessions, 10_000, 5_000);
        assert_eq!(removed, 1);
        assert!(sessions.contains_key("fresh"));
        assert!(!sessions.contains_key("stale"));
    }

    #[tokio::test]
    async fn session_cap_returns_503() {
        let (r, state) = test_state(McpHttpConfig {
            max_sessions: 1,
            ..Default::default()
        });
        send(&r, Req::post(&initialize_request(1, "2025-06-18"))).await;
        assert_eq!(state.sessions.len(), 1);

        let (status, _, body) = send(&r, Req::post(&initialize_request(2, "2025-06-18"))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], INTERNAL_ERROR);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Too many active MCP sessions")
        );
    }

    // ── Origin validation ───────────────────────────────────────────

    #[tokio::test]
    async fn origin_absent_is_allowed() {
        let r = router();
        let (status, _, _) = send(&r, Req::post(&tools_list()).sep2243("tools/list", None)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn loopback_origins_are_allowed_on_any_scheme_or_port() {
        let r = router();
        for origin in [
            "http://localhost",
            "http://localhost:7680",
            "https://127.0.0.1:3000",
            "http://[::1]:7680",
        ] {
            let (status, _, _) = send(
                &r,
                Req::post(&tools_list())
                    .sep2243("tools/list", None)
                    .header("origin", origin),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "origin {origin}");
        }
    }

    #[tokio::test]
    async fn opaque_null_origin_is_forbidden() {
        let r = router();
        let (status, _, body) = send(
            &r,
            Req::post(&tools_list())
                .sep2243("tools/list", None)
                .header("origin", "null"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "Origin not allowed");
    }

    #[tokio::test]
    async fn foreign_origin_is_forbidden() {
        let r = router();
        let (status, _, _) = send(
            &r,
            Req::post(&tools_list())
                .sep2243("tools/list", None)
                .header("origin", "http://evil.example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn allowlisted_origin_is_permitted() {
        let (r, _) = test_state(McpHttpConfig {
            allowed_origins: vec!["https://App.Example.com".to_string()],
            ..Default::default()
        });
        let (status, _, _) = send(
            &r,
            Req::post(&tools_list())
                .sep2243("tools/list", None)
                .header("origin", "https://app.example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    // ── Body limit ──────────────────────────────────────────────────

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let (r, _) = test_state(McpHttpConfig {
            max_body_bytes: 512,
            ..Default::default()
        });
        let (status, _, _) = send(&r, Req::raw(vec![b'x'; 4096])).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    // ── Era-neutral over HTTP ───────────────────────────────────────

    #[tokio::test]
    async fn discover_is_answered_without_meta_or_session() {
        let r = router();
        let (status, headers, body) =
            send(&r, Req::post(&plain_request(1, "server/discover", None))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(HDR_SESSION_ID).is_none());
        assert_eq!(
            body["result"]["supportedVersions"],
            serde_json::json!([PROTOCOL_VERSION])
        );
    }
}
