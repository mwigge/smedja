use std::path::Path;

use serde_json::Value;
use tokio::net::UnixStream;

use crate::{codec::Codec, codes, Request, RpcError};

/// JSON-RPC 2.0 client over a Unix domain socket.
pub struct Client {
    codec: Codec,
    next_id: u64,
}

impl Client {
    /// Connect to a smdjad socket at `path`.
    ///
    /// # Errors
    /// Returns an error if the socket connection fails.
    #[must_use = "check the Result; a failed connect means the socket is unavailable"]
    pub async fn connect(path: &Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self {
            codec: Codec::new(stream),
            next_id: 1,
        })
    }

    /// Send a request and return the result value, or an `RpcError` from the server.
    ///
    /// # Errors
    /// Returns a transport error (wrapped as `INTERNAL_ERROR` or `SERVER_DISCONNECTED`)
    /// or the server's `RpcError`.  When the underlying stream returns EOF or a
    /// connection-reset the error code is `SERVER_DISCONNECTED` so callers can
    /// distinguish "smdjad died" from an actual server fault.
    #[must_use = "check the Result; transport failures and server errors are both returned as Err"]
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id.cast_signed(), method, params);
        let resp = self.codec.call(&req).await.map_err(|e| {
            let msg = e.to_string();
            // Detect EOF / connection-reset: the codec returns anyhow("connection closed")
            // on EOF, and IO errors for connection-reset situations.
            if msg.contains("connection closed")
                || msg.contains("connection reset")
                || msg.contains("broken pipe")
                || msg.contains("EOF")
            {
                RpcError::new(
                    codes::SERVER_DISCONNECTED,
                    "smdjad disconnected; turn result unknown",
                )
            } else {
                RpcError::new(codes::INTERNAL_ERROR, msg)
            }
        })?;
        match (resp.result, resp.error) {
            (Some(v), _) => Ok(v),
            (_, Some(e)) => Err(e),
            _ => Err(RpcError::new(codes::INTERNAL_ERROR, "empty response")),
        }
    }

    /// Send a notification (no response expected).
    ///
    /// # Errors
    /// Returns an error if the socket write fails.
    pub async fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let req = Request::notification(method, params);
        self.codec.send_request(&req).await
    }
}
