//! Per-turn event buffering, TTL eviction, and the background subscriber and
//! stranded-turn sweeper tasks.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use smedja_bellows::{Dispatcher, StreamEvent, TurnEvent};

/// Maximum NDJSON lines buffered per turn before the oldest are discarded.
const MAX_BUFFER_PER_TURN: usize = 8192;

/// Seconds to retain a terminal turn's buffer after completion before auto-eviction.
/// This window allows late-connecting stream clients to still replay the turn.
const DELTA_TTL_SECS: u64 = 60;

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
    pub(crate) lines: VecDeque<String>,
    /// Updated on every buffered event; drives stranded-turn eviction. Uses the
    /// Tokio clock so it advances with `tokio::time` (and paused-clock tests).
    pub(crate) last_activity: tokio::time::Instant,
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

/// Buffer key for session-scoped events (a [`TurnEvent::CoworkRequest`] raised
/// outside any turn — the claude-hook and industry-ACP bridge paths, which have
/// no turn id). Never collides with a real turn id (those are non-empty). Every
/// stream connection replays this buffer alongside its turn's, so a pending
/// approval survives a TUI reconnect; the stranded sweeper keeps it alive while
/// it holds an unresolved cowork_request and reclaims it once the approval is
/// resolved and the buffer goes idle.
pub(crate) const SESSION_BUFFER_KEY: &str = "";

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
                        ref cwd,
                        ref turn_id,
                        supports_modify,
                        ref correlation,
                    } => {
                        let line = super::wire::cowork_request_line(
                            approval_id,
                            tool,
                            step_n,
                            args_display,
                            reasoning,
                            correlation.agent_name.as_deref(),
                            cwd.as_deref(),
                            supports_modify,
                        );
                        // Turn-scoped requests join their turn's buffer;
                        // session-scoped ones (turn_id None — claude hook, ACP
                        // bridge) go to the shared session buffer so a reconnect
                        // still replays the pending approval.
                        let buf = match turn_id {
                            Some(tid) => store.get_mut(tid).map(TurnBuffer::touch),
                            None => Some(
                                store
                                    .entry(SESSION_BUFFER_KEY.to_owned())
                                    .or_insert_with(TurnBuffer::new)
                                    .touch(),
                            ),
                        };
                        if let Some(buf) = buf {
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
                    TurnEvent::ToolCallUpdate {
                        ref tool_call_id,
                        ref tool_name,
                        status,
                        ref content,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        if let Some(buf) = store.get_mut(tid).map(TurnBuffer::touch) {
                            let line = serde_json::to_string(&StreamEvent::ToolCallUpdate {
                                tool_call_id: tool_call_id.clone(),
                                tool_name: tool_name.clone(),
                                status: status.as_acp_str().to_owned(),
                                content: content.clone(),
                            })
                            .unwrap_or_default();
                            evict_and_push(buf, line);
                        }
                    }
                    TurnEvent::AuditProgress {
                        iteration,
                        total,
                        ref activity,
                        findings_so_far,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        // The auditor emits no `Started`, so create the buffer on
                        // the first progress heartbeat — otherwise a late-connecting
                        // client could not replay it.
                        let buf = store
                            .entry(tid.clone())
                            .or_insert_with(TurnBuffer::new)
                            .touch();
                        let line = json!({
                            "type": "audit_progress",
                            "iteration": iteration,
                            "total": total,
                            "activity": activity,
                            "findings_so_far": findings_so_far,
                        })
                        .to_string();
                        evict_and_push(buf, line);
                    }
                    TurnEvent::AuditReport {
                        ref report,
                        ref counts,
                        ref report_path,
                        ref turn_id,
                        ..
                    } => {
                        let Some(tid) = turn_id else { continue };
                        let buf = store
                            .entry(tid.clone())
                            .or_insert_with(TurnBuffer::new)
                            .touch();
                        let mut obj = serde_json::Map::new();
                        obj.insert("type".into(), json!("audit_report"));
                        obj.insert("report".into(), json!(report));
                        obj.insert("counts".into(), counts.clone());
                        if let Some(p) = report_path {
                            obj.insert("report_path".into(), json!(p));
                        }
                        buf.push_back(serde_json::Value::Object(obj).to_string());
                        cleanup_tid = Some(tid.clone());
                    }
                    TurnEvent::CoworkResolved {
                        ref approval_id, ..
                    } => {
                        // A resolution retires the pending request: drop the
                        // matching `cowork_request` line from every buffer (the
                        // session buffer for hook/ACP requests, the turn buffer
                        // for native-loop ones) so a reconnecting client no
                        // longer replays an answered approval. The resolution
                        // itself is live-only and never buffered.
                        let needle = format!("\"approval_id\":\"{approval_id}\"");
                        for buf in store.values_mut() {
                            buf.lines.retain(|l| {
                                !(l.contains("\"cowork_request\"") && l.contains(&needle))
                            });
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
    // buffers. A buffer holding an UNRESOLVED cowork_request line is never
    // stranded: the gate is waiting on a human (up to the 30-min approval
    // timeout), so evicting it would make the pending approval un-replayable
    // on reconnect. Keeping it and refreshing last_activity re-arms the TTL.
    let store_sweep = Arc::clone(&store);
    tokio::spawn(async move {
        let ttl = std::time::Duration::from_secs(STRANDED_TTL_SECS);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(STRANDED_SWEEP_INTERVAL_SECS)).await;
            let now = tokio::time::Instant::now();
            let mut map = store_sweep.lock().await;
            let before = map.len();
            map.retain(|_, b| {
                let idle = now.saturating_duration_since(b.last_activity);
                if idle < ttl {
                    return true;
                }
                if b.lines.iter().any(|l| l.contains("\"cowork_request\"")) {
                    // Unresolved approval awaiting a human — keep it alive.
                    // (CoworkResolved removes the request line, so a resolved
                    // prompt does not keep the buffer alive.)
                    b.last_activity = now;
                    return true;
                }
                false
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_bellows::event::CorrelationCtx;
    use std::sync::Arc;

    /// Polls until `pred` holds on the delta store, replacing fixed "let the
    /// background task process" sleeps that flaked under CI load.
    async fn wait_for_store(
        store: &DeltaStore,
        pred: impl Fn(&HashMap<String, TurnBuffer>) -> bool,
        what: &str,
    ) {
        for _ in 0..250 {
            let guard = store.lock().await;
            if pred(&guard) {
                return;
            }
            drop(guard);
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let guard = store.lock().await;
        assert!(pred(&guard), "timed out waiting for {what}");
    }

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

        wait_for_store(&store, |m| m.contains_key("t1"), "buffered turn t1").await;

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

    #[tokio::test(start_paused = true)]
    async fn sweeper_keeps_pending_cowork_request_then_evicts_after_resolution() {
        let dispatcher = Arc::new(Dispatcher::new(32));
        let store = spawn_delta_buffer(&dispatcher);

        dispatcher.publish(TurnEvent::CoworkRequest {
            approval_id: "appr-keep".into(),
            tool: "bash".into(),
            step_n: 0,
            args_display: "{\"command\":\"ls\"}".into(),
            reasoning: "run: ls".into(),
            cwd: None,
            turn_id: None,
            supports_modify: true,
            correlation: CorrelationCtx::default(),
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            store.lock().await.contains_key(SESSION_BUFFER_KEY),
            "session buffer must hold the pending approval"
        );

        // Advance far past the stranded TTL (several sweep intervals): the
        // buffer must survive — a human may take up to the gate's 30-min
        // timeout to answer.
        for _ in 0..8 {
            tokio::time::advance(std::time::Duration::from_secs(STRANDED_SWEEP_INTERVAL_SECS))
                .await;
            tokio::task::yield_now().await;
        }
        assert!(
            store.lock().await.contains_key(SESSION_BUFFER_KEY),
            "a buffer holding an unresolved cowork_request must not be swept"
        );

        // Once resolved, the request line is removed and the idle buffer is
        // swept like any other.
        dispatcher.publish(TurnEvent::CoworkResolved {
            approval_id: "appr-keep".into(),
            outcome: smedja_bellows::CoworkOutcome::Approved,
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        for _ in 0..(STRANDED_TTL_SECS / STRANDED_SWEEP_INTERVAL_SECS + 2) {
            tokio::time::advance(std::time::Duration::from_secs(STRANDED_SWEEP_INTERVAL_SECS))
                .await;
            tokio::task::yield_now().await;
        }
        assert!(
            !store.lock().await.contains_key(SESSION_BUFFER_KEY),
            "after resolution the idle session buffer must be swept"
        );
    }

    #[tokio::test]
    async fn session_scoped_cowork_request_buffered_under_session_key() {
        let dispatcher = Arc::new(Dispatcher::new(32));
        let store = spawn_delta_buffer(&dispatcher);

        // A CoworkRequest with no turn id (claude hook / ACP bridge) must be
        // buffered so a reconnecting stream replays the pending approval.
        dispatcher.publish(TurnEvent::CoworkRequest {
            approval_id: "appr-buf".into(),
            tool: "bash".into(),
            step_n: 0,
            args_display: "{\"command\":\"ls\"}".into(),
            reasoning: "run: ls".into(),
            cwd: None,
            turn_id: None,
            supports_modify: true,
            correlation: CorrelationCtx::default(),
        });
        // Turn-scoped requests still join their turn's buffer, not the session's.
        dispatcher.publish(TurnEvent::Started {
            session_id: "sess".into(),
            turn_id: "t-scoped".into(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::CoworkRequest {
            approval_id: "appr-turn".into(),
            tool: "bash".into(),
            step_n: 1,
            args_display: "{\"command\":\"make\"}".into(),
            reasoning: "run: make".into(),
            cwd: None,
            turn_id: Some("t-scoped".into()),
            supports_modify: true,
            correlation: CorrelationCtx::default(),
        });

        wait_for_store(
            &store,
            |m| m.contains_key(SESSION_BUFFER_KEY),
            "session buffer for the cowork request",
        )
        .await;

        let s = store.lock().await;
        let session_buf = &s
            .get(SESSION_BUFFER_KEY)
            .expect("session buffer for turn_id-less CoworkRequest")
            .lines;
        assert!(
            session_buf.iter().any(|l| l.contains("appr-buf")),
            "session-scoped request must be buffered; got: {session_buf:?}"
        );
        let turn_buf = &s.get("t-scoped").expect("turn buffer").lines;
        assert!(
            turn_buf.iter().any(|l| l.contains("appr-turn")),
            "turn-scoped request must join the turn buffer; got: {turn_buf:?}"
        );
    }

    #[tokio::test]
    async fn cowork_resolved_removes_request_from_buffers() {
        let dispatcher = Arc::new(Dispatcher::new(32));
        let store = spawn_delta_buffer(&dispatcher);

        dispatcher.publish(TurnEvent::CoworkRequest {
            approval_id: "appr-res".into(),
            tool: "bash".into(),
            step_n: 0,
            args_display: "{\"command\":\"ls\"}".into(),
            reasoning: "run: ls".into(),
            cwd: None,
            turn_id: None,
            supports_modify: true,
            correlation: CorrelationCtx::default(),
        });
        wait_for_store(
            &store,
            |m| {
                m.get(SESSION_BUFFER_KEY)
                    .is_some_and(|b| b.lines.iter().any(|l| l.contains("appr-res")))
            },
            "buffered request before the resolution",
        )
        .await;
        assert!(
            store
                .lock()
                .await
                .get(SESSION_BUFFER_KEY)
                .is_some_and(|b| b.lines.iter().any(|l| l.contains("appr-res"))),
            "request must be buffered before the resolution"
        );

        dispatcher.publish(TurnEvent::CoworkResolved {
            approval_id: "appr-res".into(),
            outcome: smedja_bellows::CoworkOutcome::Approved,
        });
        wait_for_store(
            &store,
            |m| {
                m.get(SESSION_BUFFER_KEY)
                    .is_some_and(|b| !b.lines.iter().any(|l| l.contains("appr-res")))
            },
            "retired request after the resolution",
        )
        .await;

        let s = store.lock().await;
        let session_buf = &s.get(SESSION_BUFFER_KEY).expect("session buffer").lines;
        assert!(
            !session_buf.iter().any(|l| l.contains("appr-res")),
            "resolution must retire the buffered request; got: {session_buf:?}"
        );
        assert!(
            !session_buf.iter().any(|l| l.contains("cowork_resolved")),
            "the resolution itself must not be buffered; got: {session_buf:?}"
        );
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
}
