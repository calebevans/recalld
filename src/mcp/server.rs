//! MCP server: stateless dispatcher plus a legacy lifecycle wrapper.
//!
//! Recalld is a dual-era MCP server (see [`crate::mcp::protocol`]):
//!
//! * [`McpDispatcher`] holds no per-connection state and takes `&self`, so
//!   modern (MCP 2026-07-28) requests can be served concurrently with no
//!   lock at all.
//! * [`McpServer`] wraps a dispatcher with the legacy 2025-06-18 lifecycle
//!   state (`initialized`, negotiated version). Its constructor and
//!   `handle_message` signatures are unchanged, so stdio and the HTTP
//!   session map need no churn.
//!
//! Method dispatch is delegated to the [`McpHandler`] trait, which is
//! implemented by the domain layer (CS-22) and is UNCHANGED by the
//! 2026-07-28 migration.

use std::sync::Arc;

use async_trait::async_trait;

use crate::mcp::protocol::*;

// ── McpHandler Trait ────────────────────────────────────────────────

/// Handler for MCP tool calls and resource reads.
///
/// Implemented by `McpBridge` in CS-22. The server holds a
/// `Arc<dyn McpHandler>` and delegates all domain logic to it.
#[async_trait]
pub trait McpHandler: Send + Sync {
    /// Return the list of tools this server exposes.
    fn tools(&self) -> Vec<ToolInfo>;

    /// Return the list of static resources.
    fn resources(&self) -> Vec<ResourceInfo>;

    /// Return the list of resource URI templates.
    fn resource_templates(&self) -> Vec<ResourceTemplate>;

    /// Execute a tool call and return the result.
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> ToolCallResult;

    /// Read a resource by URI and return its content.
    async fn read_resource(&self, uri: &str) -> Result<ResourceReadResult, String>;
}

// ── Request context and outcome ─────────────────────────────────────

/// Everything the dispatcher needs to know about a single request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Which protocol era this request is served under.
    pub era: Era,
    /// Validated modern `_meta`, when the request carried one.
    pub meta: Option<RequestMeta>,
    /// The version a legacy `initialize` should echo, pre-negotiated by
    /// [`McpServer`] so the dispatcher stays stateless.
    pub legacy_version: Option<String>,
}

impl RequestContext {
    /// Context for a validated modern (2026-07-28) request.
    pub fn modern(meta: RequestMeta) -> Self {
        Self {
            era: Era::Modern,
            meta: Some(meta),
            legacy_version: None,
        }
    }

    /// Context for a legacy request, with no `initialize` negotiation.
    pub fn legacy() -> Self {
        Self {
            era: Era::Legacy,
            meta: None,
            legacy_version: None,
        }
    }

    /// Context for a legacy `initialize` echoing `version`.
    pub fn legacy_initialize(version: impl Into<String>) -> Self {
        Self {
            era: Era::Legacy,
            meta: None,
            legacy_version: Some(version.into()),
        }
    }
}

/// A successful dispatch: the method-specific payload plus whether it may
/// be cached.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The result payload. MUST serialize to a JSON object.
    pub value: serde_json::Value,
    /// SEP-2549 caching hints, when the result is cacheable.
    pub cache: Option<CacheHints>,
}

impl Outcome {
    /// A result with no caching hints.
    pub fn plain(value: serde_json::Value) -> Self {
        Self { value, cache: None }
    }

    /// A result the client may cache for `hints.ttl_ms`.
    pub fn cacheable(value: serde_json::Value, hints: CacheHints) -> Self {
        Self {
            value,
            cache: Some(hints),
        }
    }
}

// ── McpDispatcher ───────────────────────────────────────────────────

/// Stateless MCP method dispatcher.
///
/// Takes `&self` only. Modern HTTP requests call it directly, with no
/// mutex, so concurrent clients do not serialize behind one lock.
pub struct McpDispatcher {
    handler: Arc<dyn McpHandler>,
}

impl McpDispatcher {
    /// Create a dispatcher over the given domain handler.
    pub fn new(handler: Arc<dyn McpHandler>) -> Self {
        Self { handler }
    }

    /// Borrow the underlying domain handler.
    pub fn handler(&self) -> &Arc<dyn McpHandler> {
        &self.handler
    }

    /// Dispatch one JSON-RPC message.
    ///
    /// Returns `Some(response)` for requests, `None` for notifications.
    /// Every success is wrapped in a [`ResultEnvelope`] at the single `Ok`
    /// arm below, so no result type can forget `resultType` or `_meta`.
    pub async fn dispatch(
        &self,
        ctx: &RequestContext,
        msg: JsonRpcMessage,
    ) -> Option<JsonRpcResponse> {
        let id = match msg.id.clone() {
            Some(id) => id,
            None => {
                self.handle_notification(&msg.method);
                return None;
            }
        };

        let method = msg.method.as_str();
        let result = match (ctx.era, method) {
            // Era-neutral: answered identically on both paths.
            (_, "server/discover") => self.handle_discover(),

            // Removed by 2026-07-28 (SEP-2575): no handshake, no ping.
            (Era::Modern, "initialize") | (Era::Modern, "ping") => {
                Err(DispatchError::MethodNotFound(method.to_string()))
            }

            (Era::Legacy, "initialize") => self.handle_initialize(ctx, msg.params),
            (Era::Legacy, "ping") => Ok(Outcome::plain(serde_json::json!({}))),

            (_, "tools/list") => self.handle_tools_list(),
            (_, "tools/call") => self.handle_tools_call(ctx, msg.params).await,
            (_, "resources/list") => self.handle_resources_list(),
            (_, "resources/templates/list") => self.handle_resource_templates_list(),
            (_, "resources/read") => self.handle_resources_read(msg.params).await,

            (_, other) => Err(DispatchError::MethodNotFound(other.to_string())),
        };

        Some(match result {
            Ok(outcome) => {
                let mut env = ResultEnvelope::complete(outcome.value);
                if let Some(hints) = outcome.cache {
                    env = env.with_cache(hints);
                }
                match serde_json::to_value(env) {
                    Ok(value) => JsonRpcResponse::success(id, value),
                    Err(e) => JsonRpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("Serialization error: {e}"),
                    ),
                }
            }
            Err(e) => JsonRpcResponse::error_with_data(id, e.code(), e.to_string(), e.data()),
        })
    }

    /// Handle a notification (no response expected).
    fn handle_notification(&self, method: &str) {
        match method {
            "notifications/initialized" => {
                tracing::info!("Client confirmed initialization");
            }
            "notifications/cancelled" => {
                tracing::debug!("Client cancelled a request");
            }
            other => {
                tracing::debug!(method = other, "Unknown notification, ignoring");
            }
        }
    }

    /// `server/discover` -- the 2026-07-28 replacement for `initialize`.
    ///
    /// Answered on both eras and, deliberately, without requiring `_meta`;
    /// see [`is_handshake_exempt_method`] for the reasoning.
    fn handle_discover(&self) -> Result<Outcome, DispatchError> {
        let result = DiscoverResult {
            supported_versions: SUPPORTED_MODERN_VERSIONS
                .iter()
                .map(|v| (*v).to_string())
                .collect(),
            capabilities: server_capabilities(),
            instructions: Some(SERVER_INSTRUCTIONS.to_string()),
        };
        Ok(Outcome::cacheable(
            serde_json::to_value(result)?,
            CacheHints::public(CATALOG_TTL_MS),
        ))
    }

    fn handle_initialize(
        &self,
        ctx: &RequestContext,
        params: Option<serde_json::Value>,
    ) -> Result<Outcome, DispatchError> {
        let params: InitializeParams = params
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| DispatchError::InvalidParams(e.to_string()))?
            .unwrap_or_else(|| InitializeParams {
                protocol_version: LEGACY_PROTOCOL_VERSION.to_string(),
                capabilities: ClientCapabilities::default(),
                client_info: Implementation {
                    name: "unknown".to_string(),
                    version: "0.0.0".to_string(),
                },
            });

        tracing::info!(
            client = %params.client_info.name,
            version = %params.client_info.version,
            protocol = %params.protocol_version,
            "MCP client initializing (legacy era)"
        );

        // NEVER PROTOCOL_VERSION. Echoing 2026-07-28 here would advertise a
        // revision in which `initialize` does not exist.
        let negotiated = ctx
            .legacy_version
            .clone()
            .unwrap_or_else(|| negotiate_legacy_version(&params.protocol_version).to_string());

        let result = InitializeResult {
            protocol_version: negotiated,
            capabilities: server_capabilities(),
            server_info: Implementation {
                name: SERVER_NAME.to_string(),
                version: SERVER_VERSION.to_string(),
            },
            instructions: Some(SERVER_INSTRUCTIONS.to_string()),
        };

        Ok(Outcome::plain(serde_json::to_value(result)?))
    }

    fn handle_tools_list(&self) -> Result<Outcome, DispatchError> {
        let result = ToolsListResult {
            tools: self.handler.tools(),
        };
        Ok(Outcome::cacheable(
            serde_json::to_value(result)?,
            CacheHints::public(CATALOG_TTL_MS),
        ))
    }

    async fn handle_tools_call(
        &self,
        ctx: &RequestContext,
        params: Option<serde_json::Value>,
    ) -> Result<Outcome, DispatchError> {
        let params: ToolCallParams = params
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| DispatchError::InvalidParams(e.to_string()))?
            .ok_or_else(|| DispatchError::InvalidParams("Missing params".to_string()))?;

        let mut result = self.handler.call_tool(&params.name, params.arguments).await;
        // Legacy sessions include pre-structured-content protocol versions.
        // Keep their response shape unchanged until version-aware adoption.
        if ctx.era == Era::Legacy {
            result.structured_content = None;
        }
        Ok(Outcome::plain(serde_json::to_value(result)?))
    }

    fn handle_resources_list(&self) -> Result<Outcome, DispatchError> {
        let result = ResourcesListResult {
            resources: self.handler.resources(),
        };
        Ok(Outcome::cacheable(
            serde_json::to_value(result)?,
            CacheHints::public(CATALOG_TTL_MS),
        ))
    }

    fn handle_resource_templates_list(&self) -> Result<Outcome, DispatchError> {
        let result = ResourceTemplatesListResult {
            resource_templates: self.handler.resource_templates(),
        };
        Ok(Outcome::cacheable(
            serde_json::to_value(result)?,
            CacheHints::public(CATALOG_TTL_MS),
        ))
    }

    async fn handle_resources_read(
        &self,
        params: Option<serde_json::Value>,
    ) -> Result<Outcome, DispatchError> {
        let params: ResourceReadParams = params
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| DispatchError::InvalidParams(e.to_string()))?
            .ok_or_else(|| DispatchError::InvalidParams("Missing params".to_string()))?;

        let result = self
            .handler
            .read_resource(&params.uri)
            .await
            .map_err(DispatchError::Internal)?;

        // Live instance state, not a static catalog: private and
        // immediately stale.
        Ok(Outcome::cacheable(
            serde_json::to_value(result)?,
            CacheHints::private(RESOURCE_READ_TTL_MS),
        ))
    }
}

// ── McpServer ───────────────────────────────────────────────────────

/// MCP protocol server with legacy lifecycle state.
///
/// One instance per legacy connection (a stdio process, or an HTTP
/// `Mcp-Session-Id` session). Modern requests arriving here are served
/// statelessly and NEVER consult `initialized`.
pub struct McpServer {
    dispatcher: Arc<McpDispatcher>,
    initialized: bool,
    negotiated_version: Option<String>,
}

impl McpServer {
    /// Create a new MCP server with the given handler.
    pub fn new(handler: Arc<dyn McpHandler>) -> Self {
        Self::from_dispatcher(Arc::new(McpDispatcher::new(handler)))
    }

    /// Create a legacy session sharing an existing stateless dispatcher.
    pub fn from_dispatcher(dispatcher: Arc<McpDispatcher>) -> Self {
        Self {
            dispatcher,
            initialized: false,
            negotiated_version: None,
        }
    }

    /// Whether a legacy `initialize` has succeeded on this connection.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// The protocol version echoed to this legacy client, if any.
    pub fn negotiated_version(&self) -> Option<&str> {
        self.negotiated_version.as_deref()
    }

    /// Dispatch a JSON-RPC message to the appropriate handler.
    ///
    /// Returns `Some(response)` for requests, `None` for notifications.
    /// The era is decided PER MESSAGE: an open connection is not a session
    /// for a modern client.
    pub async fn handle_message(&mut self, msg: JsonRpcMessage) -> Option<JsonRpcResponse> {
        // Notifications never get a response and carry no lifecycle state,
        // so they skip era validation entirely and are simply logged.
        if msg.id.is_none() {
            return self
                .dispatcher
                .dispatch(&RequestContext::legacy(), msg)
                .await;
        }

        let ctx = match self.context_for(&msg) {
            Ok(ctx) => ctx,
            Err(resp) => return Some(*resp),
        };

        let is_legacy_initialize = ctx.era == Era::Legacy && msg.method == "initialize";
        let response = self.dispatcher.dispatch(&ctx, msg).await;

        if is_legacy_initialize && response.as_ref().is_some_and(|resp| resp.error.is_none()) {
            self.initialized = true;
            self.negotiated_version = ctx.legacy_version;
        }

        response
    }

    /// Classify a request and build its dispatch context.
    ///
    /// The error is boxed only to keep the `Result` small.
    fn context_for(&self, msg: &JsonRpcMessage) -> Result<RequestContext, Box<JsonRpcResponse>> {
        let id = msg.id.clone().unwrap_or(JsonRpcId::Null);

        match era_hint(msg) {
            EraHint::Modern => match parse_request_meta(msg.params.as_ref()) {
                Ok(meta) => Ok(RequestContext::modern(meta)),
                Err(e) => {
                    let e = DispatchError::Meta(e);
                    Err(Box::new(JsonRpcResponse::error_with_data(
                        id,
                        e.code(),
                        e.to_string(),
                        e.data(),
                    )))
                }
            },
            EraHint::LegacyHandshake => {
                let requested = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(LEGACY_PROTOCOL_VERSION);
                Ok(RequestContext::legacy_initialize(negotiate_legacy_version(
                    requested,
                )))
            }
            EraHint::Ambiguous => {
                // Era-neutral methods are answered without any handshake,
                // and an already-initialized legacy connection keeps
                // working exactly as before.
                if is_handshake_exempt_method(&msg.method) || self.initialized {
                    Ok(RequestContext::legacy())
                } else {
                    // Replaces the old -32600 "Server not initialized"
                    // gate: this message names BOTH remedies (modern
                    // `_meta`, or a legacy `initialize`).
                    let e = DispatchError::Meta(MetaError::MissingField(META_PROTOCOL_VERSION));
                    Err(Box::new(JsonRpcResponse::error_with_data(
                        id,
                        e.code(),
                        e.to_string(),
                        e.data(),
                    )))
                }
            }
        }
    }
}

// ── DispatchError ───────────────────────────────────────────────────

/// Internal dispatch errors mapped to JSON-RPC error codes.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The requested JSON-RPC method does not exist.
    #[error("Method not found: {0}")]
    MethodNotFound(String),

    /// The method parameters were invalid or missing.
    #[error("Invalid params: {0}")]
    InvalidParams(String),

    /// An internal server error occurred.
    #[error("Internal error: {0}")]
    Internal(String),

    /// A serialization or deserialization error occurred.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The modern per-request `_meta` block was missing or invalid.
    #[error(transparent)]
    Meta(#[from] MetaError),
}

impl DispatchError {
    /// Map to a JSON-RPC error code.
    pub fn code(&self) -> i32 {
        match self {
            Self::MethodNotFound(_) => METHOD_NOT_FOUND,
            Self::InvalidParams(_) => INVALID_PARAMS,
            Self::Internal(_) | Self::Serialization(_) => INTERNAL_ERROR,
            Self::Meta(e) => e.code(),
        }
    }

    /// Structured error payload, where one is defined.
    pub fn data(&self) -> Option<serde_json::Value> {
        match self {
            Self::Meta(e) => e.data(),
            _ => None,
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal in-memory handler, reused by the HTTP transport tests.
    pub(crate) struct StubHandler;

    #[async_trait]
    impl McpHandler for StubHandler {
        fn tools(&self) -> Vec<ToolInfo> {
            vec![ToolInfo {
                name: "store_memory".to_string(),
                title: None,
                description: "Store a memory".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                annotations: None,
            }]
        }

        fn resources(&self) -> Vec<ResourceInfo> {
            vec![ResourceInfo {
                uri: "recalld://health".to_string(),
                name: "health".to_string(),
                description: None,
                mime_type: Some("application/json".to_string()),
            }]
        }

        fn resource_templates(&self) -> Vec<ResourceTemplate> {
            vec![ResourceTemplate {
                uri_template: "recalld://namespaces/{name}/stats".to_string(),
                name: "namespace-stats".to_string(),
                description: None,
                mime_type: Some("application/json".to_string()),
            }]
        }

        async fn call_tool(&self, name: &str, _arguments: serde_json::Value) -> ToolCallResult {
            if name == "structured_test" {
                return ToolCallResult::json(&serde_json::json!({"count": 2})).unwrap();
            }
            ToolCallResult::text(format!("called {name}"))
        }

        async fn read_resource(&self, uri: &str) -> Result<ResourceReadResult, String> {
            Ok(ResourceReadResult {
                contents: vec![ResourceContent {
                    uri: uri.to_string(),
                    mime_type: Some("application/json".to_string()),
                    text: Some("{}".to_string()),
                }],
            })
        }
    }

    pub(crate) fn stub_handler() -> Arc<dyn McpHandler> {
        Arc::new(StubHandler)
    }

    pub(crate) fn server() -> McpServer {
        McpServer::new(stub_handler())
    }

    /// Build a request carrying a valid modern `_meta` block.
    pub(crate) fn modern_request(
        id: i64,
        method: &str,
        extra: serde_json::Value,
    ) -> JsonRpcMessage {
        let mut params = serde_json::json!({
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_INFO: { "name": "TestClient", "version": "1.0.0" },
                META_CLIENT_CAPABILITIES: {},
            }
        });
        if let (Some(dst), Some(src)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        JsonRpcMessage {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(JsonRpcId::Number(id)),
            method: method.to_string(),
            params: Some(params),
        }
    }

    pub(crate) fn plain_request(
        id: i64,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> JsonRpcMessage {
        JsonRpcMessage {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(JsonRpcId::Number(id)),
            method: method.to_string(),
            params,
        }
    }

    pub(crate) fn initialize_request(id: i64, version: &str) -> JsonRpcMessage {
        plain_request(
            id,
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": { "name": "LegacyClient", "version": "1.0.0" },
            })),
        )
    }

    fn ok(resp: &JsonRpcResponse) -> &serde_json::Value {
        assert!(
            resp.error.is_none(),
            "expected success, got {:?}",
            resp.error
        );
        resp.result.as_ref().unwrap()
    }

    // ── server/discover ─────────────────────────────────────────────

    #[tokio::test]
    async fn discover_reports_versions_capabilities_and_instructions() {
        let mut srv = server();
        let resp = srv
            .handle_message(plain_request(1, "server/discover", None))
            .await
            .unwrap();
        let result = ok(&resp);
        assert_eq!(
            result["supportedVersions"],
            serde_json::json!([PROTOCOL_VERSION])
        );
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["capabilities"]["resources"]["subscribe"], false);
        assert!(
            result["instructions"]
                .as_str()
                .unwrap()
                .contains("Recalld is an AI memory system")
        );
    }

    #[tokio::test]
    async fn discover_supported_versions_are_modern_only() {
        let mut srv = server();
        let resp = srv
            .handle_message(plain_request(1, "server/discover", None))
            .await
            .unwrap();
        let versions = ok(&resp)["supportedVersions"].clone();
        for legacy in SUPPORTED_LEGACY_VERSIONS {
            assert!(
                !versions.to_string().contains(legacy),
                "legacy version {legacy} must not appear in supportedVersions"
            );
        }
    }

    #[tokio::test]
    async fn discover_carries_public_cache_hints_and_result_type() {
        let mut srv = server();
        let resp = srv
            .handle_message(plain_request(1, "server/discover", None))
            .await
            .unwrap();
        let result = ok(&resp);
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["ttlMs"], CATALOG_TTL_MS);
        assert_eq!(result["cacheScope"], "public");
    }

    /// The stdio era probe sends `server/discover` FIRST, before it knows
    /// which era the server speaks -- so it cannot yet supply `_meta`.
    #[tokio::test]
    async fn discover_is_answered_without_meta() {
        let mut srv = server();
        let resp = srv
            .handle_message(plain_request(1, "server/discover", None))
            .await
            .unwrap();
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn discover_with_unsupported_meta_version_is_rejected() {
        let mut srv = server();
        let msg = plain_request(
            1,
            "server/discover",
            Some(serde_json::json!({
                "_meta": {
                    META_PROTOCOL_VERSION: "2027-01-01",
                    META_CLIENT_CAPABILITIES: {},
                }
            })),
        );
        let resp = srv.handle_message(msg).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(err.data.unwrap()["requested"], "2027-01-01");
    }

    // ── Modern era ──────────────────────────────────────────────────

    #[tokio::test]
    async fn modern_tools_list_needs_no_initialize() {
        let mut srv = server();
        assert!(!srv.is_initialized());
        let resp = srv
            .handle_message(modern_request(1, "tools/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(ok(&resp)["tools"][0]["name"], "store_memory");
        assert!(!srv.is_initialized(), "modern requests never initialize");
    }

    #[tokio::test]
    async fn modern_tools_call_dispatches_to_handler() {
        let mut srv = server();
        let msg = modern_request(
            1,
            "tools/call",
            serde_json::json!({ "name": "store_memory", "arguments": {} }),
        );
        let resp = srv.handle_message(msg).await.unwrap();
        assert_eq!(ok(&resp)["content"][0]["text"], "called store_memory");
    }

    #[tokio::test]
    async fn structured_results_are_modern_only_and_keep_legacy_text() {
        let mut srv = server();
        let args = serde_json::json!({"name": "structured_test", "arguments": {}});
        let modern = srv
            .handle_message(modern_request(1, "tools/call", args.clone()))
            .await
            .unwrap();
        assert_eq!(
            ok(&modern)["structuredContent"],
            serde_json::json!({"count": 2})
        );
        for version in SUPPORTED_LEGACY_VERSIONS {
            let mut legacy_server = server();
            legacy_server
                .handle_message(initialize_request(2, version))
                .await
                .unwrap();
            let legacy = legacy_server
                .handle_message(plain_request(3, "tools/call", Some(args.clone())))
                .await
                .unwrap();
            assert!(ok(&legacy).get("structuredContent").is_none());
            assert_eq!(ok(&legacy)["content"], ok(&modern)["content"]);
        }
    }

    #[tokio::test]
    async fn modern_ping_is_method_not_found() {
        let mut srv = server();
        let resp = srv
            .handle_message(modern_request(1, "ping", serde_json::json!({})))
            .await
            .unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, METHOD_NOT_FOUND);
        assert!(err.message.contains("ping"));
    }

    #[tokio::test]
    async fn modern_initialize_is_method_not_found() {
        let mut srv = server();
        let resp = srv
            .handle_message(modern_request(1, "initialize", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    // ── Legacy era ──────────────────────────────────────────────────

    #[tokio::test]
    async fn legacy_ping_before_initialize_still_returns_empty_object() {
        let mut srv = server();
        let resp = srv
            .handle_message(plain_request(1, "ping", None))
            .await
            .unwrap();
        let result = ok(&resp);
        assert_eq!(result["resultType"], "complete");
        assert!(result.get("ttlMs").is_none());
    }

    #[tokio::test]
    async fn legacy_tools_list_before_initialize_names_the_meta_key() {
        let mut srv = server();
        let resp = srv
            .handle_message(plain_request(1, "tools/list", None))
            .await
            .unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(
            err.message.contains(META_PROTOCOL_VERSION),
            "{}",
            err.message
        );
        assert!(err.message.contains("initialize"), "{}", err.message);
    }

    #[tokio::test]
    async fn legacy_initialize_echoes_the_requested_version() {
        for requested in ["2025-06-18", "2025-03-26", "2024-11-05"] {
            let mut srv = server();
            let resp = srv
                .handle_message(initialize_request(1, requested))
                .await
                .unwrap();
            assert_eq!(ok(&resp)["protocolVersion"], requested);
            assert!(srv.is_initialized());
            assert_eq!(srv.negotiated_version(), Some(requested));
        }
    }

    #[tokio::test]
    async fn legacy_initialize_falls_back_for_unknown_version() {
        let mut srv = server();
        let resp = srv
            .handle_message(initialize_request(1, "1999-01-01"))
            .await
            .unwrap();
        assert_eq!(ok(&resp)["protocolVersion"], LEGACY_PROTOCOL_VERSION);
    }

    /// THE guard against the issue's harmful premise: bumping
    /// `PROTOCOL_VERSION` must never leak into an `initialize` response,
    /// because 2026-07-28 has no `initialize`.
    #[tokio::test]
    async fn legacy_initialize_never_echoes_2026_07_28() {
        for requested in [
            "2025-06-18",
            "2025-03-26",
            "2024-11-05",
            "1999-01-01",
            PROTOCOL_VERSION,
        ] {
            let mut srv = server();
            let resp = srv
                .handle_message(initialize_request(1, requested))
                .await
                .unwrap();
            let echoed = ok(&resp)["protocolVersion"].as_str().unwrap().to_string();
            assert_ne!(
                echoed, PROTOCOL_VERSION,
                "initialize echoed the modern version for request {requested}"
            );
            assert!(SUPPORTED_LEGACY_VERSIONS.contains(&echoed.as_str()));
        }
    }

    #[tokio::test]
    async fn legacy_session_works_after_initialize() {
        let mut srv = server();
        srv.handle_message(initialize_request(1, "2025-06-18"))
            .await
            .unwrap();
        let resp = srv
            .handle_message(plain_request(2, "tools/list", None))
            .await
            .unwrap();
        assert_eq!(ok(&resp)["tools"][0]["name"], "store_memory");
    }

    // ── Envelope invariants across every method ─────────────────────

    async fn all_results() -> Vec<(&'static str, serde_json::Value)> {
        let mut out = Vec::new();
        let mut srv = server();

        // Legacy-era results.
        for (method, params) in [
            ("initialize", None),
            ("ping", None),
            ("server/discover", None),
        ] {
            let mut s = server();
            let msg = if method == "initialize" {
                initialize_request(1, "2025-06-18")
            } else {
                plain_request(1, method, params)
            };
            let resp = s.handle_message(msg).await.unwrap();
            out.push((method, resp.result.unwrap()));
        }

        // Modern-era results.
        srv.handle_message(initialize_request(1, "2025-06-18"))
            .await
            .unwrap();
        for (method, extra) in [
            ("tools/list", serde_json::json!({})),
            (
                "tools/call",
                serde_json::json!({ "name": "store_memory", "arguments": {} }),
            ),
            ("resources/list", serde_json::json!({})),
            ("resources/templates/list", serde_json::json!({})),
            (
                "resources/read",
                serde_json::json!({ "uri": "recalld://health" }),
            ),
        ] {
            let resp = srv
                .handle_message(modern_request(2, method, extra))
                .await
                .unwrap();
            out.push((method, resp.result.unwrap()));
        }
        out
    }

    #[tokio::test]
    async fn every_result_carries_result_type_complete() {
        for (method, result) in all_results().await {
            assert_eq!(result["resultType"], "complete", "method {method}");
        }
    }

    #[tokio::test]
    async fn every_result_carries_server_info_meta() {
        for (method, result) in all_results().await {
            assert_eq!(
                result["_meta"][META_SERVER_INFO]["name"], SERVER_NAME,
                "method {method}"
            );
        }
    }

    #[tokio::test]
    async fn cache_hints_match_the_spec_table() {
        let expected: &[(&str, Option<(u64, &str)>)] = &[
            ("server/discover", Some((CATALOG_TTL_MS, "public"))),
            ("initialize", None),
            ("ping", None),
            ("tools/list", Some((CATALOG_TTL_MS, "public"))),
            ("tools/call", None),
            ("resources/list", Some((CATALOG_TTL_MS, "public"))),
            ("resources/templates/list", Some((CATALOG_TTL_MS, "public"))),
            ("resources/read", Some((RESOURCE_READ_TTL_MS, "private"))),
        ];
        let results = all_results().await;
        for (method, want) in expected {
            let (_, result) = results
                .iter()
                .find(|(m, _)| m == method)
                .unwrap_or_else(|| panic!("no result captured for {method}"));
            match want {
                Some((ttl, scope)) => {
                    assert_eq!(result["ttlMs"], *ttl, "method {method}");
                    assert_eq!(result["cacheScope"], *scope, "method {method}");
                }
                None => {
                    assert!(result.get("ttlMs").is_none(), "method {method}");
                    assert!(result.get("cacheScope").is_none(), "method {method}");
                }
            }
        }
    }

    #[tokio::test]
    async fn notifications_produce_no_response() {
        let mut srv = server();
        for method in [
            "notifications/initialized",
            "notifications/cancelled",
            "notifications/unknown",
        ] {
            let msg = JsonRpcMessage {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: None,
                method: method.to_string(),
                params: None,
            };
            assert!(srv.handle_message(msg).await.is_none());
        }
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found_in_both_eras() {
        let mut srv = server();
        let resp = srv
            .handle_message(modern_request(1, "prompts/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);

        srv.handle_message(initialize_request(2, "2025-06-18"))
            .await
            .unwrap();
        let resp = srv
            .handle_message(plain_request(3, "prompts/list", None))
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }
}
