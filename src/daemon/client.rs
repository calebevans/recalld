use std::path::Path;

use tokio::io::BufReader;
use tokio::sync::Mutex;

use super::protocol::{DaemonRequest, read_framed_response, write_framed_request};
use crate::mcp::bridge::BridgeError;

/// Client for communicating with the Recalld daemon over a Unix socket.
pub struct DaemonClient {
    inner: Mutex<DaemonClientInner>,
}

struct DaemonClientInner {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl DaemonClient {
    /// Connects to the daemon at the given Unix socket path.
    pub async fn connect(socket_path: &Path) -> std::io::Result<Self> {
        let stream = tokio::net::UnixStream::connect(socket_path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            inner: Mutex::new(DaemonClientInner {
                reader: BufReader::new(reader),
                writer,
                next_id: 1,
            }),
        })
    }

    /// The Mutex serializes requests on this connection.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;

        let request = DaemonRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };

        write_framed_request(&mut inner.writer, &request)
            .await
            .map_err(|e| BridgeError::Internal(format!("daemon write error: {e}")))?;

        let response = read_framed_response(&mut inner.reader)
            .await
            .map_err(|e| BridgeError::Internal(format!("daemon read error: {e}")))?
            .ok_or_else(|| BridgeError::Internal("daemon connection closed".into()))?;

        if let Some(err) = response.error {
            return Err(err.into_bridge_error());
        }

        response
            .result
            .ok_or_else(|| BridgeError::Internal("empty daemon response".into()))
    }

    /// Sends a ping to verify the daemon is responsive.
    pub async fn ping(&self) -> Result<(), BridgeError> {
        self.call("ping", serde_json::json!({})).await?;
        Ok(())
    }
}
