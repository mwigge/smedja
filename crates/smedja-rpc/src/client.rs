use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::net::UnixStream;

use crate::{codec::Codec, codes, Request, RpcError};

/// Default per-request timeout. Without a bound a client that sent a request the
/// server never answers (dropped frame, wedged handler) would await forever.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// JSON-RPC 2.0 client over a Unix domain socket.
pub struct Client {
    codec: Codec,
    next_id: u64,
    timeout: Duration,
}

impl Client {
    /// Connect to a smdjad socket at `path`.
    ///
    /// The per-request timeout defaults to [`DEFAULT_REQUEST_TIMEOUT`]; adjust
    /// it with [`Client::set_timeout`].
    ///
    /// # Errors
    /// Returns an error if the socket connection fails.
    #[must_use = "check the Result; a failed connect means the socket is unavailable"]
    pub async fn connect(path: &Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self {
            codec: Codec::new(stream),
            next_id: 1,
            timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    /// Override the per-request timeout applied by [`Client::call`].
    pub fn set_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    /// Send a request and return the result value, or an `RpcError` from the server.
    ///
    /// # Errors
    /// Returns a transport error (wrapped as `INTERNAL_ERROR` or `SERVER_DISCONNECTED`)
    /// or the server's `RpcError`.  When the underlying stream returns EOF or a
    /// connection-reset the error code is `SERVER_DISCONNECTED` so callers can
    /// distinguish "smdjad died" from an actual server fault.  If no response
    /// arrives within the configured timeout the error code is `TIMEOUT` rather
    /// than blocking forever.
    #[must_use = "check the Result; transport failures and server errors are both returned as Err"]
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id.cast_signed(), method, params);
        let call = tokio::time::timeout(self.timeout, self.codec.call(&req));
        let resp = match call.await {
            Err(_elapsed) => {
                return Err(RpcError::new(
                    codes::TIMEOUT,
                    format!("request timed out after {:?}", self.timeout),
                ));
            }
            Ok(inner) => inner,
        }
        .map_err(|e| {
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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::net::UnixListener;

    use super::*;

    #[tokio::test]
    async fn call_times_out_when_server_never_replies() {
        let sock = std::env::temp_dir().join(format!(
            "smedja-rpc-client-timeout-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        // Accept the connection but never send a response.
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            // Hold the connection open (no reply) longer than the client timeout.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let mut client = Client::connect(&sock).await.unwrap();
        client.set_timeout(Duration::from_millis(200));

        // Outer bound so the test cannot hang even if the timeout regressed.
        let result = tokio::time::timeout(Duration::from_secs(5), client.call("ping", json!({})))
            .await
            .expect("call must return a Timeout error, not hang");

        let err = result.expect_err("a call with no reply must be an Err");
        assert_eq!(err.code, codes::TIMEOUT);
    }
}
