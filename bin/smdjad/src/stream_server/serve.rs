//! Connection acceptor plus the per-connection replay/live forwarding loop.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use smedja_bellows::Dispatcher;

use super::buffer::DeltaStore;
use super::wire::turn_event_to_ndjson;

/// Maximum seconds to keep a client stream open while waiting for live turn
/// events. This must be longer than the provider drain timeout so slow model
/// streams are not reported as stream transport failures.
const STREAM_TIMEOUT_SECS: u64 = 600;

/// Seconds to hold a stream connection open after a successful `done` so the
/// trailing Tier-1 quality snapshot — which the post-turn gate publishes a beat
/// *after* the turn completes — is delivered instead of being cut off by the
/// terminal `done`. A failed turn (`error`) has no trailing snapshot, so its
/// stream closes at once. In practice the snapshot arrives well under a second;
/// this is only the ceiling for a turn that never produces one.
const QUALITY_GRACE_SECS: u64 = 8;

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
    let mut grace_until =
        (saw_done && buf_age < grace).then(|| tokio::time::Instant::now() + grace);

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

async fn write_line(writer: &mut (impl AsyncWriteExt + Unpin), line: &str) -> std::io::Result<()> {
    let mut buf = line.to_owned();
    buf.push('\n');
    writer.write_all(buf.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_server::spawn_delta_buffer;
    use smedja_bellows::event::CorrelationCtx;
    use smedja_bellows::TurnEvent;
    use std::sync::Arc;

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
