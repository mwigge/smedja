use std::sync::Arc;

use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use serde_json::Value;

use crate::codec::{read_frame, write_frame};
use crate::{codes, router::Router, Request, Response, RpcError};

/// Maximum number of concurrently-handled connections.
///
/// The socket is 0o600 so only local processes can connect, but an
/// unconstrained accept loop would still allow a local process to exhaust
/// file descriptors and starve legitimate turn processing.
const MAX_CONNECTIONS: usize = 64;

/// Listens on a `UnixListener` and dispatches incoming requests via a `Router`.
pub struct Server {
    router: Arc<Router>,
}

impl Server {
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self {
            router: Arc::new(router),
        }
    }

    /// Accept connections indefinitely, spawning a task per connection.
    ///
    /// At most [`MAX_CONNECTIONS`] (64) connections are handled concurrently.
    /// `accept()` is called unconditionally so the OS backlog stays drained;
    /// the semaphore permit is acquired *inside* the spawned task, which means
    /// a connection that arrives when all slots are busy waits inside the task
    /// rather than blocking the accept loop.
    ///
    /// # Errors
    /// Returns an error only if `listener.accept()` itself fails fatally.
    pub async fn serve(self, listener: UnixListener) -> anyhow::Result<()> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            // Accept unconditionally so the kernel backlog stays drained under
            // high connection load.  The permit is acquired inside the task.
            let (stream, _) = listener.accept().await?;
            let router = Arc::clone(&self.router);
            let semaphore = Arc::clone(&semaphore);
            tokio::spawn(async move {
                // Acquire a slot before doing any work; this back-pressures at
                // the protocol layer rather than the socket accept layer.
                let Ok(permit) = semaphore.acquire_owned().await else {
                    return;
                };
                handle_connection(stream, router).await;
                drop(permit); // release slot when the connection closes
            });
        }
    }
}

/// Handles one connection: reads framed requests, dispatches each on its own
/// task so a slow handler does not block siblings, and funnels every response
/// through a single writer task so framing stays intact.
async fn handle_connection(stream: UnixStream, router: Arc<Router>) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // The single writer owns the write half; all responses are sent through it.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Response>(64);
    let writer = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(resp) = rx.recv().await {
            let Ok(line) = serde_json::to_string(&resp) else {
                continue;
            };
            if write_frame(&mut write_half, &line).await.is_err() {
                break;
            }
        }
    });

    loop {
        // EOF, read error, or oversized frame: stop reading this connection.
        let Ok(Some(frame)) = read_frame(&mut reader).await else {
            break;
        };
        let req = match serde_json::from_str::<Request>(frame.trim_end()) {
            Ok(req) => req,
            Err(_) => {
                // Not a valid `Request`. If the payload is JSON carrying a
                // non-null `id`, the sender is awaiting a reply, so answer with
                // an Invalid Request error rather than dropping it silently and
                // leaving the caller blocked forever. A payload with no id (or a
                // null id) is treated as a notification and skipped.
                if let Ok(value) = serde_json::from_str::<Value>(frame.trim_end()) {
                    if let Some(id) = value.get("id").filter(|v| !v.is_null()) {
                        let resp = Response::err(
                            Some(id.clone()),
                            RpcError::new(codes::INVALID_REQUEST, "invalid request"),
                        );
                        let _ = tx.send(resp).await;
                    }
                }
                continue;
            }
        };
        let is_notification = req.is_notification();
        let id = req.id.clone();
        let router = Arc::clone(&router);
        let tx = tx.clone();
        // Spawn the handler so a slow request never blocks sibling requests.
        tokio::spawn(async move {
            let result = router.dispatch(&req.method, req.params).await;
            if !is_notification {
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                let _ = tx.send(resp).await;
            }
        });
    }

    // Close the channel so the writer task finishes once in-flight responses drain.
    drop(tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Codec;
    use serde_json::json;

    #[tokio::test]
    async fn slow_handler_does_not_block_fast_sibling() {
        // A "slow" method sleeps; a "fast" method returns immediately. Sent on
        // one connection slow-then-fast, the fast response must arrive first.
        let mut router = Router::new();
        router.register("slow", |_| async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(json!("slow-done"))
        });
        router.register("fast", |_| async { Ok(json!("fast-done")) });

        let sock =
            std::env::temp_dir().join(format!("smedja-rpc-slowfast-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(Server::new(router).serve(listener));

        let client = UnixStream::connect(&sock).await.unwrap();
        let mut codec = Codec::new(client);

        // Fire slow (id 1) then fast (id 2) back-to-back.
        codec
            .send_request(&Request::new(1_i64, "slow", json!({})))
            .await
            .unwrap();
        codec
            .send_request(&Request::new(2_i64, "fast", json!({})))
            .await
            .unwrap();

        // The first response received must be the fast one (id 2).
        let first = codec.recv_response().await.unwrap().unwrap();
        assert_eq!(
            first.id,
            Some(json!(2)),
            "fast handler must respond before the slow one"
        );
        assert_eq!(first.result, Some(json!("fast-done")));

        let second = codec.recv_response().await.unwrap().unwrap();
        assert_eq!(second.id, Some(json!(1)));
    }

    #[tokio::test]
    async fn malformed_request_with_id_gets_invalid_request_error() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        // A router with no handlers is enough; the payload never reaches dispatch.
        let router = Router::new();
        let sock =
            std::env::temp_dir().join(format!("smedja-rpc-malformed-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(Server::new(router).serve(listener));

        let mut client = UnixStream::connect(&sock).await.unwrap();
        // Valid JSON, but not a valid `Request` (no `method`), carrying an id.
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"params\":{}}\n")
            .await
            .unwrap();

        // Bound the read so a regression (silent drop) fails fast instead of hanging.
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .expect("server must reply to an id-bearing malformed request, not hang")
        .unwrap();
        assert!(n > 0, "expected an error reply, got EOF/silence");

        let resp: Response = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(resp.id, Some(json!(99)));
        assert_eq!(
            resp.error.expect("must carry an error").code,
            codes::INVALID_REQUEST
        );
    }

    #[tokio::test]
    async fn malformed_notification_without_id_is_dropped() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let mut router = Router::new();
        router.register("ping", |_| async { Ok(json!("pong")) });
        let sock = std::env::temp_dir().join(format!(
            "smedja-rpc-malformed-notif-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(Server::new(router).serve(listener));

        let mut client = UnixStream::connect(&sock).await.unwrap();
        // Not a valid Request and no id: nothing to correlate a reply to.
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"params\":{}}\n")
            .await
            .unwrap();
        // A well-formed request follows; only its reply should come back.
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\",\"params\":{}}\n")
            .await
            .unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .expect("must not hang")
        .unwrap();
        let resp: Response = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(
            resp.id,
            Some(json!(7)),
            "first reply must be for the ping, not the dropped notification"
        );
        assert_eq!(resp.result, Some(json!("pong")));
    }
}
