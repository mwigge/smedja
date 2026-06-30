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
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use smedja_bellows::{Dispatcher, StreamEvent, TurnEvent};

/// Maximum NDJSON lines buffered per turn before the oldest are discarded.
const MAX_BUFFER_PER_TURN: usize = 8192;

/// Maximum seconds to keep a client stream open while waiting for live turn
/// events. This must be longer than the provider drain timeout so slow model
/// streams are not reported as stream transport failures.
const STREAM_TIMEOUT_SECS: u64 = 600;

/// Seconds to retain a terminal turn's buffer after completion before auto-eviction.
/// This window allows late-connecting stream clients to still replay the turn.
const DELTA_TTL_SECS: u64 = 60;

/// Per-turn event buffer — keyed by `turn_id` (= `task_id` in smdjad).
///
/// Populated by a background subscriber task; drained by each streaming
/// connection for that turn before it switches to live Bellows events.
pub type DeltaStore = Arc<Mutex<HashMap<String, VecDeque<String>>>>;

/// Appends `line` to `buf`, enforcing `MAX_BUFFER_PER_TURN`.
///
/// When the buffer is full, the oldest entry is evicted. If no overflow
/// marker exists at the front, one is inserted (consuming another slot so
/// the total never exceeds `MAX_BUFFER_PER_TURN`).
fn evict_and_push(buf: &mut std::collections::VecDeque<String>, line: String) {
    if buf.len() < MAX_BUFFER_PER_TURN {
        buf.push_back(line);
        return;
    }
    buf.pop_front();
    if let Some(marker) = buf
        .front_mut()
        .filter(|l| l.contains("\"buffer_overflow\""))
    {
        // Increment the existing lost counter so repeated overflow is accurately tracked.
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(marker) {
            let lost = v["lost"].as_u64().unwrap_or(1) + 1;
            v["lost"] = serde_json::json!(lost);
            *marker = v.to_string();
        }
    } else {
        // No marker yet — pop one more slot so total stays ≤ cap, then insert.
        buf.pop_front();
        buf.push_front(serde_json::json!({"type":"buffer_overflow","lost":1}).to_string());
    }
    buf.push_back(line);
}

/// Create a new empty [`DeltaStore`] and spawn the background subscriber that
/// populates it from the Bellows dispatcher.
///
/// The background task subscribes to `dispatcher` and appends NDJSON-formatted
/// event lines to the per-turn buffer.  When a turn reaches a terminal state
/// (`Completed` or `Failed`) the buffer entry is retained so late-connecting
/// stream clients can still replay it; callers should call
/// [`cleanup_turn`](cleanup_turn) after a short delay to reclaim memory.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn spawn_delta_buffer(dispatcher: &Arc<Dispatcher>) -> DeltaStore {
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
            let mut cleanup_tid: Option<String> = None;
            {
                let mut store = store_inner.lock().await;
                match event {
                    TurnEvent::Started {
                        ref turn_id,
                        ref correlation,
                        ..
                    } => {
                        store.insert(turn_id.clone(), VecDeque::new());
                        // Emit a started event so the TUI can capture agent_name.
                        if let Some(ref name) = correlation.agent_name {
                            if let Some(buf) = store.get_mut(turn_id) {
                                let line =
                                    json!({"type": "started", "agent_name": name}).to_string();
                                buf.push_back(line);
                            }
                        }
                    }
                    TurnEvent::AssistantDelta {
                        ref content,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid) {
                            let line = json!({"type": "delta", "text": content}).to_string();
                            evict_and_push(buf, line);
                        }
                    }
                    TurnEvent::ThinkingDelta {
                        ref content,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid) {
                            let line = json!({"type": "thinking", "text": content}).to_string();
                            evict_and_push(buf, line);
                        }
                    }
                    TurnEvent::ToolCalled {
                        ref tool_name,
                        ref input_summary,
                        ref full_input,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid) {
                            let line = json!({"type": "tool_call", "name": tool_name, "input": input_summary, "full": full_input})
                                .to_string();
                            evict_and_push(buf, line);
                        }
                    }
                    TurnEvent::Completed {
                        ref turn_id,
                        output_tokens,
                        input_tokens,
                        ref traceparent,
                        ..
                    } => {
                        if let Some(buf) = store.get_mut(turn_id) {
                            let mut obj = serde_json::Map::new();
                            obj.insert("type".into(), json!("done"));
                            obj.insert("output_tok".into(), json!(output_tokens));
                            if let Some(n) = input_tokens {
                                obj.insert("input_tok".into(), json!(n));
                            }
                            if let Some(tp) = traceparent {
                                obj.insert("traceparent".into(), json!(tp));
                            }
                            let line = serde_json::Value::Object(obj).to_string();
                            buf.push_back(line);
                        }
                        cleanup_tid = Some(turn_id.clone());
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
                        cleanup_tid = Some(turn_id.clone());
                    }
                    TurnEvent::QualitySnapshot {
                        ref turn_id,
                        score,
                        tdd_pass,
                        clean_pass,
                        ref file_advisories,
                        ref skill_advisories,
                        llm_reviewed,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid) {
                            let line = json!({
                                "type": "quality",
                                "score": score,
                                "tdd_pass": tdd_pass,
                                "clean_pass": clean_pass,
                                "file_advisories": file_advisories,
                                "skill_advisories": skill_advisories,
                                "llm_reviewed": llm_reviewed,
                            })
                            .to_string();
                            buf.push_back(line);
                        }
                    }
                    TurnEvent::CoworkRequest {
                        ref approval_id,
                        ref tool,
                        step_n,
                        ref args_display,
                        ref reasoning,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid) {
                            let line = serde_json::to_string(&StreamEvent::CoworkRequest {
                                approval_id: approval_id.clone(),
                                tool: tool.clone(),
                                step_n,
                                args_display: args_display.clone(),
                                reasoning: reasoning.clone(),
                            })
                            .unwrap_or_default();
                            evict_and_push(buf, line);
                        }
                    }
                    TurnEvent::TokenUsage {
                        input_tok,
                        output_tok,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid) {
                            let line = json!({"type": "usage", "input_tok": input_tok, "output_tok": output_tok}).to_string();
                            evict_and_push(buf, line);
                        }
                    }
                    TurnEvent::ToolCallChunk {
                        ref name,
                        ref partial_input,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid) {
                            let line = json!({"type": "tool_call_chunk", "name": name, "partial_input": partial_input}).to_string();
                            evict_and_push(buf, line);
                        }
                    }
                }
            } // lock released here
              // Schedule buffer eviction after TTL so late-connecting stream
              // clients can still replay the final event.
            if let Some(tid) = cleanup_tid {
                let store_gc = Arc::clone(&store_inner);
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(DELTA_TTL_SECS)).await;
                    cleanup_turn(&store_gc, &tid).await;
                });
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

/// Convert a [`TurnEvent`] to `(turn_id, ndjson_line, is_terminal)`.
///
/// Returns `(None, _, _)` for events where the `turn_id` is unknown or not
/// relevant to the caller's filter (e.g. daemon-level events).
#[allow(clippy::too_many_lines)]
fn turn_event_to_ndjson(
    event: &TurnEvent,
    expected_turn_id: &str,
) -> (Option<String>, String, bool) {
    let ser = |ev: &StreamEvent| serde_json::to_string(ev).unwrap_or_default();
    match event {
        TurnEvent::AssistantDelta {
            content, turn_id, ..
        } => {
            let line = ser(&StreamEvent::Delta {
                text: content.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::ThinkingDelta {
            content, turn_id, ..
        } => {
            let line = ser(&StreamEvent::Thinking {
                text: content.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::ToolCalled {
            tool_name,
            input_summary,
            full_input,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::ToolCall {
                name: tool_name.clone(),
                input: input_summary.clone(),
                full: full_input.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::Completed {
            turn_id,
            output_tokens,
            input_tokens,
            traceparent,
            ..
        } => {
            if turn_id != expected_turn_id {
                return (Some(turn_id.clone()), String::new(), false);
            }
            let line = ser(&StreamEvent::Done {
                output_tok: *output_tokens,
                input_tok: *input_tokens,
                traceparent: traceparent.clone(),
            });
            (Some(turn_id.clone()), line, true)
        }
        TurnEvent::Failed {
            turn_id, reason, ..
        } => {
            if turn_id != expected_turn_id {
                return (Some(turn_id.clone()), String::new(), false);
            }
            let line = ser(&StreamEvent::Error {
                message: reason.clone(),
            });
            (Some(turn_id.clone()), line, true)
        }
        TurnEvent::Started {
            turn_id,
            correlation,
            ..
        } => {
            if let Some(ref name) = correlation.agent_name {
                let line = ser(&StreamEvent::Started {
                    agent_name: Some(name.clone()),
                });
                (Some(turn_id.clone()), line, false)
            } else {
                (None, String::new(), false)
            }
        }
        TurnEvent::QualitySnapshot {
            score,
            tdd_pass,
            clean_pass,
            file_advisories,
            skill_advisories,
            llm_reviewed,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::Quality {
                score: *score,
                tdd_pass: *tdd_pass,
                clean_pass: *clean_pass,
                file_advisories: file_advisories.clone(),
                skill_advisories: skill_advisories.clone(),
                llm_reviewed: *llm_reviewed,
                suggested_command: None,
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::CoworkRequest {
            approval_id,
            tool,
            step_n,
            args_display,
            reasoning,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::CoworkRequest {
                approval_id: approval_id.clone(),
                tool: tool.clone(),
                step_n: *step_n,
                args_display: args_display.clone(),
                reasoning: reasoning.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::TokenUsage {
            input_tok,
            output_tok,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::Usage {
                input_tok: *input_tok,
                output_tok: *output_tok,
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::ToolCallChunk {
            name,
            partial_input,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::ToolCallChunk {
                name: name.clone(),
                partial_input: partial_input.clone(),
            });
            (turn_id.clone(), line, false)
        }
    }
}

async fn write_line(writer: &mut (impl AsyncWriteExt + Unpin), line: &str) -> std::io::Result<()> {
    let mut buf = line.to_owned();
    buf.push('\n');
    writer.write_all(buf.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_bellows::event::CorrelationCtx;
    use std::sync::Arc;

    #[tokio::test]
    async fn delta_buffer_populates_on_assistant_delta() {
        let dispatcher = Arc::new(Dispatcher::new(32));
        let store = spawn_delta_buffer(&dispatcher);

        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t1".into(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "hello".into(),
            turn_id: Some("t1".into()),
            correlation: CorrelationCtx::default(),
        });

        // Give the background task a moment to process.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let s = store.lock().await;
        let buf = s.get("t1").expect("buffer entry for t1");
        assert_eq!(buf.len(), 1);
        assert!(
            buf[0].contains("hello"),
            "expected delta line, got: {}",
            buf[0]
        );
    }

    #[tokio::test]
    async fn delta_buffer_caps_at_max_per_turn() {
        // Dispatcher capacity must exceed MAX_BUFFER_PER_TURN so the background
        // subscriber never lags and the Started event is never dropped.
        let dispatcher = Arc::new(Dispatcher::new(MAX_BUFFER_PER_TURN + 256));
        let store = spawn_delta_buffer(&dispatcher);

        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t2".into(),
            correlation: CorrelationCtx::default(),
        });

        for i in 0..=MAX_BUFFER_PER_TURN {
            dispatcher.publish(TurnEvent::AssistantDelta {
                content: format!("chunk-{i}"),
                turn_id: Some("t2".into()),
                correlation: CorrelationCtx::default(),
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
            correlation: CorrelationCtx::default(),
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
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t4");
        assert_eq!(tid.as_deref(), Some("t4"));
        assert!(line.contains(r#""type":"done""#));
        assert!(terminal);
    }

    #[test]
    fn turn_event_to_ndjson_completed_includes_traceparent_and_input_tok() {
        let event = TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: "t5".into(),
            output_tokens: 88,
            input_tokens: Some(412),
            traceparent: Some("00-abc123-def456-01".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t5");
        assert_eq!(tid.as_deref(), Some("t5"));
        assert!(terminal);
        assert!(
            line.contains(r#""input_tok":412"#),
            "expected input_tok in done line; got: {line}"
        );
        assert!(
            line.contains("abc123"),
            "expected traceparent in done line; got: {line}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delta_buffer_evicts_after_ttl() {
        let dispatcher = Arc::new(Dispatcher::new(64));
        let store = spawn_delta_buffer(&dispatcher);

        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t-ttl".into(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "hello".into(),
            turn_id: Some("t-ttl".into()),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Completed {
            session_id: "sess".into(),
            turn_id: "t-ttl".into(),
            output_tokens: 1,
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        });

        // Let the event-loop task process all queued events.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Buffer exists immediately after the terminal event.
        assert!(
            store.lock().await.contains_key("t-ttl"),
            "buffer must persist before TTL expires"
        );

        // Advance the clock past the TTL so the GC task fires.
        tokio::time::advance(std::time::Duration::from_secs(DELTA_TTL_SECS + 1)).await;
        tokio::task::yield_now().await;

        assert!(
            !store.lock().await.contains_key("t-ttl"),
            "buffer must be evicted after TTL"
        );
    }

    #[test]
    fn turn_event_to_ndjson_started_with_agent_name_emits_started_line() {
        let event = TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "t-start".into(),
            correlation: CorrelationCtx {
                agent_name: Some("review".into()),
                ..CorrelationCtx::default()
            },
        };
        let (_tid, line, terminal) = turn_event_to_ndjson(&event, "t-start");
        assert!(
            line.contains(r#""type":"started""#),
            "started event must have type=started; got: {line}"
        );
        assert!(
            line.contains("review"),
            "agent_name must appear in started line; got: {line}"
        );
        assert!(!terminal, "started event is not terminal");
    }

    #[test]
    fn turn_event_to_ndjson_started_without_agent_name_emits_empty() {
        let event = TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "t-no-agent".into(),
            correlation: CorrelationCtx::default(),
        };
        let (_tid, line, _terminal) = turn_event_to_ndjson(&event, "t-no-agent");
        assert!(
            line.is_empty(),
            "started without agent_name must emit empty line; got: {line}"
        );
    }

    #[test]
    fn turn_event_to_ndjson_thinking_delta_returns_thinking_type() {
        let event = TurnEvent::ThinkingDelta {
            content: "let me reason about this".into(),
            turn_id: Some("t-think".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t-think");
        assert_eq!(tid.as_deref(), Some("t-think"));
        assert!(
            line.contains(r#""type":"thinking""#),
            "thinking delta must have type=thinking; got: {line}"
        );
        assert!(
            line.contains("let me reason"),
            "thinking content must appear in NDJSON; got: {line}"
        );
        assert!(!terminal, "thinking delta must not be a terminal event");
    }
}
