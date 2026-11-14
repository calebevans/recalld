use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::mcp::bridge::BridgeError;

// ── Wire types ───────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request sent to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRequest {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Request identifier for correlating responses.
    pub id: u64,
    /// RPC method name.
    pub method: String,
    /// Method parameters.
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    /// JSON-RPC version string (always "2.0").
    pub jsonrpc: String,
    /// Matching request identifier.
    pub id: u64,
    /// Success payload, if the call succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error payload, if the call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DaemonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRpcError {
    /// Numeric error code (see `ERR_*` constants).
    pub code: i32,
    /// Human-readable error description.
    pub message: String,
}

// ── Param structs ────────────────────────────────────────────────────

/// Parameters for the `find_similar` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindSimilarParams {
    /// Source memory ID.
    pub id: String,
    /// Maximum number of results.
    pub limit: usize,
    /// Minimum similarity threshold.
    pub min_score: Option<f32>,
    /// Whether to restrict results to the same namespace.
    pub same_namespace: bool,
}

/// Parameters for the `get_memory` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetMemoryParams {
    /// Memory ID to retrieve.
    pub id: String,
}

/// Parameters for the `delete_memory` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteMemoryParams {
    /// Memory ID to delete.
    pub id: String,
}

/// Parameters for the `delete_memories` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteMemoriesParams {
    /// Memory IDs to delete, in the order the caller supplied them.
    /// The reply carries one flag per id in the same order.
    pub ids: Vec<String>,
}

/// Parameters for the `reinforce_memory` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReinforceParams {
    /// Memory ID to reinforce.
    pub id: String,
    /// Quality rating (1-4).
    pub quality: u8,
}

/// Parameters for the `scan_duplicates` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDuplicatesParams {
    /// Namespace to scan.
    pub namespace: String,
    /// Similarity threshold for duplicate detection.
    pub threshold: f32,
    /// Maximum number of memories to sample.
    pub max_memories: usize,
}

/// Parameters for the `namespace_stats` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceStatsParams {
    /// Namespace name to query.
    pub name: String,
}

// ── Error codes ──────────────────────────────────────────────────────

/// RPC error code: resource not found.
pub const ERR_NOT_FOUND: i32 = -32001;
/// RPC error code: invalid input parameters.
pub const ERR_INVALID_INPUT: i32 = -32602;
/// RPC error code: storage layer failure.
pub const ERR_STORAGE: i32 = -32003;
/// RPC error code: search subsystem failure.
pub const ERR_SEARCH: i32 = -32004;
/// RPC error code: internal server error.
pub const ERR_INTERNAL: i32 = -32603;
/// RPC error code: the request or response did not fit in a protocol frame.
pub const ERR_TOO_LARGE: i32 = -32005;
/// RPC error code: the operation failed after destroying something.
pub const ERR_PARTIALLY_APPLIED: i32 = -32006;

/// RPC error code: an explicit namespace assertion failed.
pub const ERR_NAMESPACE_MISMATCH: i32 = -32007;

// ── BridgeError conversion ───────────────────────────────────────────

impl From<&BridgeError> for DaemonRpcError {
    fn from(e: &BridgeError) -> Self {
        match e {
            BridgeError::NamespaceMismatch(msg) => DaemonRpcError {
                code: ERR_NAMESPACE_MISMATCH,
                message: msg.clone(),
            },
            BridgeError::NotFound(msg) => DaemonRpcError {
                code: ERR_NOT_FOUND,
                message: msg.clone(),
            },
            BridgeError::InvalidInput(msg) => DaemonRpcError {
                code: ERR_INVALID_INPUT,
                message: msg.clone(),
            },
            BridgeError::Storage(msg) => DaemonRpcError {
                code: ERR_STORAGE,
                message: msg.clone(),
            },
            BridgeError::Search(msg) => DaemonRpcError {
                code: ERR_SEARCH,
                message: msg.clone(),
            },
            BridgeError::Internal(msg) => DaemonRpcError {
                code: ERR_INTERNAL,
                message: msg.clone(),
            },
            BridgeError::TooLarge(msg) => DaemonRpcError {
                code: ERR_TOO_LARGE,
                message: msg.clone(),
            },
            BridgeError::PartiallyApplied(msg) => DaemonRpcError {
                code: ERR_PARTIALLY_APPLIED,
                message: msg.clone(),
            },
        }
    }
}

impl DaemonRpcError {
    /// Converts this RPC error into the corresponding `BridgeError` variant.
    pub fn into_bridge_error(self) -> BridgeError {
        match self.code {
            ERR_NAMESPACE_MISMATCH => BridgeError::NamespaceMismatch(self.message),
            ERR_NOT_FOUND => BridgeError::NotFound(self.message),
            ERR_INVALID_INPUT => BridgeError::InvalidInput(self.message),
            ERR_STORAGE => BridgeError::Storage(self.message),
            ERR_SEARCH => BridgeError::Search(self.message),
            ERR_TOO_LARGE => BridgeError::TooLarge(self.message),
            ERR_PARTIALLY_APPLIED => BridgeError::PartiallyApplied(self.message),
            _ => BridgeError::Internal(self.message),
        }
    }
}

// ── DaemonResponse helpers ───────────────────────────────────────────

impl DaemonResponse {
    /// Creates a successful response with the given result payload.
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Creates an error response with the given RPC error.
    pub fn error(id: u64, error: DaemonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

// ── Framing ──────────────────────────────────────────────────────────
//
// Length-prefixed JSON: [4-byte big-endian length][JSON payload].

/// Version of the daemon framing protocol implemented by this build.
///
/// Version 1 (recalld <= 0.1.10) had no negotiation and a hard 1 MiB frame
/// limit on both sides. Version 2 raises the limit and adds recovery from
/// oversized frames. A v2 peer announces itself in the `ping` request /
/// response; a peer that says nothing is assumed to be v1 and is only ever
/// sent frames within [`LEGACY_MAX_MESSAGE_SIZE`].
pub const PROTOCOL_VERSION: u32 = 2;

/// Maximum size in bytes of a single protocol frame.
///
/// This bounds the *serialized* JSON envelope, not the content limits the
/// MCP layer enforces. The gap between the two is JSON escaping, which is
/// a pure transport artifact, so the headroom belongs here rather than in
/// [`crate::model::constants::FULL_TEXT_MAX_BYTES`].
///
/// `serde_json` expands a byte by at most 6x (a control byte becomes
/// `\u00XX`), which puts the worst-case `store_memory` request at:
///
/// ```text
/// fullText     1,048,576 x 6           = 6,291,456
/// summary          2,000 x 6           =    12,000
/// tags          64 x (128 x 6 + 3)     =    49,344
/// ent+top+emo   96 x (<=128 x 6 + 3)   <    74,016
/// namespace           64 x 6           =       384
/// scaffold                             <     1,024
///                               total  < 6,428,224  (6.13 MiB)
/// ```
///
/// 8 MiB is the smallest power of two that clears that with headroom: 2 MiB
/// is not enough for a newline-heavy 1 MiB `fullText` (`\n` alone doubles),
/// and 4 MiB sits under the worst case.
///
/// Those content limits are enforced by
/// [`validate_memory_input`](crate::model::validate_memory_input), and the
/// sum above is checked against a real serialized frame by
/// `worst_case_valid_store_request_fits_in_one_frame` in this module's
/// tests — so raising a content limit past this budget fails a test rather
/// than surfacing as a store rejected at the transport.
pub const MAX_MESSAGE_SIZE: usize = 8 * 1024 * 1024;

/// The frame limit a protocol v1 peer (recalld <= 0.1.10) enforces.
///
/// Used as the assumed limit for a peer that has not announced itself, so
/// we never hand an old peer a frame it will reject — an old peer rejects
/// the length prefix *without* draining the payload, which corrupts its
/// stream permanently.
pub const LEGACY_MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Largest declared frame length that is still worth draining.
///
/// Draining is allocation-free (it copies into [`tokio::io::sink`]), so this
/// bounds only how much garbage we are willing to *skip*, not how much we
/// allocate — hence it is much larger than [`MAX_MESSAGE_SIZE`]. Beyond it,
/// the declared length is more likely fiction than truth and the frame
/// boundary cannot be trusted, so the connection is closed instead.
pub const MAX_DRAIN_SIZE: usize = 64 * 1024 * 1024;

/// How long a peer may take to deliver an oversized frame we are draining.
///
/// Without this, a peer could declare [`MAX_DRAIN_SIZE`] and then dribble
/// bytes forever, pinning one of [`MAX_CONNECTIONS`] connection tasks.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a peer may take to deliver a frame payload it has already
/// declared with a length prefix.
///
/// The same argument as [`DRAIN_TIMEOUT`], for the frames we actually keep:
/// without it a peer could declare a full [`MAX_MESSAGE_SIZE`] frame and
/// then send nothing, pinning a connection task forever.
///
/// This bounds only the *body* of a frame whose length prefix has already
/// arrived. The wait for the next length prefix stays unbounded on purpose:
/// that is the client waiting for the daemon to finish work, and the idle
/// monitor is what reclaims genuinely idle connections.
pub const PAYLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a declared payload is allocated at a time.
///
/// A frame's length prefix is a claim, not evidence. Allocating the whole
/// declared length before a single body byte arrives would let 32 peers
/// (see [`MAX_CONNECTIONS`]) reserve 32 x [`MAX_MESSAGE_SIZE`] for free, so
/// the buffer grows in steps as bytes actually arrive. A peer that declares
/// a frame and sends nothing therefore costs one step, not one frame.
const PAYLOAD_CHUNK: usize = 64 * 1024;

/// Maximum number of concurrent client connections the daemon will accept.
/// Additional connections are dropped immediately until an existing one closes.
pub const MAX_CONNECTIONS: u32 = 32;

/// An error encountered while reading or writing a protocol frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The peer closed the connection cleanly between frames.
    #[error("connection closed by peer")]
    Closed,

    /// The outgoing message does not fit in a frame. Nothing was written.
    #[error("frame too large: {size} bytes exceeds the {limit} byte protocol limit")]
    TooLarge {
        /// Serialized size of the message.
        size: usize,
        /// Frame limit in force for this peer.
        limit: usize,
    },

    /// An oversized incoming frame was skipped. The stream is resynchronized.
    #[error(
        "discarded oversized incoming frame of {size} bytes (limit {limit}); stream resynchronized"
    )]
    OversizedDiscarded {
        /// Declared length of the discarded frame.
        size: usize,
        /// Frame limit in force for this peer.
        limit: usize,
    },

    /// A frame was read whole but its payload is not a valid message.
    /// The stream is resynchronized.
    #[error("malformed frame payload: {0}")]
    Malformed(String),

    /// The declared frame length is too large to be believed. The frame
    /// boundary is unknown, so the stream cannot be resynchronized.
    #[error("unrecoverable framing desync: declared length {size} exceeds drain limit {limit}")]
    Desync {
        /// Declared length of the frame.
        size: usize,
        /// [`MAX_DRAIN_SIZE`].
        limit: usize,
    },

    /// The peer stopped sending mid-drain. The frame boundary was never
    /// reached, so the stream cannot be resynchronized.
    #[error(
        "timed out draining an oversized frame of {size} bytes; stream cannot be resynchronized"
    )]
    DrainTimeout {
        /// Declared length of the frame being drained.
        size: usize,
    },

    /// The peer declared a frame and then failed to deliver its payload
    /// within [`PAYLOAD_TIMEOUT`]. The frame boundary was never reached, so
    /// the stream cannot be resynchronized.
    #[error("timed out reading a declared {size} byte frame; stream cannot be resynchronized")]
    PayloadTimeout {
        /// Declared length of the frame that never arrived.
        size: usize,
    },

    /// Transport failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl FrameError {
    /// True when the stream is known to sit on a frame boundary, so the
    /// connection can keep serving after this error.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::OversizedDiscarded { .. } | Self::Malformed(_))
    }
}

/// Serializes `value` and checks it against `limit` **before** any byte
/// reaches the socket.
///
/// This is what keeps an oversized message from poisoning a connection: the
/// caller gets [`FrameError::TooLarge`] with the stream untouched.
fn encode_frame<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(value).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if payload.len() > limit {
        return Err(FrameError::TooLarge {
            size: payload.len(),
            limit,
        });
    }
    Ok(payload)
}

/// Writes an already-encoded payload as a length-prefixed frame.
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), FrameError> {
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads exactly `len` bytes, growing `buf` in [`PAYLOAD_CHUNK`] steps.
///
/// Split out from [`read_frame`] so the whole body read can be wrapped in
/// one [`PAYLOAD_TIMEOUT`] without the borrow of `buf` outliving it.
async fn read_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: usize,
    buf: &mut Vec<u8>,
) -> Result<(), FrameError> {
    while buf.len() < len {
        let want = (len - buf.len()).min(PAYLOAD_CHUNK);
        let filled = buf.len();
        // `resize` grows the allocation amortized, so a genuine 8 MiB frame
        // still costs a handful of reallocations rather than one per chunk.
        buf.resize(filled + want, 0);
        match reader.read_exact(&mut buf[filled..]).await {
            Ok(_) => {}
            // EOF mid-frame: the boundary was never reached.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(FrameError::Closed);
            }
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}

/// Reads one length-prefixed frame, recovering where recovery is provable.
///
/// Once the length prefix has been read the frame boundary is known, so an
/// oversized-but-believable frame can be skipped exactly and the stream left
/// sitting on the next frame. See [`FrameError::is_recoverable`].
///
/// Both the drained and the kept path are bounded in time, and neither
/// allocates on the strength of the declared length alone: a peer that
/// declares a frame and then goes quiet costs one [`PAYLOAD_CHUNK`] and at
/// most one timeout, not a pinned connection task holding
/// [`MAX_MESSAGE_SIZE`].
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Vec<u8>, FrameError> {
    let len = match reader.read_u32().await {
        Ok(n) => n as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(FrameError::Io(e)),
    };

    if len > MAX_DRAIN_SIZE {
        return Err(FrameError::Desync {
            size: len,
            limit: MAX_DRAIN_SIZE,
        });
    }

    if len > limit {
        let mut limited = (&mut *reader).take(len as u64);
        let mut sink = tokio::io::sink();
        let drain = tokio::io::copy(&mut limited, &mut sink);
        return match tokio::time::timeout(DRAIN_TIMEOUT, drain).await {
            Ok(Ok(n)) if n == len as u64 => {
                Err(FrameError::OversizedDiscarded { size: len, limit })
            }
            // EOF before the frame boundary: nothing left to resynchronize to.
            Ok(Ok(_)) => Err(FrameError::Closed),
            Ok(Err(e)) => Err(FrameError::Io(e)),
            Err(_) => Err(FrameError::DrainTimeout { size: len }),
        };
    }

    let mut buf: Vec<u8> = Vec::new();
    match tokio::time::timeout(PAYLOAD_TIMEOUT, read_payload(reader, len, &mut buf)).await {
        Ok(Ok(())) => Ok(buf),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(FrameError::PayloadTimeout { size: len }),
    }
}

/// Reads a length-prefixed JSON-RPC request from the stream.
pub async fn read_framed_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<DaemonRequest, FrameError> {
    let buf = read_frame(reader, limit).await?;
    serde_json::from_slice(&buf).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Reads a length-prefixed JSON-RPC response from the stream.
pub async fn read_framed_response<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<DaemonResponse, FrameError> {
    let buf = read_frame(reader, limit).await?;
    serde_json::from_slice(&buf).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Writes a length-prefixed JSON-RPC response to the stream.
///
/// Returns [`FrameError::TooLarge`] without writing anything if the response
/// exceeds `limit`.
pub async fn write_framed_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &DaemonResponse,
    limit: usize,
) -> Result<(), FrameError> {
    let payload = encode_frame(response, limit)?;
    write_frame(writer, &payload).await
}

/// Writes a length-prefixed JSON-RPC request to the stream.
///
/// Returns [`FrameError::TooLarge`] without writing anything if the request
/// exceeds `limit`.
pub async fn write_framed_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request: &DaemonRequest,
    limit: usize,
) -> Result<(), FrameError> {
    let payload = encode_frame(request, limit)?;
    write_frame(writer, &payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames used in these tests are tiny, so a tiny limit keeps every
    /// assertion sub-millisecond.
    const TINY: usize = 64;
    const ROOMY: usize = 64 * 1024;

    fn request(id: u64) -> DaemonRequest {
        DaemonRequest {
            jsonrpc: "2.0".into(),
            id,
            method: "ping".into(),
            params: serde_json::json!({}),
        }
    }

    /// A request whose serialized form is far larger than [`TINY`].
    fn fat_request() -> DaemonRequest {
        DaemonRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "store_memory".into(),
            params: serde_json::json!({ "fullText": "z".repeat(500) }),
        }
    }

    // 1
    #[tokio::test]
    async fn round_trip_request() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        let sent = DaemonRequest {
            jsonrpc: "2.0".into(),
            id: 7,
            method: "get_memory".into(),
            params: serde_json::json!({ "id": "abc" }),
        };

        write_framed_request(&mut a, &sent, ROOMY).await.unwrap();
        let got = read_framed_message(&mut b, ROOMY).await.unwrap();

        assert_eq!(got.id, 7);
        assert_eq!(got.method, "get_memory");
        assert_eq!(got.params, serde_json::json!({ "id": "abc" }));
    }

    // 2
    #[tokio::test]
    async fn round_trip_response_success() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        let sent = DaemonResponse::success(3, serde_json::json!({ "ok": true }));

        write_framed_message(&mut a, &sent, ROOMY).await.unwrap();
        let got = read_framed_response(&mut b, ROOMY).await.unwrap();

        assert_eq!(got.id, 3);
        assert_eq!(got.result, Some(serde_json::json!({ "ok": true })));
        assert!(got.error.is_none());
    }

    // 2b
    #[tokio::test]
    async fn round_trip_response_error() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        let sent = DaemonResponse::error(
            9,
            DaemonRpcError {
                code: ERR_NOT_FOUND,
                message: "no such memory".into(),
            },
        );

        write_framed_message(&mut a, &sent, ROOMY).await.unwrap();
        let got = read_framed_response(&mut b, ROOMY).await.unwrap();

        let err = got.error.expect("error payload");
        assert_eq!(got.id, 9);
        assert_eq!(err.code, ERR_NOT_FOUND);
        assert_eq!(err.message, "no such memory");
        assert!(got.result.is_none());
    }

    // 3
    #[tokio::test]
    async fn three_frames_read_back_in_order() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        for id in 1..=3 {
            write_framed_request(&mut a, &request(id), ROOMY)
                .await
                .unwrap();
        }

        for id in 1..=3 {
            assert_eq!(read_framed_message(&mut b, ROOMY).await.unwrap().id, id);
        }
    }

    // 4
    #[tokio::test]
    async fn read_returns_closed_on_clean_eof() {
        let (a, mut b) = tokio::io::duplex(ROOMY);
        drop(a);

        let err = read_framed_message(&mut b, ROOMY).await.unwrap_err();
        assert!(matches!(err, FrameError::Closed), "got {err}");
        assert!(!err.is_recoverable());
    }

    // 5
    #[tokio::test]
    async fn write_rejects_oversized_request_without_writing() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);

        match write_framed_request(&mut a, &fat_request(), TINY)
            .await
            .unwrap_err()
        {
            FrameError::TooLarge { size, limit } => {
                assert!(size > TINY);
                assert_eq!(limit, TINY);
            }
            other => panic!("expected TooLarge, got {other}"),
        }

        // Nothing at all reached the peer.
        let peeked = tokio::time::timeout(Duration::from_millis(10), b.read_u8()).await;
        assert!(peeked.is_err(), "the socket must be untouched");
    }

    // 6
    #[tokio::test]
    async fn write_rejects_oversized_response_without_writing() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        let fat = DaemonResponse::success(1, serde_json::json!({ "blob": "z".repeat(500) }));

        let err = write_framed_message(&mut a, &fat, TINY).await.unwrap_err();
        assert!(
            matches!(err, FrameError::TooLarge { limit: TINY, .. }),
            "got {err}"
        );

        let peeked = tokio::time::timeout(Duration::from_millis(10), b.read_u8()).await;
        assert!(peeked.is_err(), "the socket must be untouched");
    }

    // 7 — the flagship: an oversized frame must not cost us the connection.
    #[tokio::test]
    async fn oversized_incoming_frame_is_drained_and_stream_resyncs() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);

        let junk = vec![b'x'; 200];
        a.write_u32(junk.len() as u32).await.unwrap();
        a.write_all(&junk).await.unwrap();
        write_framed_request(&mut a, &request(42), ROOMY)
            .await
            .unwrap();

        let err = read_framed_message(&mut b, TINY).await.unwrap_err();
        assert!(
            matches!(
                err,
                FrameError::OversizedDiscarded {
                    size: 200,
                    limit: TINY
                }
            ),
            "got {err}"
        );
        assert!(err.is_recoverable());

        // The stream landed exactly on the next frame boundary.
        assert_eq!(read_framed_message(&mut b, ROOMY).await.unwrap().id, 42);
    }

    // 8
    #[tokio::test]
    async fn desync_length_beyond_drain_limit_is_fatal() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        a.write_u32(MAX_DRAIN_SIZE as u32 + 1).await.unwrap();

        let err = read_framed_message(&mut b, TINY).await.unwrap_err();
        assert!(
            matches!(err, FrameError::Desync { limit, .. } if limit == MAX_DRAIN_SIZE),
            "got {err}"
        );
        assert!(!err.is_recoverable());
    }

    // 9
    #[tokio::test(start_paused = true)]
    async fn drain_times_out_when_payload_never_arrives() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        a.write_u32(200).await.unwrap();
        a.write_all(b"only ten b").await.unwrap();

        // `a` stays alive, so the read blocks rather than seeing EOF.
        let err = read_framed_message(&mut b, TINY).await.unwrap_err();
        assert!(
            matches!(err, FrameError::DrainTimeout { size: 200 }),
            "got {err}"
        );
        assert!(!err.is_recoverable());
        drop(a);
    }

    // 9b — regression: the in-limit path must be bounded in time too.
    //
    // Before the fix this path did `vec![0u8; len]` and a bare `read_exact`,
    // so a peer that declared a frame and then went quiet held a connection
    // task (and its buffer) forever.
    #[tokio::test(start_paused = true)]
    async fn declared_payload_that_never_arrives_times_out() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        a.write_u32(200).await.unwrap();
        a.write_all(b"only ten b").await.unwrap();

        // 200 is inside `ROOMY`, so this takes the kept path, not the drain.
        // The outer guard is twice `PAYLOAD_TIMEOUT`, so a regression that
        // drops the bound fails here instead of hanging the suite.
        let err = tokio::time::timeout(PAYLOAD_TIMEOUT * 2, read_framed_message(&mut b, ROOMY))
            .await
            .expect("the payload read must be bounded")
            .unwrap_err();
        assert!(
            matches!(err, FrameError::PayloadTimeout { size: 200 }),
            "got {err}"
        );
        // The boundary was never reached, so the stream cannot resynchronize.
        assert!(!err.is_recoverable());
        drop(a);
    }

    // 9c — a huge *declared* length must not be allocated up front.
    //
    // The peer never sends a byte, so the read must give up on the timeout
    // rather than reserving `MAX_MESSAGE_SIZE`. Thirty-two of these (the
    // connection cap) used to be 256 MiB of buffers held by nothing.
    #[tokio::test(start_paused = true)]
    async fn maximum_declared_length_with_no_body_times_out_without_hoarding() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        a.write_u32(MAX_MESSAGE_SIZE as u32).await.unwrap();

        let err = tokio::time::timeout(
            PAYLOAD_TIMEOUT * 2,
            read_framed_message(&mut b, MAX_MESSAGE_SIZE),
        )
        .await
        .expect("the payload read must be bounded")
        .unwrap_err();
        assert!(
            matches!(
                err,
                FrameError::PayloadTimeout {
                    size: MAX_MESSAGE_SIZE
                }
            ),
            "got {err}"
        );
        drop(a);
    }

    // 9d — a payload cut short by a hangup is a closed connection, not a
    // timeout: there is nothing left to wait for.
    #[tokio::test]
    async fn payload_truncated_by_hangup_reports_closed() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        a.write_u32(200).await.unwrap();
        a.write_all(b"partial").await.unwrap();
        drop(a);

        let err = read_framed_message(&mut b, ROOMY).await.unwrap_err();
        assert!(matches!(err, FrameError::Closed), "got {err}");
        assert!(!err.is_recoverable());
    }

    // 9e — a payload larger than one `PAYLOAD_CHUNK` still round-trips, so
    // the chunked read is not silently truncating anything.
    #[tokio::test]
    async fn multi_chunk_payload_round_trips_intact() {
        let (mut a, mut b) = tokio::io::duplex(4 * PAYLOAD_CHUNK);
        let big = DaemonRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "store_memory".into(),
            params: serde_json::json!({ "fullText": "q".repeat(3 * PAYLOAD_CHUNK) }),
        };

        let writer = tokio::spawn(async move {
            write_framed_request(&mut a, &big, MAX_MESSAGE_SIZE)
                .await
                .unwrap();
            a
        });

        let got = read_framed_message(&mut b, MAX_MESSAGE_SIZE).await.unwrap();
        let _a = writer.await.unwrap();
        assert_eq!(
            got.params["fullText"].as_str().unwrap().len(),
            3 * PAYLOAD_CHUNK
        );
    }

    // 10
    #[tokio::test]
    async fn malformed_json_is_recoverable_and_stream_resyncs() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        let junk = b"not json at all";
        a.write_u32(junk.len() as u32).await.unwrap();
        a.write_all(junk).await.unwrap();
        write_framed_request(&mut a, &request(5), ROOMY)
            .await
            .unwrap();

        let err = read_framed_message(&mut b, ROOMY).await.unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)), "got {err}");
        assert!(err.is_recoverable());

        assert_eq!(read_framed_message(&mut b, ROOMY).await.unwrap().id, 5);
    }

    // 11
    #[tokio::test]
    async fn zero_length_frame_is_malformed_not_fatal() {
        let (mut a, mut b) = tokio::io::duplex(ROOMY);
        a.write_u32(0).await.unwrap();
        write_framed_request(&mut a, &request(6), ROOMY)
            .await
            .unwrap();

        let err = read_framed_message(&mut b, ROOMY).await.unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)), "got {err}");
        assert!(err.is_recoverable());

        assert_eq!(read_framed_message(&mut b, ROOMY).await.unwrap().id, 6);
    }

    // 12
    #[test]
    fn too_large_error_message_names_size_and_limit() {
        let err = FrameError::TooLarge {
            size: 2_000_000,
            limit: 1_048_576,
        };
        let text = err.to_string();
        assert!(text.contains("2000000"), "{text}");
        assert!(text.contains("1048576"), "{text}");
    }

    // 13
    #[test]
    fn err_too_large_round_trips_through_daemon_rpc_error() {
        let original = BridgeError::too_large(9_000_000, MAX_MESSAGE_SIZE);
        let wire = DaemonRpcError::from(&original);
        assert_eq!(wire.code, ERR_TOO_LARGE);

        let back = wire.into_bridge_error();
        assert!(matches!(back, BridgeError::TooLarge(_)), "got {back}");
        assert_eq!(back.to_string(), original.to_string());
        assert!(back.to_string().contains("9000000"));
    }

    /// The worst-case *valid* `store_memory` request must fit in one
    /// frame.
    ///
    /// This is the arithmetic in [`MAX_MESSAGE_SIZE`]'s doc comment,
    /// executed instead of asserted in prose. It fails the moment
    /// anyone raises a content limit, adds an unbounded field to the
    /// store payload, or lowers the frame limit — the collision that
    /// would otherwise surface as a store rejected at the transport
    /// after the caller was told its input was within limits.
    #[test]
    fn worst_case_valid_store_request_fits_in_one_frame() {
        use crate::model::constants::{
            EMOTION_MAX_BYTES, ENTITY_MAX_BYTES, FULL_TEXT_MAX_BYTES, MAX_EMOTIONS, MAX_ENTITIES,
            MAX_TAGS, MAX_TOPICS, NAMESPACE_NAME_MAX_BYTES, SUMMARY_MAX_BYTES, TAG_MAX_BYTES,
            TOPIC_MAX_BYTES,
        };
        use crate::model::validation::{MemoryInputRef, validate_memory_input};

        // A control byte is the worst case for JSON: one byte of content
        // becomes the six characters `\u0001` on the wire. Nothing in the shared
        // validator rejects control bytes, so this is a request a client
        // can really send.
        let worst = |n: usize| "\u{1}".repeat(n);
        let many =
            |count: usize, len: usize| -> Vec<String> { (0..count).map(|_| worst(len)).collect() };

        let summary = worst(SUMMARY_MAX_BYTES);
        let full_text = worst(FULL_TEXT_MAX_BYTES);
        let tags = many(MAX_TAGS, TAG_MAX_BYTES);
        let entities = many(MAX_ENTITIES, ENTITY_MAX_BYTES);
        let topics = many(MAX_TOPICS, TOPIC_MAX_BYTES);
        let emotions = many(MAX_EMOTIONS, EMOTION_MAX_BYTES);

        // Everything above is at, not over, the limits the entry points
        // enforce — otherwise this would be measuring a request that can
        // never be sent.
        validate_memory_input(MemoryInputRef {
            summary: &summary,
            full_text: Some(&full_text),
            tags: &tags,
            entities: &entities,
            topics: &topics,
            emotions: &emotions,
        })
        .expect("the worst case must itself be a valid request");

        let request = DaemonRequest {
            jsonrpc: "2.0".into(),
            id: u64::MAX,
            method: "store_memory".into(),
            params: serde_json::json!({
                "summary": summary,
                "fullText": full_text,
                "tags": tags,
                "entities": entities,
                "topics": topics,
                "emotions": emotions,
                "namespace": "n".repeat(NAMESPACE_NAME_MAX_BYTES),
                "parentId": "0195c9a4-1f4b-7c3a-9d21-4b7e5f6a8c90",
                "supersedes": "0195c9a4-1f4b-7c3a-9d21-4b7e5f6a8c91",
            }),
        };

        let encoded = serde_json::to_vec(&request).expect("request serializes");
        assert!(
            encoded.len() <= MAX_MESSAGE_SIZE,
            "worst-case valid store request serializes to {} bytes, over the \
             {MAX_MESSAGE_SIZE}-byte frame limit",
            encoded.len()
        );
    }

    #[test]
    fn frame_limits_leave_headroom_over_the_content_limit() {
        // The transport must be able to carry a maximum-size `fullText`
        // after worst-case JSON escaping (one byte -> `\u00XX`).
        const _: () = assert!(MAX_MESSAGE_SIZE >= crate::model::constants::FULL_TEXT_MAX_BYTES * 6);
        // Skipping garbage costs no memory, so the drain bound is looser.
        const _: () = assert!(MAX_DRAIN_SIZE > MAX_MESSAGE_SIZE);
        const _: () = assert!(LEGACY_MAX_MESSAGE_SIZE < MAX_MESSAGE_SIZE);
    }
}
