//! Agent-event push server.
//!
//! Binds `<rpc_sock>.agent` and streams [`PaneEvent`] JSON lines to every
//! connected terminal pane.
//!
//! Protocol (one JSON object per line, server → client):
//!
//! ```text
//! {"type":"turn_start","params":{"session_id":"…","turn_id":"…","trace_id":null,"span_id":null}}
//! {"type":"tool_call","params":{"tool_name":"Bash","args_summary":"ls"}}
//! {"type":"stream_delta","params":{"text":"hello"}}
//! {"type":"turn_end","params":{"input_tokens":412,"output_tokens":88,"latency_ms":4200}}
//! ```
//!
//! Clients send a single subscribe line on connect, then receive events:
//!
//! ```text
//! {"method":"subscribe_pane","params":{"pane_id":"<uuid>"}}\n
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex};

use smedja_bellows::{Dispatcher, TurnEvent};

type SubList = Arc<Mutex<Vec<mpsc::Sender<String>>>>;

/// Returns the agent socket path: `<rpc_path>.agent`.
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

/// Accept connections on `listener` and fan out [`TurnEvent`]s to all panes.
///
/// Each connected pane receives every event; panes that disconnect are silently
/// removed from the subscriber list.
pub async fn serve(listener: UnixListener, dispatcher: Arc<Dispatcher>) {
    let subs: SubList = Arc::new(Mutex::new(Vec::new()));

    // Subscribe before spawning to avoid losing events published between
    // spawn() and the task's first await point.
    let mut rx = dispatcher.subscribe();
    let subs_bg = Arc::clone(&subs);
    tokio::spawn(async move {
        let mut start_times: HashMap<String, tokio::time::Instant> = HashMap::new();
        loop {
            let event = match rx.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "agent fanout lagged; events dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let line = turn_event_to_pane_json(&event, &mut start_times);
            let mut locked = subs_bg.lock().await;
            locked.retain(|tx| tx.try_send(line.clone()).is_ok());
        }
    });

    let semaphore = Arc::new(tokio::sync::Semaphore::new(32));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "agent socket accept error");
                continue;
            }
        };
        let subs = Arc::clone(&subs);
        let sem = Arc::clone(&semaphore);
        tokio::spawn(async move {
            let Ok(permit) = sem.acquire_owned().await else {
                return;
            };
            handle_connection(stream, subs).await;
            drop(permit);
        });
    }
}

async fn handle_connection(stream: UnixStream, subs: SubList) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // Read the subscribe_pane request (required but not validated further).
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() || line.is_empty() {
        return;
    }

    let (tx, mut rx) = mpsc::channel::<String>(64);
    subs.lock().await.push(tx);

    while let Some(event_line) = rx.recv().await {
        let mut buf = event_line;
        buf.push('\n');
        if writer.write_all(buf.as_bytes()).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

/// Convert a [`TurnEvent`] to a `PaneEvent` JSON line.
///
/// `start_times` is used to compute latency for `Completed` events.
fn turn_event_to_pane_json(
    event: &TurnEvent,
    start_times: &mut HashMap<String, tokio::time::Instant>,
) -> String {
    match event {
        TurnEvent::Started {
            session_id,
            turn_id,
            trace_id,
            span_id,
            ..
        } => {
            start_times.insert(turn_id.clone(), tokio::time::Instant::now());
            json!({
                "type": "turn_start",
                "params": {
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "trace_id": trace_id,
                    "span_id": span_id,
                }
            })
            .to_string()
        }
        TurnEvent::ToolCalled {
            tool_name,
            input_summary,
            ..
        } => json!({
            "type": "tool_call",
            "params": {
                "tool_name": tool_name,
                "args_summary": input_summary,
            }
        })
        .to_string(),
        TurnEvent::AssistantDelta { content, .. } => json!({
            "type": "stream_delta",
            "params": { "text": content }
        })
        .to_string(),
        TurnEvent::Completed {
            turn_id,
            output_tokens,
            input_tokens,
            traceparent,
            ..
        } => {
            let latency_ms = start_times.remove(turn_id).map_or(0, |t| {
                u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            let mut params = serde_json::Map::new();
            params.insert(
                "input_tokens".into(),
                json!(u64::from(input_tokens.unwrap_or(0))),
            );
            params.insert("output_tokens".into(), json!(u64::from(*output_tokens)));
            params.insert("latency_ms".into(), json!(latency_ms));
            if let Some(tp) = traceparent {
                params.insert("traceparent".into(), json!(tp));
            }
            json!({ "type": "turn_end", "params": serde_json::Value::Object(params) }).to_string()
        }
        TurnEvent::Failed { turn_id, .. } => {
            start_times.remove(turn_id);
            json!({
                "type": "turn_end",
                "params": {
                    "input_tokens": 0u64,
                    "output_tokens": 0u64,
                    "latency_ms": 0u64,
                }
            })
            .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn agent_socket_path_appends_dot_agent() {
        let p = agent_socket_path(Path::new("/run/smdjad.sock"));
        assert_eq!(p, std::path::PathBuf::from("/run/smdjad.sock.agent"));
    }

    #[test]
    fn turn_start_includes_session_and_turn_id() {
        let mut start_times = HashMap::new();
        let event = TurnEvent::Started {
            session_id: "sess-1".into(),
            turn_id: "turn-1".into(),
            conversation_id: None,
            trace_id: Some("abc".into()),
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let line = turn_event_to_pane_json(&event, &mut start_times);
        assert!(line.contains("turn_start"));
        assert!(line.contains("sess-1"));
        assert!(line.contains("turn-1"));
        // Start time should be recorded.
        assert!(start_times.contains_key("turn-1"));
    }

    #[test]
    fn turn_end_zero_latency_for_failed() {
        let mut start_times = HashMap::new();
        let event = TurnEvent::Failed {
            session_id: "s".into(),
            turn_id: "t-fail".into(),
            reason: "timeout".into(),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let line = turn_event_to_pane_json(&event, &mut start_times);
        assert!(line.contains("turn_end"));
        assert!(line.contains(r#""latency_ms":0"#));
        assert!(line.contains(r#""input_tokens":0"#));
    }

    #[test]
    fn completed_includes_traceparent_and_latency() {
        let mut start_times = HashMap::new();
        start_times.insert("t1".into(), tokio::time::Instant::now());
        let event = TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: "t1".into(),
            output_tokens: 88,
            input_tokens: Some(412),
            traceparent: Some("00-abc123def456-0102030405060708-01".into()),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let line = turn_event_to_pane_json(&event, &mut start_times);
        assert!(line.contains("turn_end"));
        assert!(line.contains(r#""input_tokens":412"#));
        assert!(line.contains(r#""output_tokens":88"#));
        assert!(line.contains("abc123def456"));
        // start_times entry removed after completion
        assert!(!start_times.contains_key("t1"));
    }
}
