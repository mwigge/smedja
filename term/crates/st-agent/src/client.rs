//! smdjad UDS client, socket discovery helpers, and per-pane env-var injection.

use std::io;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info};
use uuid::Uuid;

use crate::events::{ApprovalDecision, PaneEvent};

// ─────────────────────────────────────────────────────────────────────────────
// Socket discovery
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the path to the smdjad Unix domain socket.
///
/// Uses `$XDG_RUNTIME_DIR/smdjad.sock` when `XDG_RUNTIME_DIR` is set,
/// otherwise falls back to `/tmp/smdjad.sock`.
#[must_use]
pub fn smdjad_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("smdjad.sock")
    } else {
        PathBuf::from("/tmp/smdjad.sock")
    }
}

/// Returns `true` if the smdjad socket exists on the filesystem.
pub async fn socket_exists() -> bool {
    tokio::fs::metadata(smdjad_socket_path()).await.is_ok()
}

/// Returns the agent-event push socket path: `<rpc_path>.agent`.
#[must_use]
pub fn agent_socket_path(rpc_path: &Path) -> PathBuf {
    let name = rpc_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut p = rpc_path.to_path_buf();
    p.set_file_name(format!("{name}.agent"));
    p
}

// ─────────────────────────────────────────────────────────────────────────────
// Pane environment-variable injection
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the `(key, value)` pair to inject into a child process environment
/// so that the agent inside the pane can report its pane identity to smdjad.
#[must_use]
pub fn pane_env_var(pane_id: &Uuid) -> (String, String) {
    ("SMEDJA_TERM_PANE".into(), pane_id.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// SmdjadClient
// ─────────────────────────────────────────────────────────────────────────────

/// Async tokio UDS client connected to the smdjad daemon.
///
/// The protocol is newline-delimited JSON. After connecting the caller should
/// call [`subscribe_pane`](SmdjadClient::subscribe_pane) to start receiving
/// [`PaneEvent`]s via [`next_event`](SmdjadClient::next_event).
pub struct SmdjadClient {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    writer: tokio::io::WriteHalf<UnixStream>,
}

impl SmdjadClient {
    /// Opens a connection to the smdjad socket at the path returned by
    /// [`smdjad_socket_path`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket does not exist or the connection
    /// is refused.
    pub async fn connect() -> Result<Self, io::Error> {
        Self::connect_to(&smdjad_socket_path()).await
    }

    /// Opens a connection to the smdjad socket at `path`.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket does not exist or the connection
    /// is refused.
    pub async fn connect_to(path: &Path) -> Result<Self, io::Error> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, writer) = tokio::io::split(stream);
        let reader = BufReader::new(read_half);
        debug!("connected to smdjad socket");
        Ok(Self { reader, writer })
    }

    /// Opens a connection to the smdjad agent-event socket.
    ///
    /// The agent socket path is derived from [`smdjad_socket_path`] via
    /// [`agent_socket_path`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket does not exist or the connection
    /// is refused.
    pub async fn connect_agent() -> Result<Self, io::Error> {
        let path = agent_socket_path(&smdjad_socket_path());
        Self::connect_to(&path).await
    }

    /// Sends a `subscribe_pane` request for the given pane UUID.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if serialisation fails or the write to the
    /// socket fails.
    pub async fn subscribe_pane(&mut self, pane_id: &str) -> Result<(), io::Error> {
        let msg = serde_json::json!({
            "method": "subscribe_pane",
            "params": { "pane_id": pane_id }
        });
        let mut line = serde_json::to_string(&msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        debug!(pane_id, "subscribed to pane");
        Ok(())
    }

    /// Reads the next event from the smdjad stream.
    ///
    /// Returns `Ok(None)` on EOF.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if reading from the socket fails.
    pub async fn next_event(&mut self) -> Result<Option<PaneEvent>, io::Error> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return Ok(None);
        }
        debug!(line = trimmed, "received smdjad line");
        Ok(PaneEvent::from_json_line(trimmed))
    }

    /// Sends an approval decision for a pending tool call.
    ///
    /// This is an associated function, not a method: the answer goes to the
    /// main RPC socket as a `cowork.resolve` request with an id, because the
    /// agent-event socket this client is usually connected to is write-only
    /// from smdjad's side — it never reads past the initial `subscribe_pane`
    /// line. A short-lived connection is opened per call instead.
    ///
    /// Returns `Ok(true)` when the daemon resolved the gate, `Ok(false)` when
    /// the reply carried `resolved: false` (the gate was already gone —
    /// answered elsewhere or timed out — so the local prompt must be dropped,
    /// not re-pended).
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the RPC socket cannot be reached,
    /// serialisation fails, the write to the socket fails, or no well-formed
    /// reply arrives.
    pub async fn send_approval(
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<bool, io::Error> {
        Self::send_approval_to(&smdjad_socket_path(), approval_id, decision).await
    }

    /// Sends an approval decision to the smdjad RPC socket at `path`.
    ///
    /// The frame is built with [`smedja_rpc::Request::new`] (a request WITH an
    /// id) so the daemon's `{resolved}` reply comes back and the caller can
    /// tell a resolved gate apart from an already-gone one — the previous
    /// fire-and-forget `approval_response` notification forced a pessimistic
    /// re-pend on any failure, including gates that no longer existed. The
    /// params carry both `id` (the `cowork.resolve` contract) and
    /// `approval_id` (the pre-rename spelling) so a daemon mid-rename answers
    /// either way.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket cannot be reached,
    /// serialisation fails, the write to the socket fails, or the reply
    /// cannot be read or parsed.
    pub async fn send_approval_to(
        path: &Path,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<bool, io::Error> {
        let approved = decision == ApprovalDecision::Approve;
        let msg = smedja_rpc::Request::new(
            "cowork-resolve",
            "cowork.resolve",
            serde_json::json!({
                "id": approval_id,
                "approval_id": approval_id,
                "approved": approved,
            }),
        );
        let mut line = serde_json::to_string(&msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut client = Self::connect_to(path).await?;
        client.writer.write_all(line.as_bytes()).await?;
        client.writer.flush().await?;
        let mut reply = String::new();
        client.reader.read_line(&mut reply).await?;
        let v: serde_json::Value = serde_json::from_str(reply.trim_end())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(err) = v.get("error") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cowork.resolve rejected: {err}"),
            ));
        }
        let resolved = v
            .get("result")
            .and_then(|r| r.get("resolved"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        info!(approval_id, approved, resolved, "sent approval response");
        Ok(resolved)
    }
}
