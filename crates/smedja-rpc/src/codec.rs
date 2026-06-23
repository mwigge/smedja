use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::{Request, Response};

/// Maximum inbound frame size: 4 MiB. Prevents unbounded allocation from a
/// malicious or runaway local process sending a giant JSON payload.
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Newline-delimited JSON framing over a Unix socket.
pub struct Codec {
    stream: BufReader<UnixStream>,
}

impl Codec {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }

    /// # Errors
    /// Returns an error if serialisation or the socket write fails.
    #[must_use = "check the Result to confirm the request was written to the socket"]
    pub async fn send_request(&mut self, req: &Request) -> Result<()> {
        self.write_line(&serde_json::to_string(req)?).await
    }

    /// # Errors
    /// Returns an error if serialisation or the socket write fails.
    #[must_use = "check the Result to confirm the response was written to the socket"]
    pub async fn send_response(&mut self, resp: &Response) -> Result<()> {
        self.write_line(&serde_json::to_string(resp)?).await
    }

    /// # Errors
    /// Returns an error if the socket read or JSON deserialisation fails.
    /// Returns `Ok(None)` on EOF.
    #[must_use = "check the Result and handle the Option; None means EOF"]
    pub async fn recv_request(&mut self) -> Result<Option<Request>> {
        self.read_line_as().await
    }

    /// # Errors
    /// Returns an error if the socket read or JSON deserialisation fails.
    /// Returns `Ok(None)` on EOF.
    #[must_use = "check the Result and handle the Option; None means EOF"]
    pub async fn recv_response(&mut self) -> Result<Option<Response>> {
        self.read_line_as().await
    }

    /// Send a request and wait for the response.
    ///
    /// # Errors
    /// Returns an error if send, recv, or deserialisation fails, or if the connection closes.
    #[must_use = "check the Result; transport failures and missing responses are both Err"]
    pub async fn call(&mut self, req: &Request) -> Result<Response> {
        self.send_request(req).await?;
        self.recv_response()
            .await?
            .ok_or_else(|| anyhow::anyhow!("connection closed"))
    }

    async fn write_line(&mut self, s: &str) -> Result<()> {
        let mut buf = s.to_owned();
        buf.push('\n');
        self.stream.get_mut().write_all(buf.as_bytes()).await?;
        Ok(())
    }

    async fn read_line_as<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let mut line = String::new();
        let n = self.stream.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        if line.len() > MAX_FRAME_BYTES {
            anyhow::bail!(
                "incoming JSON-RPC frame too large: {} bytes (max {})",
                line.len(),
                MAX_FRAME_BYTES
            );
        }
        Ok(Some(serde_json::from_str(line.trim_end())?))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::net::UnixStream;

    use super::*;
    use crate::{Request, Response, RpcError as Error};

    #[tokio::test]
    async fn send_recv_request_roundtrip() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client = Codec::new(a);
        let mut server = Codec::new(b);

        let req = Request::new(1_i64, "ping", json!({"x": 1}));
        client.send_request(&req).await.unwrap();

        let got = server.recv_request().await.unwrap().unwrap();
        assert_eq!(got.method, "ping");
        assert_eq!(got.id, Some(json!(1)));
        assert_eq!(got.params["x"], 1);
    }

    #[tokio::test]
    async fn send_recv_response_roundtrip() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut server = Codec::new(a);
        let mut client = Codec::new(b);

        let resp = Response::ok(Some(json!(1)), json!("pong"));
        server.send_response(&resp).await.unwrap();

        let got = client.recv_response().await.unwrap().unwrap();
        assert_eq!(got.result, Some(json!("pong")));
        assert!(got.error.is_none());
    }

    #[tokio::test]
    async fn recv_returns_none_on_eof() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut codec = Codec::new(a);
        drop(b);
        assert!(codec.recv_request().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn call_returns_response() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client = Codec::new(a);
        let mut server = Codec::new(b);

        tokio::spawn(async move {
            let req = server.recv_request().await.unwrap().unwrap();
            let resp = Response::ok(req.id, json!("pong"));
            server.send_response(&resp).await.unwrap();
        });

        let req = Request::new(1_i64, "ping", json!({}));
        let resp = client.call(&req).await.unwrap();
        assert_eq!(resp.result, Some(json!("pong")));
    }

    #[tokio::test]
    async fn recv_rejects_frame_exceeding_max_size() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut server = Codec::new(a);
        let mut client = Codec::new(b);

        // Send a line larger than MAX_FRAME_BYTES.
        let giant = "x".repeat(MAX_FRAME_BYTES + 1) + "\n";
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            client
                .stream
                .get_mut()
                .write_all(giant.as_bytes())
                .await
                .unwrap();
        });

        let result = server.recv_request().await;
        assert!(result.is_err(), "oversized frame must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("too large"),
            "error must mention 'too large': {msg}"
        );
    }

    #[tokio::test]
    async fn call_propagates_rpc_error() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client = Codec::new(a);
        let mut server = Codec::new(b);

        tokio::spawn(async move {
            let req = server.recv_request().await.unwrap().unwrap();
            let resp = Response::err(req.id, Error::new(-32601, "method not found"));
            server.send_response(&resp).await.unwrap();
        });

        let req = Request::new(1_i64, "unknown", json!({}));
        let resp = client.call(&req).await.unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }
}
