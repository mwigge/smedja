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

/// Seconds to hold a stream connection open after a successful `done` so the
/// trailing Tier-1 quality snapshot — which the post-turn gate publishes a beat
/// *after* the turn completes — is delivered instead of being cut off by the
/// terminal `done`. A failed turn (`error`) has no trailing snapshot, so its
/// stream closes at once. In practice the snapshot arrives well under a second;
/// this is only the ceiling for a turn that never produces one.
const QUALITY_GRACE_SECS: u64 = 8;

/// Seconds of inactivity after which a *non-terminal* turn buffer is considered
/// stranded and evicted by the background sweeper. A turn whose orchestrator
/// panics mid-flight never emits a terminal event, so its buffer would otherwise
/// leak forever. Kept comfortably larger than [`DELTA_TTL_SECS`] so a merely slow
/// turn is never mistaken for a stranded one.
const STRANDED_TTL_SECS: u64 = 300;

/// How often the stranded-turn sweeper wakes to reclaim idle buffers.
const STRANDED_SWEEP_INTERVAL_SECS: u64 = 60;

/// A per-turn event buffer plus the bookkeeping the stranded-turn sweeper needs.
///
/// Public only because it appears in the [`DeltaStore`] type alias; its fields
/// are an internal implementation detail of this module.
pub struct TurnBuffer {
    /// Buffered NDJSON lines, drained by each streaming connection before it
    /// switches to live Bellows events.
    lines: VecDeque<String>,
    /// Updated on every buffered event; drives stranded-turn eviction. Uses the
    /// Tokio clock so it advances with `tokio::time` (and paused-clock tests).
    last_activity: tokio::time::Instant,
}

impl TurnBuffer {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            last_activity: tokio::time::Instant::now(),
        }
    }

    /// Marks activity and returns the mutable line buffer, so call sites keep
    /// operating on the `VecDeque` exactly as before.
    fn touch(&mut self) -> &mut VecDeque<String> {
        self.last_activity = tokio::time::Instant::now();
        &mut self.lines
    }
}

/// Per-turn event buffer store — keyed by `turn_id` (= `task_id` in smdjad).
///
/// Populated by a background subscriber task; drained by each streaming
/// connection for that turn before it switches to live Bellows events.
pub type DeltaStore = Arc<Mutex<HashMap<String, TurnBuffer>>>;

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
                        store.insert(turn_id.clone(), TurnBuffer::new());
                        // Emit a started event so the TUI can capture agent_name.
                        if let Some(ref name) = correlation.agent_name {
                            if let Some(buf) = store.get_mut(turn_id).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(turn_id).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(turn_id).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
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
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
                            let line = json!({"type": "tool_call_chunk", "name": name, "partial_input": partial_input}).to_string();
                            evict_and_push(buf, line);
                        }
                    }
                    TurnEvent::HistoryReplaced {
                        ref session_id,
                        ref turn_id,
                        summary_tokens,
                    } => {
                        if let Some(buf) = store.get_mut(turn_id).map(TurnBuffer::touch) {
                            let line = json!({"type": "history_replaced", "session_id": session_id, "summary_tokens": summary_tokens}).to_string();
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

    // Stranded-turn sweeper: a turn whose orchestrator panics mid-flight never
    // emits a terminal event, so its buffer never gets a scheduled TTL GC and
    // would leak forever. Periodically drop any buffer idle longer than
    // STRANDED_TTL_SECS. Terminal turns are removed far sooner (DELTA_TTL_SECS)
    // by the scheduled GC above, so this only ever reclaims genuinely stranded
    // buffers.
    let store_sweep = Arc::clone(&store);
    tokio::spawn(async move {
        let ttl = std::time::Duration::from_secs(STRANDED_TTL_SECS);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(STRANDED_SWEEP_INTERVAL_SECS)).await;
            let now = tokio::time::Instant::now();
            let mut map = store_sweep.lock().await;
            let before = map.len();
            map.retain(|_, b| now.saturating_duration_since(b.last_activity) < ttl);
            let removed = before - map.len();
            drop(map);
            if removed > 0 {
                tracing::warn!(
                    removed,
                    "stream_server: evicted stranded turn buffers (no activity within TTL)"
                );
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
    // event if the turn completed before this connection was established. Capture
    // the buffer's age too: it bounds how long we wait for a trailing quality
    // snapshot on the replay path (see below).
    let (buffered, buf_age): (VecDeque<String>, std::time::Duration) = {
        let s = store.lock().await;
        match s.get(&task_id) {
            Some(b) => (
                b.lines.clone(),
                tokio::time::Instant::now().saturating_duration_since(b.last_activity),
            ),
            None => (VecDeque::new(), std::time::Duration::ZERO),
        }
    };

    // Replay every buffered line. A successful turn's buffer ends with `done`
    // followed a beat later by its Tier-1 `quality` snapshot, so replaying past
    // the terminal `done` (rather than stopping at it) is what delivers that
    // snapshot to a client that connected after the turn ended.
    let mut saw_done = false;
    let mut saw_error = false;
    let mut saw_quality = false;
    for event_line in &buffered {
        if write_line(&mut writer, event_line).await.is_err() {
            return;
        }
        if event_line.contains(r#""type":"quality""#) {
            saw_quality = true;
        } else if event_line.contains(r#""type":"done""#) {
            saw_done = true;
        } else if event_line.contains(r#""type":"error""#) {
            saw_error = true;
        }
    }

    // The turn is fully reported once its terminal event (and, for a success, the
    // trailing quality snapshot) has been sent. When `done` was buffered but the
    // quality snapshot has not landed in the buffer yet, fall through to the live
    // loop and wait for it within a bounded grace window instead of closing now
    // and dropping it. Do NOT evict the buffer here — the scheduled TTL GC owns
    // terminal-buffer removal so other clients can still replay within the window.
    if saw_error || (saw_done && saw_quality) {
        return;
    }
    // Only wait for a trailing quality snapshot when `done` is *recent* — a fresh
    // completion whose snapshot is still in flight. A stale buffer (the turn ended
    // long ago and no snapshot ever landed) closes at once instead of idling for
    // the whole grace window.
    let grace = std::time::Duration::from_secs(QUALITY_GRACE_SECS);
    let mut grace_until = (saw_done && buf_age < grace).then(|| tokio::time::Instant::now() + grace);

    // Forward live events filtered to this turn's task_id.
    //
    // This is an IDLE timeout, reset on every received event — not a cap on the
    // whole turn. A long agentic turn (a repo-wide review, many tool calls)
    // legitimately streams for minutes; only a genuinely stalled stream (no
    // event for STREAM_TIMEOUT_SECS) should error out. The overall turn budget is
    // enforced separately by the orchestrator's wall-clock cap.
    loop {
        // Normally an idle cap; after a successful `done` it is the remaining
        // grace window we hold open for the trailing quality snapshot.
        let wait = match grace_until {
            Some(dl) => dl.saturating_duration_since(tokio::time::Instant::now()),
            None => std::time::Duration::from_secs(STREAM_TIMEOUT_SECS),
        };
        let event = match tokio::time::timeout(wait, rx.recv()).await {
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
                // A grace deadline lapsing just means no quality snapshot followed
                // this turn's `done` — close quietly. A genuine idle stall (no
                // grace armed) is a transport failure worth surfacing.
                if grace_until.is_none() {
                    let msg = json!({
                        "type": "error",
                        "message": format!("stream stalled: no events for {STREAM_TIMEOUT_SECS}s")
                    })
                    .to_string();
                    let _ = write_line(&mut writer, &msg).await;
                }
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

        // The quality snapshot is the trailing event held open for after `done`;
        // once it is delivered the turn is fully reported.
        if ndjson_line.contains(r#""type":"quality""#) {
            break;
        }
        if is_terminal {
            // A successful `done` may be followed by a Tier-1 quality snapshot the
            // post-turn hook publishes a beat later — arm the grace window and keep
            // the stream open for it. `error` turns get no snapshot, so close now.
            if ndjson_line.contains(r#""type":"done""#) {
                grace_until = Some(
                    tokio::time::Instant::now()
                        + std::time::Duration::from_secs(QUALITY_GRACE_SECS),
                );
                continue;
            }
            break;
        }
    }

    // Do NOT evict the buffer on connection close. For a completed turn the
    // scheduled TTL GC owns removal (and other clients may still replay within
    // DELTA_TTL_SECS); for a turn that ended without a terminal event the
    // stranded-turn sweeper reclaims it after STRANDED_TTL_SECS. Removing it here
    // would either defeat the replay window or drop a still-running turn's buffer.
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
        TurnEvent::HistoryReplaced { turn_id, .. } => (Some(turn_id.clone()), String::new(), false),
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
        let buf = &s.get("t1").expect("buffer entry for t1").lines;
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
        let buf = &s.get("t2").expect("buffer for t2").lines;
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

    #[tokio::test(start_paused = true)]
    async fn stranded_turn_evicted_by_sweeper() {
        // A turn that emits Started + deltas but never a terminal event (its
        // orchestrator panicked) must not leak its buffer forever: the stranded
        // sweeper evicts it once it has been idle past STRANDED_TTL_SECS.
        let dispatcher = Arc::new(Dispatcher::new(64));
        let store = spawn_delta_buffer(&dispatcher);

        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t-stranded".into(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "partial".into(),
            turn_id: Some("t-stranded".into()),
            correlation: CorrelationCtx::default(),
        });
        // NB: no Completed/Failed event is ever published.

        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            store.lock().await.contains_key("t-stranded"),
            "stranded buffer must exist before the sweep TTL elapses"
        );

        // Advance past the stranded TTL plus one sweep interval so the sweeper
        // runs at least once with the buffer fully aged out.
        tokio::time::advance(std::time::Duration::from_secs(
            STRANDED_TTL_SECS + STRANDED_SWEEP_INTERVAL_SECS + 5,
        ))
        .await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        assert!(
            !store.lock().await.contains_key("t-stranded"),
            "stranded turn buffer must be evicted by the sweeper"
        );
    }

    #[tokio::test]
    async fn second_client_can_replay_completed_turn_within_ttl() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let dispatcher = Arc::new(Dispatcher::new(64));
        let store = spawn_delta_buffer(&dispatcher);

        // Build a completed turn buffer (Started + delta + Completed).
        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t-replay".into(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "hi".into(),
            turn_id: Some("t-replay".into()),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Completed {
            session_id: "sess".into(),
            turn_id: "t-replay".into(),
            output_tokens: 3,
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        });
        // The post-turn gate publishes a trailing quality snapshot a beat after
        // `done`; it lands in the same buffer.
        dispatcher.publish(TurnEvent::QualitySnapshot {
            score: 75,
            tdd_pass: true,
            clean_pass: true,
            file_advisories: vec![],
            skill_advisories: vec![],
            llm_reviewed: false,
            turn_id: Some("t-replay".into()),
            correlation: CorrelationCtx::default(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            store.lock().await.contains_key("t-replay"),
            "completed buffer must persist within TTL"
        );

        // Drive one full stream connection and collect its output until EOF.
        async fn replay(store: &DeltaStore, dispatcher: &Arc<Dispatcher>) -> String {
            let (mut client, server) = UnixStream::pair().expect("socketpair");
            let store = Arc::clone(store);
            let dispatcher = Arc::clone(dispatcher);
            let handle =
                tokio::spawn(
                    async move { handle_stream_connection(server, store, dispatcher).await },
                );
            client
                .write_all(b"{\"task_id\":\"t-replay\"}\n")
                .await
                .expect("write request");
            let mut out = String::new();
            client
                .read_to_string(&mut out)
                .await
                .expect("read response");
            handle.await.expect("handler task");
            out
        }

        // First client replays the completed turn — including the trailing
        // quality snapshot that follows `done`.
        let out1 = replay(&store, &dispatcher).await;
        assert!(
            out1.contains(r#""type":"done""#),
            "first client must receive the done line; got: {out1}"
        );
        assert!(
            out1.contains(r#""type":"quality""#),
            "replay past `done` must also deliver the trailing quality snapshot; got: {out1}"
        );

        // The first client must NOT have evicted the buffer.
        assert!(
            store.lock().await.contains_key("t-replay"),
            "first replay must not evict the buffer within the TTL"
        );

        // Second client, still within the TTL, must also be able to replay it.
        let out2 = replay(&store, &dispatcher).await;
        assert!(
            out2.contains(r#""type":"done""#),
            "second client must still replay the completed turn; got: {out2}"
        );
    }

    // Regression: the Tier-1 quality snapshot is published a beat *after* the
    // turn's `done`. `done` is terminal, so before the grace window the stream
    // closed on `done` and the trailing snapshot was lost — leaving the quality
    // and value panels perpetually empty. A live connection must stay open long
    // enough to forward the snapshot that arrives after `done`.
    #[tokio::test]
    async fn live_stream_delivers_quality_snapshot_after_done() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let dispatcher = Arc::new(Dispatcher::new(64));
        let store = spawn_delta_buffer(&dispatcher);

        let (mut client, server) = UnixStream::pair().expect("socketpair");
        let store_c = Arc::clone(&store);
        let dispatcher_c = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move {
            handle_stream_connection(server, store_c, dispatcher_c).await;
        });
        client
            .write_all(b"{\"task_id\":\"t-live\"}\n")
            .await
            .expect("write request");

        // Give the handler a moment to subscribe before events are published.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        dispatcher.publish(TurnEvent::Completed {
            session_id: "sess".into(),
            turn_id: "t-live".into(),
            output_tokens: 3,
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        });
        // The snapshot arrives a beat after `done`, mirroring the async hook.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        dispatcher.publish(TurnEvent::QualitySnapshot {
            score: 100,
            tdd_pass: true,
            clean_pass: true,
            file_advisories: vec![],
            skill_advisories: vec![],
            llm_reviewed: false,
            turn_id: Some("t-live".into()),
            correlation: CorrelationCtx::default(),
        });

        let mut out = String::new();
        client
            .read_to_string(&mut out)
            .await
            .expect("read response");
        handle.await.expect("handler task");

        assert!(
            out.contains(r#""type":"done""#),
            "must forward the terminal done; got: {out}"
        );
        assert!(
            out.contains(r#""type":"quality""#),
            "must forward the quality snapshot published after done; got: {out}"
        );
    }
}
