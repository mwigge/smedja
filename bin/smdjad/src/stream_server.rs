//! NDJSON turn-streaming server.
//!
//! Accepts connections on a dedicated Unix socket (`<rpc_sock>.stream`).
//! Each connection reads a single JSON request `{"task_id":"..."}`, subscribes
//! to the Bellows dispatcher for that turn, replays any buffered events, then
//! forwards live events until the turn reaches a terminal state, at which point
//! it writes a `{"type":"done",...}` or `{"type":"error",...}` line and closes.
//!
//! Wire protocol (NDJSON — one JSON object per line):
//!
//! ```text
//! {"type":"delta","text":"Hello"}
//! {"type":"tool_call","name":"Bash","input":"ls"}
//! {"type":"done","output_tok":88,"input_tok":412,"elapsed_ms":4200}
//! {"type":"error","message":"stream timed out"}
//! ```

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use smedja_bellows::{Dispatcher, TurnEvent};

/// Maximum NDJSON lines buffered per turn before the oldest are discarded.
const MAX_BUFFER_PER_TURN: usize = 2048;

/// Maximum seconds to wait for a turn to start emitting events after a stream
/// connection arrives.  If the task_id is valid but the turn has not yet fired
/// `Started`, the subscriber waits up to this duration before giving up.
const STREAM_TIMEOUT_SECS: u64 = 90;

/// Per-turn event buffer — keyed by turn_id (= task_id in smdjad).
///
/// Populated by a background subscriber task; drained by each streaming
/// connection for that turn before it switches to live Bellows events.
pub type DeltaStore = Arc<Mutex<HashMap<String, VecDeque<String>>>>;

/// Create a new empty [`DeltaStore`] and spawn the background subscriber that
/// populates it from the Bellows dispatcher.
///
/// The background task subscribes to `dispatcher` and appends NDJSON-formatted
/// event lines to the per-turn buffer.  When a turn reaches a terminal state
/// (`Completed` or `Failed`) the buffer entry is retained so late-connecting
/// stream clients can still replay it; callers should call
/// [`cleanup_turn`](cleanup_turn) after a short delay to reclaim memory.
pub fn spawn_delta_buffer(dispatcher: Arc<Dispatcher>) -> DeltaStore {
    let store: DeltaStore = Arc::new(Mutex::new(HashMap::new()));
    let store_inner = Arc::clone(&store);
    // Subscribe before spawning to avoid losing events published between
    // spawn() and the task's first await point.
    let mut rx = dispatcher.subscribe();
    tokio::spawn(async move {
        loop {
            let event = match rx.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "delta buffer lagged; some stream events lost");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let mut store = store_inner.lock().await;
            match event {
                TurnEvent::Started { ref turn_id, .. } => {
                    store.insert(turn_id.clone(), VecDeque::new());
                }
                TurnEvent::AssistantDelta {
                    ref content,
                    ref turn_id,
                    ..
                } => {
                    let tid = match turn_id {
                        Some(t) => t,
                        None => continue,
                    };
                    if let Some(buf) = store.get_mut(tid) {
                        let line = json!({"type": "delta", "text": content}).to_string();
                        if buf.len() >= MAX_BUFFER_PER_TURN {
                            buf.pop_front();
                        }
                        buf.push_back(line);
                    }
                }
                TurnEvent::ToolCalled {
                    ref tool_name,
                    ref input_summary,
                    ref turn_id,
                    ..
                } => {
                    let tid = match turn_id {
                        Some(t) => t,
                        None => continue,
                    };
                    if let Some(buf) = store.get_mut(tid) {
                        let line =
                            json!({"type": "tool_call", "name": tool_name, "input": input_summary})
                                .to_string();
                        if buf.len() >= MAX_BUFFER_PER_TURN {
                            buf.pop_front();
                        }
                        buf.push_back(line);
                    }
                }
                TurnEvent::Completed {
                    ref turn_id,
                    output_tokens,
                    ..
                } => {
                    if let Some(buf) = store.get_mut(turn_id) {
                        let line = json!({
                            "type": "done",
                            "output_tok": output_tokens,
                        })
                        .to_string();
                        buf.push_back(line);
                    }
                }
                TurnEvent::Failed {
                    ref turn_id,
                    ref reason,
                    ..
                } => {
                    if let Some(buf) = store.get_mut(turn_id) {
                        let line = json!({"type": "error", "message": reason}).to_string();
                        buf.push_back(line);
                    }
                }
            }
        }
    });
    store
}

/// Remove a turn's buffer entry once the streaming connection has closed.
pub async fn cleanup_turn(store: &DeltaStore, turn_id: &str) {
    store.lock().await.remove(turn_id);
}

/// Returns the stream socket path for a given RPC socket path.
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
        let is_terminal = event_line.contains(r#""type":"done""#)
            || event_line.contains(r#""type":"error""#);
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
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(STREAM_TIMEOUT_SECS);

    loop {
        let event = match tokio::time::timeout_at(deadline, rx.recv()).await {
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
                let msg = json!({"type":"error","message":"stream timed out"}).to_string();
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

/// Convert a [`TurnEvent`] to `(turn_id, ndjson_line, is_terminal)`.
///
/// Returns `(None, _, _)` for events where the turn_id is unknown or not
/// relevant to the caller's filter (e.g. daemon-level events).
fn turn_event_to_ndjson(
    event: &TurnEvent,
    expected_turn_id: &str,
) -> (Option<String>, String, bool) {
    match event {
        TurnEvent::AssistantDelta {
            content, turn_id, ..
        } => {
            let line = json!({"type": "delta", "text": content}).to_string();
            (turn_id.clone(), line, false)
        }
        TurnEvent::ToolCalled {
            tool_name,
            input_summary,
            turn_id,
            ..
        } => {
            let line =
                json!({"type": "tool_call", "name": tool_name, "input": input_summary}).to_string();
            (turn_id.clone(), line, false)
        }
        TurnEvent::Completed {
            turn_id,
            output_tokens,
            ..
        } => {
            if turn_id != expected_turn_id {
                return (Some(turn_id.clone()), String::new(), false);
            }
            let line = json!({"type": "done", "output_tok": output_tokens}).to_string();
            (Some(turn_id.clone()), line, true)
        }
        TurnEvent::Failed {
            turn_id, reason, ..
        } => {
            if turn_id != expected_turn_id {
                return (Some(turn_id.clone()), String::new(), false);
            }
            let line = json!({"type": "error", "message": reason}).to_string();
            (Some(turn_id.clone()), line, true)
        }
        _ => (None, String::new(), false),
    }
}

async fn write_line(
    writer: &mut (impl AsyncWriteExt + Unpin),
    line: &str,
) -> std::io::Result<()> {
    let mut buf = line.to_owned();
    buf.push('\n');
    writer.write_all(buf.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn delta_buffer_populates_on_assistant_delta() {
        let dispatcher = Arc::new(Dispatcher::new(32));
        let store = spawn_delta_buffer(Arc::clone(&dispatcher));

        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t1".into(),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        });
        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "hello".into(),
            turn_id: Some("t1".into()),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        });

        // Give the background task a moment to process.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let s = store.lock().await;
        let buf = s.get("t1").expect("buffer entry for t1");
        assert_eq!(buf.len(), 1);
        assert!(buf[0].contains("hello"), "expected delta line, got: {}", buf[0]);
    }

    #[tokio::test]
    async fn delta_buffer_caps_at_max_per_turn() {
        let dispatcher = Arc::new(Dispatcher::new(4096));
        let store = spawn_delta_buffer(Arc::clone(&dispatcher));

        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t2".into(),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        });

        for i in 0..=MAX_BUFFER_PER_TURN {
            dispatcher.publish(TurnEvent::AssistantDelta {
                content: format!("chunk-{i}"),
                turn_id: Some("t2".into()),
                conversation_id: None,
                trace_id: None,
                span_id: None,
                parent_span_id: None,
                operation_name: None,
                agent_name: None,
                status: None,
            });
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let s = store.lock().await;
        let buf = s.get("t2").expect("buffer for t2");
        assert!(
            buf.len() <= MAX_BUFFER_PER_TURN,
            "buffer must not exceed cap; got {}",
            buf.len()
        );
    }

    #[test]
    fn turn_event_to_ndjson_delta_returns_correct_type() {
        let event = TurnEvent::AssistantDelta {
            content: "hello world".into(),
            turn_id: Some("t3".into()),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t3");
        assert_eq!(tid.as_deref(), Some("t3"));
        assert!(line.contains(r#""type":"delta""#));
        assert!(line.contains("hello world"));
        assert!(!terminal);
    }

    #[test]
    fn turn_event_to_ndjson_completed_is_terminal() {
        let event = TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: "t4".into(),
            output_tokens: 42,
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t4");
        assert_eq!(tid.as_deref(), Some("t4"));
        assert!(line.contains(r#""type":"done""#));
        assert!(terminal);
    }
}
