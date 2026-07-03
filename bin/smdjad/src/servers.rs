//! Sibling socket servers spawned alongside the main RPC listener:
//! the ACP HTTP server, the NDJSON turn-stream server, and the agent-event
//! server.

use std::path::Path;
use std::sync::Arc;

use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_vault::Vault;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::embedder_port::Embedder;
use crate::paths::{write_acp_secret, SocketGuard};
use crate::{acp, agent_server, stream_server};

/// Starts the ACP HTTP server when `SMEDJA_ACP_PORT` is set.
///
/// Binds before spawning so a port conflict fails at startup rather than inside
/// the task. A no-op (returns `Ok`) when the env var is absent or unparseable.
pub(crate) async fn spawn_acp_server(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    workspace_root: std::path::PathBuf,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn Embedder>,
) -> anyhow::Result<()> {
    let Ok(port_str) = std::env::var("SMEDJA_ACP_PORT") else {
        return Ok(());
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return Ok(());
    };

    // Generate a one-time auth token and write it to the runtime secret file.
    let acp_token = uuid::Uuid::new_v4().to_string();
    write_acp_secret(&acp_token);
    let acp_state = acp::AcpState {
        ingot,
        dispatcher,
        auth_token: acp_token,
        workspace: workspace_root,
        vault,
        embedder,
        replay: std::sync::Arc::new(acp::EventBuffer::new()),
        next_seq: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
    };
    let acp_router = acp::build_acp_router(acp_state);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    // Bind before spawning so a port conflict fails at startup, not inside the task.
    let tcp_listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("ACP bind failed on {addr}: {e}"))?;
    info!(%addr, "ACP HTTP server listening");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(tcp_listener, acp_router).await {
            tracing::error!(error = %e, "ACP server error");
        }
    });
    Ok(())
}

/// Binds the streaming NDJSON server on the sibling socket for live turn events.
///
/// Returns the [`SocketGuard`] that cleans up the socket file on shutdown, or
/// `None` when the bind fails (live streaming is then unavailable but the daemon
/// keeps running).
pub(crate) fn spawn_stream_server(
    rpc_path: &Path,
    dispatcher: &Arc<Dispatcher>,
) -> Option<SocketGuard> {
    let delta_store = stream_server::spawn_delta_buffer(dispatcher);
    let stream_sock_path = stream_server::stream_socket_path(rpc_path);
    let _ = std::fs::remove_file(&stream_sock_path);
    let guard = SocketGuard {
        path: stream_sock_path.clone(),
    };
    match UnixListener::bind(&stream_sock_path) {
        Ok(stream_listener) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(
                    &stream_sock_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            info!(path = %stream_sock_path.display(), "turn stream server listening");
            let ds = Arc::clone(&delta_store);
            let dp = Arc::clone(dispatcher);
            tokio::spawn(async move {
                stream_server::serve(stream_listener, ds, dp).await;
            });
            Some(guard)
        }
        Err(e) => {
            warn!(error = %e, "failed to bind stream socket; live streaming unavailable");
            None
        }
    }
}

/// Binds the agent-event push server on the sibling socket for live pane
/// telemetry. Returns the socket guard, or `None` when the bind fails.
pub(crate) fn spawn_agent_server(
    rpc_path: &Path,
    dispatcher: &Arc<Dispatcher>,
    ingot: &IngotHandle,
) -> Option<SocketGuard> {
    let agent_sock_path = agent_server::agent_socket_path(rpc_path);
    let _ = std::fs::remove_file(&agent_sock_path);
    let guard = SocketGuard {
        path: agent_sock_path.clone(),
    };
    match UnixListener::bind(&agent_sock_path) {
        Ok(agent_listener) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(
                    &agent_sock_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            info!(path = %agent_sock_path.display(), "agent event server listening");
            let dp = Arc::clone(dispatcher);
            let agent_ingot = ingot.clone();
            tokio::spawn(async move {
                agent_server::serve(agent_listener, dp, agent_ingot).await;
            });
            Some(guard)
        }
        Err(e) => {
            warn!(error = %e, "failed to bind agent socket");
            None
        }
    }
}
