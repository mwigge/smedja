//! Stream socket listener and per-connection request handling.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use smedja_bellows::Dispatcher;

use super::buffer::{cleanup_turn, DeltaStore};
use super::convert::turn_event_to_ndjson;

/// Maximum seconds to keep a client stream open while waiting for live turn
/// events. This must be longer than the provider drain timeout so slow model
/// streams are not reported as stream transport failures.
const STREAM_TIMEOUT_SECS: u64 = 600;

/// Returns the stream socket path for a given RPC socket path.
#[must_use]
pub fn stream_socket_path(rpc_path: &Path) -> std::path::PathBuf {
    let mut p = rpc_path.as_os_str().to_owned();
    p.push(".stream");
    std::path::PathBuf::from(p)
}

/// Accept streaming connections indefinitely on `listener`.
///
/// Each accepted connection is handled in an isolated task; panics in one
/// handler do not affect others.
pub async fn serve(listener: UnixListener, store: DeltaStore, dispatcher: Arc<Dispatcher>) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(32));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "stream socket accept error");
                continue;
            }
        };
        let store = Arc::clone(&store);
        let dispatcher = Arc::clone(&dispatcher);
        let sem = Arc::clone(&semaphore);
        tokio::spawn(async move {
            let Ok(permit) = sem.acquire_owned().await else {
                let (_, mut writer) = tokio::io::split(stream);
                let msg =
                    serde_json::json!({"type":"error","message":"at_capacity"}).to_string() + "\n";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, msg.as_bytes()).await;
                return;
            };
            handle_stream_connection(stream, store, dispatcher).await;
            drop(permit);
        });
    }
}

async fn handle_stream_connection(
    stream: UnixStream,
    store: DeltaStore,
    dispatcher: Arc<Dispatcher>,
) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // Read the single request line: {"task_id":"..."}
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() || line.is_empty() {
        return;
    }
    let task_id = match serde_json::from_str::<serde_json::Value>(line.trim()) {
        Ok(v) => v["task_id"].as_str().unwrap_or("").to_owned(),
        Err(_) => return,
    };
    if task_id.is_empty() {
        return;
    }

    // Subscribe to live events BEFORE replaying the buffer, so we don't miss
    // events that arrive between draining the buffer and subscribing.
    let mut rx = dispatcher.subscribe();

    // Replay buffered events.  The buffer may already contain the terminal
    // event if the turn completed before this connection was established.
    let buffered: VecDeque<String> = {
        let s = store.lock().await;
        s.get(&task_id).cloned().unwrap_or_default()
    };

    let mut saw_terminal = false;
    for event_line in &buffered {
        let is_terminal =
            event_line.contains(r#""type":"done""#) || event_line.contains(r#""type":"error""#);
        if write_line(&mut writer, event_line).await.is_err() {
            return;
        }
        if is_terminal {
            saw_terminal = true;
            break;
        }
    }

    if saw_terminal {
        cleanup_turn(&store, &task_id).await;
        return;
    }

    // Forward live events filtered to this turn's task_id.
    //
    // This is an IDLE timeout, reset on every received event — not a cap on the
    // whole turn. A long agentic turn (a repo-wide review, many tool calls)
    // legitimately streams for minutes; only a genuinely stalled stream (no
    // event for STREAM_TIMEOUT_SECS) should error out. The overall turn budget is
    // enforced separately by the orchestrator's wall-clock cap.
    loop {
        let idle = std::time::Duration::from_secs(STREAM_TIMEOUT_SECS);
        let event = match tokio::time::timeout(idle, rx.recv()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::warn!(
                    task_id = %task_id,
                    dropped = n,
                    "stream subscriber lagged; some events skipped"
                );
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_elapsed) => {
                let msg = json!({
                    "type": "error",
                    "message": format!("stream stalled: no events for {STREAM_TIMEOUT_SECS}s")
                })
                .to_string();
                let _ = write_line(&mut writer, &msg).await;
                break;
            }
        };

        let (event_turn_id, ndjson_line, is_terminal) = turn_event_to_ndjson(&event, &task_id);

        // Skip events that belong to a different turn.
        if let Some(tid) = &event_turn_id {
            if tid != &task_id {
                continue;
            }
        } else if !is_terminal {
            continue;
        }

        if write_line(&mut writer, &ndjson_line).await.is_err() {
            break;
        }
        if is_terminal {
            break;
        }
    }

    cleanup_turn(&store, &task_id).await;
}

async fn write_line(writer: &mut (impl AsyncWriteExt + Unpin), line: &str) -> std::io::Result<()> {
    let mut buf = line.to_owned();
    buf.push('\n');
    writer.write_all(buf.as_bytes()).await?;
    writer.flush().await
}
