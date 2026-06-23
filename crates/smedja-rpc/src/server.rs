use std::sync::Arc;

use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use crate::codec::{read_frame, write_frame};
use crate::{router::Router, Request, Response};

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
        let Ok(req) = serde_json::from_str::<Request>(frame.trim_end()) else {
            // Malformed frame — no reliable id to correlate an error to; skip it.
            continue;
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
}
