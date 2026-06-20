use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};

use crate::{codec::Codec, router::Router, Response};

/// Listens on a `UnixListener` and dispatches incoming requests via a `Router`.
pub struct Server {
    router: Arc<Router>,
}

impl Server {
    pub fn new(router: Router) -> Self {
        Self {
            router: Arc::new(router),
        }
    }

    /// Accept connections indefinitely, spawning a task per connection.
    ///
    /// # Errors
    /// Returns an error only if `listener.accept()` itself fails fatally.
    pub async fn serve(self, listener: UnixListener) -> anyhow::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(handle_connection(stream, Arc::clone(&self.router)));
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
