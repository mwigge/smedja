use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use crate::{codec::Codec, router::Router, Response};

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

async fn handle_connection(stream: UnixStream, router: Arc<Router>) {
    let mut codec = Codec::new(stream);
    loop {
        let Ok(Some(req)) = codec.recv_request().await else {
            break;
        };
        let is_notification = req.is_notification();
        let id = req.id.clone();
        let result = router.dispatch(&req.method, req.params).await;
        if !is_notification {
            let resp = match result {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, e),
            };
            if codec.send_response(&resp).await.is_err() {
                break;
            }
        }
    }
}
