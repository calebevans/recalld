//! JSON-RPC 2.0 and MCP message types.
//!
//! All wire-format types for the Model Context Protocol, including
//! lifecycle, tool, and resource messages.
//!
//! Recalld is a **dual-era** MCP server. Two protocol eras share this
//! module and both transports:
//!
//! * **Modern** (MCP `2026-07-28`) is stateless. There is no
//!   `initialize` handshake and no session; every request carries its
//!   own protocol version, client info and client capabilities in
//!   `params._meta`, and every result carries `resultType`, caching
//!   hints and `_meta.io.modelcontextprotocol/serverInfo`.
//! * **Legacy** (MCP `2025-06-18` and earlier) keeps the `initialize`
//!   handshake and, over HTTP, the `Mcp-Session-Id` session.
//!
//! The era is decided **per message** by [`era_hint`].

use serde::{Deserialize, Serialize};

// ── Constants ───────────────────────────────────────────────────────

/// MCP protocol version this server implements natively (modern era).
///
/// This value is used for the stateless 2026-07-28 path only: it is the
/// sole version accepted in `_meta.io.modelcontextprotocol/protocolVersion`
/// and the value expected in the `MCP-Protocol-Version` header.
///
/// It is deliberately **never** echoed from an `initialize` response --
/// see [`negotiate_legacy_version`].
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Modern protocol versions this server accepts on the stateless path.
///
/// Advertised by `server/discover` as `supportedVersions` and returned in
/// the `data.supported` array of an [`UNSUPPORTED_PROTOCOL_VERSION`] error.
///
/// This list is intentionally MODERN-ONLY and is **not** the dual-era
/// union: the field feeds a client's fall-forward retry loop, so listing
/// `2025-06-18` here would invite a modern client to retry with
/// `_meta.protocolVersion = "2025-06-18"` on the stateless path -- which we
/// would then have to reject, contradicting our own advertisement. Legacy
/// support stays discoverable the legacy way (send `initialize`).
///
/// NOTE (flagged inference): the spec does not state how a dual-era server
/// should populate `supportedVersions`; this is inferred from the field's
/// role in the -32022 retry loop.
pub const SUPPORTED_MODERN_VERSIONS: &[&str] = &[PROTOCOL_VERSION];

/// Legacy protocol versions accepted -- and echoed -- by `initialize`.
///
/// NOTE (flagged): `2025-11-25` is deliberately absent. Its wire changes
/// were not verified against recalld's current message shapes; adding it
/// needs a diff review first.
pub const SUPPORTED_LEGACY_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Version echoed by `initialize` when the client asks for one we do not
/// recognise.
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-06-18";

/// JSON-RPC version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Server name reported in initialize / discover responses.
pub const SERVER_NAME: &str = "recalld";

/// Server version reported in initialize / discover responses.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Modern `_meta` keys (reverse-DNS, MCP 2026-07-28) ───────────────

/// `_meta` key carrying the request's protocol version (REQUIRED).
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// `_meta` key carrying the client's name and version (optional).
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
/// `_meta` key carrying the client's capability declarations (REQUIRED).
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
/// `_meta` key carrying the client's desired log level (optional, unused).
pub const META_LOG_LEVEL: &str = "io.modelcontextprotocol/logLevel";
/// `_meta` key under which every result reports this server's identity.
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

// ── Cache hint tuning (SEP-2549) ────────────────────────────────────

/// Freshness hint for the tool/resource/template catalogs and
/// `server/discover`, in milliseconds.
///
/// Recalld's catalogs are compile-time constants, so one hour is
/// conservative rather than optimistic.
pub const CATALOG_TTL_MS: u64 = 3_600_000;

/// Freshness hint for `resources/read`, in milliseconds.
///
/// Zero (immediately stale): resource contents are live instance state,
/// not a static catalog.
pub const RESOURCE_READ_TTL_MS: u64 = 0;

/// Usage instructions surfaced by both `initialize` and `server/discover`.
pub const SERVER_INSTRUCTIONS: &str = "Recalld is an AI memory system with human-like forgetting. \
     Use store_memory to save observations, recall_memories to \
     search by semantic similarity, and reinforce_memory to \
     strengthen useful memories. Memories decay naturally over \
     time unless reinforced.";

// ── JSON-RPC error codes ────────────────────────────────────────────

/// JSON-RPC parse error code.
pub const PARSE_ERROR: i32 = -32700;
/// JSON-RPC invalid request error code.
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC method not found error code.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC invalid params error code.
pub const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC internal error code.
pub const INTERNAL_ERROR: i32 = -32603;

// ── MCP 2026-07-28 error codes ──────────────────────────────────────

/// SEP-2243: a required MCP header is missing, or disagrees with the body.
pub const HEADER_MISMATCH: i32 = -32020;

/// SEP-2575: the server needs a client capability the client did not
/// declare, with `data.requiredCapabilities` naming them.
///
/// DEFINED BUT NEVER EMITTED. Recalld requires no client capabilities of
/// any kind, so there is no call site -- the constant exists so the error
/// space is complete and a reader does not go hunting for one.
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i32 = -32021;

/// SEP-2575: the request's protocol version is not one this server
/// supports. Carries `data.supported` and `data.requested`.
pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

// ── Core JSON-RPC Types ─────────────────────────────────────────────

/// JSON-RPC 2.0 request or notification.
///
/// Notifications have `id: None`. Requests have `id: Some(...)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcMessage {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Request ID, or `None` for notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
    /// Method name to invoke.
    pub method: String,
    /// Method parameters, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC message ID -- an integer, a string, or null.
///
/// `Null` exists so a response to an unparseable request can say "we could
/// not read your id" instead of fabricating id `0` and colliding with a
/// real in-flight request. Variant order matters for `untagged`
/// deserialization: `null` matches neither `Number` nor `String`, and
/// `Option<JsonRpcId>` still resolves an explicit `"id": null` to `None`
/// because serde tries `Option` first.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric request ID.
    Number(i64),
    /// String request ID.
    String(String),
    /// Explicit JSON `null` -- used when the request id is unknowable.
    Null,
}

/// JSON-RPC 2.0 success or error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Request ID this response corresponds to.
    pub id: JsonRpcId,
    /// Success result payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error object, present on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Additional error data, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    /// Build a success response.
    pub fn success(id: JsonRpcId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response with no `data` member.
    pub fn error(id: JsonRpcId, code: i32, message: impl Into<String>) -> Self {
        Self::error_with_data(id, code, message, None)
    }

    /// Build an error response, optionally carrying a `data` member.
    ///
    /// [`UNSUPPORTED_PROTOCOL_VERSION`] requires `data`.
    pub fn error_with_data(
        id: JsonRpcId,
        code: i32,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

// ── MCP Lifecycle Types ─────────────────────────────────────────────

/// Parameters for `initialize` request (client -> server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Requested MCP protocol version.
    pub protocol_version: String,
    /// Client capability declarations.
    pub capabilities: ClientCapabilities,
    /// Client name and version.
    pub client_info: Implementation,
}

/// Result for `initialize` response (server -> client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Negotiated MCP protocol version.
    pub protocol_version: String,
    /// Server capability declarations.
    pub capabilities: ServerCapabilities,
    /// Server name and version.
    pub server_info: Implementation,
    /// Optional usage instructions for the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Name and version of a client or server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    /// Name of the client or server.
    pub name: String,
    /// Version string.
    pub version: String,
}

/// Client capabilities (we accept but don't inspect these).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Unstructured capability fields from the client.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Server capabilities declared during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Tool capabilities, if the server exposes tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    /// Resource capabilities, if the server exposes resources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
}

/// Capability flags for tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// Whether the server supports `tools/list_changed` notifications.
    #[serde(default)]
    pub list_changed: bool,
}

/// Capability flags for resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    /// Whether the server supports resource subscriptions.
    #[serde(default)]
    pub subscribe: bool,
    /// Whether the server supports `resources/list_changed` notifications.
    #[serde(default)]
    pub list_changed: bool,
}

// ── Tool Protocol Types ─────────────────────────────────────────────

/// Tool definition returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    /// Tool name used in `tools/call`.
    pub name: String,
    /// Human-readable title for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
    /// Behavioral hints for tool UIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Behavioral hints for tool UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// Whether the tool only reads data without side effects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// Whether the tool performs destructive operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Whether repeated calls with the same input are safe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// Whether the tool interacts with external systems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// Parameters for `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallParams {
    /// Name of the tool to call.
    pub name: String,
    /// Tool input arguments.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Result of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    /// Content blocks in the result.
    pub content: Vec<ContentBlock>,
    /// Set to `true` if the tool call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A content block in a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    /// Plain text content.
    Text { text: String },
}

impl ToolCallResult {
    /// Build a success result with a single text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            is_error: None,
        }
    }

    /// Build a JSON result (serialized to text).
    pub fn json(value: &impl Serialize) -> Result<Self, serde_json::Error> {
        let text = serde_json::to_string_pretty(value)?;
        Ok(Self::text(text))
    }

    /// Build an error result.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text {
                text: message.into(),
            }],
            is_error: Some(true),
        }
    }
}

// ── Resource Protocol Types ─────────────────────────────────────────

/// Resource definition returned by `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    /// Resource URI.
    pub uri: String,
    /// Human-readable resource name.
    pub name: String,
    /// Description of the resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type of the resource content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Resource template returned by `resources/templates/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    /// URI template with `{param}` placeholders.
    pub uri_template: String,
    /// Human-readable template name.
    pub name: String,
    /// Description of the resource template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type of the resource content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Parameters for `resources/read`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReadParams {
    /// URI of the resource to read.
    pub uri: String,
}

/// Result of a resource read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReadResult {
    /// Resource content blobs.
    pub contents: Vec<ResourceContent>,
}

/// A single resource content blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContent {
    /// URI identifying this content.
    pub uri: String,
    /// MIME type of the content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Text content of the resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// ── List Response Wrappers ──────────────────────────────────────────

/// Response wrapper for `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    /// List of available tools.
    pub tools: Vec<ToolInfo>,
}

/// Response wrapper for `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcesListResult {
    /// List of available resources.
    pub resources: Vec<ResourceInfo>,
}

/// Response wrapper for `resources/templates/list`.
///
/// The `camelCase` rename is load-bearing: every MCP revision names this
/// field `resourceTemplates`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplatesListResult {
    /// List of available resource templates.
    pub resource_templates: Vec<ResourceTemplate>,
}

// ── MCP 2026-07-28: result envelope (SEP-2549, SEP-2575) ────────────

/// Whether a result is final or an interim request for more input.
///
/// REQUIRED on every 2026-07-28 result. Clients speaking older revisions
/// treat its absence as `Complete`, so emitting it is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultType {
    /// A final result.
    Complete,
    /// An interim result asking the client for more input. Recalld never
    /// produces one; interim results carry no caching hints.
    InputRequired,
}

/// Who may reuse a cached result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    /// Contains no user-specific data; any shared proxy may serve it to
    /// any user.
    Public,
    /// Reusable only within the same authorization context.
    Private,
}

/// SEP-2549 caching hints, emitted at the TOP LEVEL of a result (siblings
/// of `tools`/`resources`/`contents`), never inside `_meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheHints {
    /// Freshness lifetime in milliseconds, analogous to HTTP
    /// `Cache-Control: max-age`. `0` means immediately stale. It is a
    /// hint, not a guarantee, and not a polling interval.
    pub ttl_ms: u64,
    /// Sharing scope for the cached value.
    pub cache_scope: CacheScope,
}

impl CacheHints {
    /// Hints for a result containing no user-specific data.
    pub const fn public(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            cache_scope: CacheScope::Public,
        }
    }

    /// Hints for a result reusable only within one authorization context.
    pub const fn private(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            cache_scope: CacheScope::Private,
        }
    }
}

/// The `_meta` object attached to every result.
///
/// The 2026-07-28 spec says servers SHOULD report their identity on every
/// result. `clientInfo`/`serverInfo` are explicitly not verified by the
/// protocol and must not drive behavioral or security decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMeta {
    /// This server's name and version.
    #[serde(rename = "io.modelcontextprotocol/serverInfo")]
    pub server_info: Implementation,
}

impl ResultMeta {
    /// Build the `_meta` block for this build of recalld.
    pub fn current() -> Self {
        Self {
            server_info: Implementation {
                name: SERVER_NAME.to_string(),
                version: SERVER_VERSION.to_string(),
            },
        }
    }
}

impl Default for ResultMeta {
    fn default() -> Self {
        Self::current()
    }
}

/// Wraps any result payload with the fields 2026-07-28 requires.
///
/// Applied once, at the single success arm of the dispatcher, rather than
/// as fields on each result struct -- one place to get right, and no result
/// type can forget it.
///
/// The envelope is emitted on BOTH eras. In the 2025-06-18 schema `Result`
/// is `{ _meta?: {...}; [key: string]: unknown }`, so the extra top-level
/// keys are schema-legal there, not merely tolerated. If a strict
/// `additionalProperties: false` client ever appears, the escape hatch is a
/// single era conditional here.
///
/// `payload` MUST serialize to a JSON object: `serde(flatten)` over a
/// non-object fails at serialization time (surfacing as an internal error).
/// There is exactly one `flatten` here on purpose -- flattening an
/// `Option` alongside `skip_serializing_if` is a known serde footgun, so
/// the cache hints are two plain `Option` fields instead.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultEnvelope {
    /// The method-specific result body.
    #[serde(flatten)]
    pub payload: serde_json::Value,
    /// Whether this result is final.
    pub result_type: ResultType,
    /// Freshness lifetime, when the result is cacheable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// Cache sharing scope, when the result is cacheable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<CacheScope>,
    /// Server identity metadata.
    #[serde(rename = "_meta")]
    pub meta: ResultMeta,
}

impl ResultEnvelope {
    /// Wrap a final result payload, with no caching hints.
    pub fn complete(payload: serde_json::Value) -> Self {
        debug_assert!(
            payload.is_object(),
            "ResultEnvelope payload must be a JSON object (serde flatten requirement)"
        );
        Self {
            payload,
            result_type: ResultType::Complete,
            ttl_ms: None,
            cache_scope: None,
            meta: ResultMeta::current(),
        }
    }

    /// Attach SEP-2549 caching hints.
    pub fn with_cache(mut self, hints: CacheHints) -> Self {
        self.ttl_ms = Some(hints.ttl_ms);
        self.cache_scope = Some(hints.cache_scope);
        self
    }
}

// ── MCP 2026-07-28: server/discover ─────────────────────────────────

/// Result of `server/discover`, the 2026-07-28 replacement for the
/// `initialize` handshake. Servers MUST implement it.
///
/// `resultType`, `ttlMs`, `cacheScope` and `_meta` are supplied by
/// [`ResultEnvelope`], not by this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverResult {
    /// Modern protocol versions this server accepts.
    pub supported_versions: Vec<String>,
    /// Server capability declarations.
    pub capabilities: ServerCapabilities,
    /// Optional usage instructions for the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The capability set recalld advertises.
///
/// Shared by `initialize` and `server/discover` so the two cannot drift.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        tools: Some(ToolsCapability {
            list_changed: false,
        }),
        resources: Some(ResourcesCapability {
            subscribe: false,
            list_changed: false,
        }),
    }
}

// ── MCP 2026-07-28: per-request `_meta` ─────────────────────────────

/// Raw, permissive view of `params._meta`.
///
/// Every field is `Option` on purpose: it lets us emit a -32602 that NAMES
/// the missing key instead of leaking a serde error message.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawRequestMeta {
    /// The request's protocol version.
    #[serde(rename = "io.modelcontextprotocol/protocolVersion")]
    pub protocol_version: Option<String>,
    /// The client's name and version.
    #[serde(rename = "io.modelcontextprotocol/clientInfo")]
    pub client_info: Option<Implementation>,
    /// The client's capability declarations.
    #[serde(rename = "io.modelcontextprotocol/clientCapabilities")]
    pub client_capabilities: Option<ClientCapabilities>,
    /// The client's desired log level. Parsed but unused -- recalld emits
    /// no `notifications/message`.
    #[serde(rename = "io.modelcontextprotocol/logLevel")]
    pub log_level: Option<String>,
    /// Progress token for long-running requests.
    #[serde(rename = "progressToken")]
    pub progress_token: Option<serde_json::Value>,
}

/// Validated per-request metadata from a modern client.
#[derive(Debug, Clone)]
pub struct RequestMeta {
    /// The request's protocol version (already checked against
    /// [`SUPPORTED_MODERN_VERSIONS`]).
    pub protocol_version: String,
    /// The client's name and version, if it sent them.
    pub client_info: Option<Implementation>,
    /// The client's capability declarations.
    pub client_capabilities: ClientCapabilities,
    /// Progress token, if any.
    ///
    /// Parsed and carried but UNUSED: recalld sends no progress
    /// notifications. The field exists so it is visible that we saw the
    /// token rather than silently dropping it, as the pre-2026 code did.
    pub progress_token: Option<serde_json::Value>,
}

/// Failures parsing or validating `params._meta`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MetaError {
    /// A required `_meta` key was absent.
    #[error("{}", missing_field_message(.0))]
    MissingField(&'static str),

    /// `_meta` was present but not shaped as the spec requires.
    #[error("Invalid params: malformed `_meta`: {0}")]
    Malformed(String),

    /// The requested protocol version is not one we serve.
    #[error("Unsupported protocol version \"{requested}\"")]
    UnsupportedVersion {
        /// The version the client asked for.
        requested: String,
    },
}

/// Legacy clients have no way to discover the modern `_meta` requirement,
/// so the protocol-version error also names their remedy.
fn missing_field_message(field: &str) -> String {
    let hint = if field == META_PROTOCOL_VERSION {
        " (2026-07-28 clients); legacy clients must send `initialize` first"
    } else {
        ""
    };
    format!("Invalid params: missing required `_meta` field \"{field}\"{hint}")
}

impl MetaError {
    /// Map to a JSON-RPC error code.
    pub fn code(&self) -> i32 {
        match self {
            Self::MissingField(_) | Self::Malformed(_) => INVALID_PARAMS,
            Self::UnsupportedVersion { .. } => UNSUPPORTED_PROTOCOL_VERSION,
        }
    }

    /// Structured error payload, where the spec defines one.
    pub fn data(&self) -> Option<serde_json::Value> {
        match self {
            Self::UnsupportedVersion { requested } => Some(serde_json::json!({
                "supported": SUPPORTED_MODERN_VERSIONS,
                "requested": requested,
            })),
            _ => None,
        }
    }
}

/// Parse and validate a modern client's `params._meta` block.
pub fn parse_request_meta(params: Option<&serde_json::Value>) -> Result<RequestMeta, MetaError> {
    let raw_meta = params
        .and_then(|p| p.get("_meta"))
        .ok_or(MetaError::MissingField(META_PROTOCOL_VERSION))?;

    if !raw_meta.is_object() {
        return Err(MetaError::Malformed("`_meta` is not an object".to_string()));
    }

    let raw: RawRequestMeta = serde_json::from_value(raw_meta.clone())
        .map_err(|e| MetaError::Malformed(e.to_string()))?;

    let protocol_version = raw
        .protocol_version
        .ok_or(MetaError::MissingField(META_PROTOCOL_VERSION))?;

    if !SUPPORTED_MODERN_VERSIONS.contains(&protocol_version.as_str()) {
        return Err(MetaError::UnsupportedVersion {
            requested: protocol_version,
        });
    }

    let client_capabilities = raw
        .client_capabilities
        .ok_or(MetaError::MissingField(META_CLIENT_CAPABILITIES))?;

    Ok(RequestMeta {
        protocol_version,
        client_info: raw.client_info,
        client_capabilities,
        progress_token: raw.progress_token,
    })
}

// ── Era detection ───────────────────────────────────────────────────

/// What a single message suggests about which era its sender speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EraHint {
    /// Carries a modern `_meta` protocol version: serve it statelessly.
    Modern,
    /// A legacy lifecycle message (`initialize` /
    /// `notifications/initialized`).
    LegacyHandshake,
    /// Neither marker is present; the caller decides from context.
    Ambiguous,
}

/// The era a request is actually served under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    /// Stateless MCP 2026-07-28.
    Modern,
    /// Session-oriented MCP 2025-06-18 and earlier.
    Legacy,
}

/// Classify a message by era.
///
/// ORDER MATTERS: the `_meta` protocol-version key WINS over the method
/// name. A legacy client may legitimately send `params._meta` (it carries
/// `progressToken` in 2025-06-18) but never the reverse-DNS
/// `io.modelcontextprotocol/protocolVersion` key. The consequence is
/// deliberate: an `initialize` carrying modern `_meta` classifies as
/// `Modern` and is answered with -32601, because `initialize` was removed
/// in 2026-07-28.
pub fn era_hint(msg: &JsonRpcMessage) -> EraHint {
    let has_modern_meta = msg
        .params
        .as_ref()
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .is_some();

    if has_modern_meta {
        return EraHint::Modern;
    }
    if msg.method == "initialize" || msg.method == "notifications/initialized" {
        return EraHint::LegacyHandshake;
    }
    EraHint::Ambiguous
}

/// Whether a method may be dispatched without first establishing an era —
/// that is, with neither modern `_meta` nor a completed legacy
/// `initialize`.
///
/// This is a *handshake* exemption, not a promise about the answer. The two
/// members earn it for different reasons and are answered differently:
///
/// * `server/discover` is genuinely era-neutral: both eras answer it, and
///   identically. FLAGGED DEVIATION from "every request MUST carry
///   `_meta`": the stdio era probe sends `server/discover` FIRST, so
///   demanding a protocol version in order to discover which versions are
///   supported is circular -- and any error at all (the spec forbids keying
///   the fallback to one code) would push a genuinely modern client into
///   the legacy fallback.
/// * `ping` is legacy-only, and merely *tolerated* without `_meta`. It was
///   removed in 2026-07-28 (SEP-2575), so a request that does carry modern
///   `_meta` is answered `-32601` by the dispatcher; being listed here only
///   means an un-annotated `ping` is served on the legacy path (`{}`)
///   instead of being refused for a missing handshake, which is what
///   existing health-checking clients expect.
///
/// If `_meta` IS supplied on either method it is still version-validated,
/// which is exactly how a modern `ping` reaches its `-32601`.
pub fn is_handshake_exempt_method(method: &str) -> bool {
    matches!(method, "server/discover" | "ping")
}

/// Choose the version echoed by a legacy `initialize` response.
///
/// Never returns a modern version -- claiming 2026-07-28 in an `initialize`
/// response is incoherent, since that revision has no `initialize`. Echoing
/// [`PROTOCOL_VERSION`] here would tell a 2025-06-18 client it is talking to
/// a revision where the handshake it just performed does not exist, and
/// under 2025-06-18 lifecycle rules that client SHOULD then disconnect.
pub fn negotiate_legacy_version(requested: &str) -> &'static str {
    SUPPORTED_LEGACY_VERSIONS
        .iter()
        .copied()
        .find(|v| *v == requested)
        .unwrap_or(LEGACY_PROTOCOL_VERSION)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(method: &str, params: Option<serde_json::Value>) -> JsonRpcMessage {
        JsonRpcMessage {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(JsonRpcId::Number(1)),
            method: method.to_string(),
            params,
        }
    }

    fn modern_meta() -> serde_json::Value {
        serde_json::json!({
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
            }
        })
    }

    /// Every MCP revision names this field `resourceTemplates`. Without
    /// `rename_all = "camelCase"` this emitted `resource_templates`, which
    /// no compliant client can consume.
    #[test]
    fn resource_templates_list_serializes_camel_case() {
        let result = ResourceTemplatesListResult {
            resource_templates: vec![ResourceTemplate {
                uri_template: "recalld://namespaces/{name}/stats".to_string(),
                name: "namespace-stats".to_string(),
                description: None,
                mime_type: None,
            }],
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(
            json.get("resourceTemplates").is_some(),
            "expected camelCase `resourceTemplates`, got: {json}"
        );
        assert!(json.get("resource_templates").is_none());
        assert!(json["resourceTemplates"][0].get("uriTemplate").is_some());
    }

    #[test]
    fn envelope_adds_result_type_and_preserves_payload() {
        let env = ResultEnvelope::complete(serde_json::json!({ "tools": [] }));
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["resultType"], "complete");
        assert_eq!(json["tools"], serde_json::json!([]));
    }

    #[test]
    fn envelope_cache_hints_are_top_level_not_in_meta() {
        let env = ResultEnvelope::complete(serde_json::json!({ "tools": [] }))
            .with_cache(CacheHints::public(CATALOG_TTL_MS));
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["ttlMs"], CATALOG_TTL_MS);
        assert_eq!(json["cacheScope"], "public");
        assert!(json["_meta"].get("ttlMs").is_none());
        assert!(json["_meta"].get("cacheScope").is_none());
    }

    #[test]
    fn envelope_omits_cache_hints_when_absent() {
        let env = ResultEnvelope::complete(serde_json::json!({ "ok": true }));
        let json = serde_json::to_value(&env).unwrap();
        assert!(json.get("ttlMs").is_none());
        assert!(json.get("cacheScope").is_none());
    }

    #[test]
    fn envelope_meta_uses_reverse_dns_server_info_key() {
        let env = ResultEnvelope::complete(serde_json::json!({}));
        let json = serde_json::to_value(&env).unwrap();
        let info = &json["_meta"][META_SERVER_INFO];
        assert_eq!(info["name"], SERVER_NAME);
        assert_eq!(info["version"], SERVER_VERSION);
        assert!(json["_meta"].get("serverInfo").is_none());
    }

    #[test]
    fn result_type_and_cache_scope_serialize_as_spec_strings() {
        assert_eq!(
            serde_json::to_value(ResultType::Complete).unwrap(),
            "complete"
        );
        assert_eq!(
            serde_json::to_value(ResultType::InputRequired).unwrap(),
            "input_required"
        );
        assert_eq!(serde_json::to_value(CacheScope::Public).unwrap(), "public");
        assert_eq!(
            serde_json::to_value(CacheScope::Private).unwrap(),
            "private"
        );
    }

    #[test]
    fn era_hint_detects_modern_meta() {
        assert_eq!(
            era_hint(&msg("tools/list", Some(modern_meta()))),
            EraHint::Modern
        );
    }

    #[test]
    fn era_hint_detects_legacy_handshake() {
        assert_eq!(era_hint(&msg("initialize", None)), EraHint::LegacyHandshake);
        assert_eq!(
            era_hint(&msg("notifications/initialized", None)),
            EraHint::LegacyHandshake
        );
    }

    #[test]
    fn era_hint_is_ambiguous_for_plain_requests() {
        assert_eq!(era_hint(&msg("tools/list", None)), EraHint::Ambiguous);
        // A legacy client may send `_meta` for progressToken; that alone is
        // not a modern marker.
        let legacy_meta = serde_json::json!({ "_meta": { "progressToken": 7 } });
        assert_eq!(
            era_hint(&msg("tools/list", Some(legacy_meta))),
            EraHint::Ambiguous
        );
    }

    #[test]
    fn era_hint_meta_key_wins_over_initialize_method() {
        // `initialize` was removed in 2026-07-28: a message carrying modern
        // `_meta` is modern regardless of its method name.
        assert_eq!(
            era_hint(&msg("initialize", Some(modern_meta()))),
            EraHint::Modern
        );
    }

    #[test]
    fn parse_meta_missing_protocol_version_names_the_key() {
        let err = parse_request_meta(None).unwrap_err();
        assert_eq!(err.code(), INVALID_PARAMS);
        let text = err.to_string();
        assert!(text.contains(META_PROTOCOL_VERSION), "{text}");
        assert!(text.contains("initialize"), "{text}");

        let params = serde_json::json!({ "_meta": { META_CLIENT_CAPABILITIES: {} } });
        let err = parse_request_meta(Some(&params)).unwrap_err();
        assert!(err.to_string().contains(META_PROTOCOL_VERSION));
    }

    #[test]
    fn parse_meta_missing_client_capabilities_is_invalid_params() {
        let params = serde_json::json!({
            "_meta": { META_PROTOCOL_VERSION: PROTOCOL_VERSION }
        });
        let err = parse_request_meta(Some(&params)).unwrap_err();
        assert_eq!(err.code(), INVALID_PARAMS);
        assert!(err.to_string().contains(META_CLIENT_CAPABILITIES));
        assert!(err.data().is_none());
    }

    #[test]
    fn parse_meta_unsupported_version_reports_supported_and_requested() {
        let params = serde_json::json!({
            "_meta": {
                META_PROTOCOL_VERSION: "2027-01-01",
                META_CLIENT_CAPABILITIES: {},
            }
        });
        let err = parse_request_meta(Some(&params)).unwrap_err();
        assert_eq!(err.code(), UNSUPPORTED_PROTOCOL_VERSION);
        let data = err.data().unwrap();
        assert_eq!(data["requested"], "2027-01-01");
        assert_eq!(data["supported"], serde_json::json!([PROTOCOL_VERSION]));
        // supportedVersions is modern-only.
        assert!(!data["supported"].to_string().contains("2025-06-18"));
    }

    #[test]
    fn parse_meta_accepts_full_modern_block_and_carries_progress_token() {
        let params = serde_json::json!({
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_INFO: { "name": "ExampleClient", "version": "1.0.0" },
                META_CLIENT_CAPABILITIES: {},
                META_LOG_LEVEL: "info",
                "progressToken": "tok-1",
            }
        });
        let meta = parse_request_meta(Some(&params)).unwrap();
        assert_eq!(meta.protocol_version, PROTOCOL_VERSION);
        assert_eq!(meta.client_info.unwrap().name, "ExampleClient");
        assert_eq!(meta.progress_token.unwrap(), "tok-1");
    }

    #[test]
    fn json_rpc_id_null_round_trips() {
        let json = serde_json::to_value(JsonRpcId::Null).unwrap();
        assert_eq!(json, serde_json::Value::Null);
        assert_eq!(
            serde_json::from_value::<JsonRpcId>(serde_json::Value::Null).unwrap(),
            JsonRpcId::Null
        );
        // An explicit `"id": null` on a message still means "notification".
        let msg: JsonRpcMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap();
        assert!(msg.id.is_none());
        // Number and String still win over Null.
        assert_eq!(
            serde_json::from_str::<JsonRpcId>("42").unwrap(),
            JsonRpcId::Number(42)
        );
        assert_eq!(
            serde_json::from_str::<JsonRpcId>(r#""abc""#).unwrap(),
            JsonRpcId::String("abc".to_string())
        );
    }

    #[test]
    fn negotiate_legacy_version_echoes_known_and_falls_back() {
        assert_eq!(negotiate_legacy_version("2025-06-18"), "2025-06-18");
        assert_eq!(negotiate_legacy_version("2025-03-26"), "2025-03-26");
        assert_eq!(negotiate_legacy_version("2024-11-05"), "2024-11-05");
        assert_eq!(
            negotiate_legacy_version("1999-01-01"),
            LEGACY_PROTOCOL_VERSION
        );
        // Never the modern version, whatever is asked for.
        assert_ne!(negotiate_legacy_version(PROTOCOL_VERSION), PROTOCOL_VERSION);
    }

    #[test]
    fn handshake_exempt_methods_are_discover_and_ping() {
        assert!(is_handshake_exempt_method("server/discover"));
        assert!(is_handshake_exempt_method("ping"));
        assert!(!is_handshake_exempt_method("tools/list"));
        assert!(!is_handshake_exempt_method("initialize"));
    }
}
