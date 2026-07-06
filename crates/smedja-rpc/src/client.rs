use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::net::UnixStream;

use crate::{codec::Codec, codes, Request, RpcError};

/// Default per-request timeout. Without a bound a client that sent a request the
/// server never answers (dropped frame, wedged handler) would await forever.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for long, blocking request/response RPCs that drive a bounded LLM
/// exploration loop and legitimately take minutes (e.g. `audit.run`, whose
/// auditor budget is ~12 iterations / 200k tokens). The 30s
/// [`DEFAULT_REQUEST_TIMEOUT`] would kill these mid-loop, so their call sites opt
/// in via [`Client::call_with_timeout`]. Generous headroom over the auditor
/// budget; the guard still fires if the handler truly wedges.
pub const LONG_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
        let timeout = self.timeout;
        self.call_with_timeout(method, params, timeout).await
    }

    /// Send a request bounded by an explicit `timeout` instead of the client's
    /// configured default.
    ///
    /// Long, blocking RPCs that drive a bounded LLM loop (e.g. `audit.run`,
    /// minutes-long by design) would be killed by the 30s
    /// [`DEFAULT_REQUEST_TIMEOUT`]; their call sites pass
    /// [`LONG_REQUEST_TIMEOUT`] here so ordinary RPCs keep the tight default
    /// guard while long ones are allowed to finish.
    ///
    /// # Errors
    /// Same as [`Client::call`]: a transport error, the server's `RpcError`, or
    /// `TIMEOUT` if no response arrives within `timeout`.
    #[must_use = "check the Result; transport failures and server errors are both returned as Err"]
    pub async fn call_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id.cast_signed(), method, params);
        let call = tokio::time::timeout(timeout, self.codec.call(&req));
        let resp = match call.await {
            Err(_elapsed) => {
                return Err(RpcError::new(
                    codes::TIMEOUT,
                    format!("request timed out after {timeout:?}"),
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

    /// The long timeout is comfortably larger than the default so a minutes-long
    /// blocking RPC (e.g. `audit.run`) is not killed by the 30s default guard.
    #[test]
    fn long_timeout_exceeds_default() {
        assert_eq!(DEFAULT_REQUEST_TIMEOUT, Duration::from_secs(30));
        assert!(LONG_REQUEST_TIMEOUT > DEFAULT_REQUEST_TIMEOUT);
        assert!(LONG_REQUEST_TIMEOUT >= Duration::from_secs(15 * 60));
    }

    /// Binds a one-shot server that accepts a connection, reads one request, waits
    /// `reply_after`, then replies with a result echoing the request id.
    async fn spawn_slow_reply_server(sock: std::path::PathBuf, reply_after: Duration) {
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut codec = Codec::new(stream);
            let Some(req) = codec.recv_request().await.unwrap() else {
                return;
            };
            tokio::time::sleep(reply_after).await;
            let resp = crate::Response::ok(req.id, json!({ "ok": true }));
            let _ = codec.send_response(&resp).await;
        });
    }

    /// A long method uses the long timeout: a reply that lands after a delay which
    /// would blow a short guard still succeeds via `call_with_timeout` given a
    /// generous window. Same slow op as `short_timeout_kills_the_same_slow_op`.
    #[tokio::test]
    async fn call_with_timeout_allows_reply_past_a_short_guard() {
        let sock =
            std::env::temp_dir().join(format!("smedja-rpc-long-ok-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        spawn_slow_reply_server(sock.clone(), Duration::from_millis(300)).await;

        let mut client = Client::connect(&sock).await.unwrap();
        // Generous window (well past the 300ms reply) — models `LONG_REQUEST_TIMEOUT`.
        let result = client
            .call_with_timeout("audit.run", json!({}), Duration::from_secs(5))
            .await;

        let value = result.expect("a reply within the long timeout must succeed");
        assert_eq!(value["ok"], json!(true));
    }

    /// The short guard still bites: the *same* 300ms reply is killed when the
    /// timeout is tight, proving the guard is per-call and not globally removed.
    #[tokio::test]
    async fn short_timeout_kills_the_same_slow_op() {
        let sock =
            std::env::temp_dir().join(format!("smedja-rpc-short-kill-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        spawn_slow_reply_server(sock.clone(), Duration::from_millis(300)).await;

        let mut client = Client::connect(&sock).await.unwrap();
        let err = client
            .call_with_timeout("session.get", json!({}), Duration::from_millis(50))
            .await
            .expect_err("a reply past the tight guard must time out");
        assert_eq!(err.code, codes::TIMEOUT);
    }

    /// The timeout still fires for a genuinely hung long call: even opting into a
    /// longer window, a server that never replies yields `TIMEOUT` rather than
    /// blocking forever.
    #[tokio::test]
    async fn call_with_timeout_fires_on_hung_server() {
        let sock =
            std::env::temp_dir().join(format!("smedja-rpc-long-hang-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        // Reply is scheduled far past the client's window, i.e. effectively hung.
        spawn_slow_reply_server(sock.clone(), Duration::from_secs(60)).await;

        let mut client = Client::connect(&sock).await.unwrap();
        let err = client
            .call_with_timeout("audit.run", json!({}), Duration::from_millis(200))
            .await
            .expect_err("a hung call must still time out");
        assert_eq!(err.code, codes::TIMEOUT);
    }
}
