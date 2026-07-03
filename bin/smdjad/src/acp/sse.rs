//! Server-Sent Events streaming for ACP turns, with `Last-Event-ID` reconnect.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use smedja_bellows::TurnEvent;

use super::event_buffer::EventBuffer;
use super::state::AcpState;

/// Returns the turn identifier an event is correlated with, if any.
fn turn_id_of(event: &TurnEvent) -> Option<&str> {
    match event {
        TurnEvent::Started { turn_id, .. }
        | TurnEvent::Completed { turn_id, .. }
        | TurnEvent::Failed { turn_id, .. }
        | TurnEvent::HistoryReplaced { turn_id, .. } => Some(turn_id.as_str()),
        TurnEvent::ToolCalled { turn_id, .. }
        | TurnEvent::AssistantDelta { turn_id, .. }
        | TurnEvent::ThinkingDelta { turn_id, .. }
        | TurnEvent::QualitySnapshot { turn_id, .. }
        | TurnEvent::CoworkRequest { turn_id, .. }
        | TurnEvent::TokenUsage { turn_id, .. }
        | TurnEvent::ToolCallChunk { turn_id, .. } => turn_id.as_deref(),
    }
}

/// Reports whether `event` is a terminal event (`Completed` or `Failed`) for
/// `turn_id`.
fn is_terminal_for(event: &TurnEvent, turn_id: &str) -> bool {
    matches!(
        event,
        TurnEvent::Completed { turn_id: t, .. } | TurnEvent::Failed { turn_id: t, .. } if t == turn_id
    )
}

/// Builds an SSE response that forwards every [`TurnEvent`] for `turn_id` from
/// `receiver`, terminating after the turn's terminal event. A keep-alive
/// heartbeat prevents idle-timeout disconnects.
///
/// Each emitted event carries an `id:` sequence number and is stored in
/// `replay` so reconnecting clients can catch up via `Last-Event-ID`.
///
/// `pending` is empty for new connections and pre-populated with buffered
/// events when a client reconnects with `Last-Event-ID`.
pub(crate) fn build_turn_sse(
    receiver: tokio::sync::broadcast::Receiver<TurnEvent>,
    turn_id: String,
    pending: std::collections::VecDeque<(u64, String)>,
    replay: Arc<EventBuffer>,
    next_seq: Arc<std::sync::atomic::AtomicU64>,
) -> axum::response::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use std::sync::atomic::Ordering;

    use axum::response::sse::{Event, KeepAlive, Sse};

    let stream = futures_util::stream::unfold(
        (receiver, turn_id, pending, false, replay, next_seq),
        |(mut rx, turn_id, mut pending, finished, replay, next_seq)| async move {
            if finished {
                return None;
            }
            // Phase 1: drain buffered replay events (reconnect path).
            if let Some((seq, data)) = pending.pop_front() {
                let sse_event = Event::default().id(seq.to_string()).data(data);
                return Some((
                    Ok(sse_event),
                    (rx, turn_id, pending, false, replay, next_seq),
                ));
            }
            // Phase 2: live events from the dispatcher.
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        match turn_id_of(&event) {
                            Some(tid) if tid == turn_id => {}
                            _ => continue,
                        }
                        let terminal = is_terminal_for(&event, &turn_id);
                        let data = serde_json::to_string(&event).unwrap_or_default();
                        let seq = next_seq.fetch_add(1, Ordering::Relaxed);
                        replay.push(&turn_id, seq, data.clone());
                        let sse_event = Event::default().id(seq.to_string()).data(data);
                        return Some((
                            Ok(sse_event),
                            (rx, turn_id, pending, terminal, replay, next_seq),
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// GET `/acp/v1/session/{id}/events/{turn_id}` — SSE stream for a running or
/// completed turn, with `Last-Event-ID` reconnect support.
pub(crate) async fn get_turn_events(
    Path((_, turn_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    State(s): State<AcpState>,
) -> impl IntoResponse {
    let last_seq: u64 = headers
        .get("Last-Event-ID")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let pending = std::collections::VecDeque::from(s.replay.events_after(&turn_id, last_seq));
    let receiver = s.dispatcher.subscribe();
    build_turn_sse(receiver, turn_id, pending, s.replay, s.next_seq)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::response::IntoResponse as _;
    use smedja_bellows::Dispatcher;

    #[tokio::test]
    async fn turn_sse_starts_with_started_and_ends_on_terminal() {
        use smedja_bellows::event::CorrelationCtx;
        use smedja_bellows::TurnEvent;

        let dispatcher = Dispatcher::new(32);
        let turn_id = "turn-sse-1".to_owned();
        let rx = dispatcher.subscribe();

        // Publish Started then Completed for the turn (buffered for the rx).
        dispatcher.publish(TurnEvent::Started {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            output_tokens: 1,
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        });

        let sse = super::build_turn_sse(
            rx,
            turn_id,
            std::collections::VecDeque::new(),
            Arc::new(super::EventBuffer::new()),
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
        );
        let resp = sse.into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);

        let started_at = text.find("Started").expect("Started must be delivered");
        let completed_at = text.find("Completed").expect("Completed must be delivered");
        assert!(
            started_at < completed_at,
            "Started must precede Completed; got: {text}"
        );
    }

    #[tokio::test]
    async fn turn_sse_ignores_other_turns_and_still_terminates() {
        use smedja_bellows::event::CorrelationCtx;
        use smedja_bellows::TurnEvent;

        let dispatcher = Dispatcher::new(32);
        let turn_id = "mine".to_owned();
        let rx = dispatcher.subscribe();

        // An event for a different turn must be ignored; heartbeats aside, the
        // stream must still end on this turn's terminal event.
        dispatcher.publish(TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "someone-else".into(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Started {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Failed {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            reason: "boom".into(),
            correlation: CorrelationCtx::default(),
        });

        let sse = super::build_turn_sse(
            rx,
            turn_id,
            std::collections::VecDeque::new(),
            Arc::new(super::EventBuffer::new()),
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
        );
        let resp = sse.into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("someone-else"),
            "events for other turns must be filtered out; got: {text}"
        );
        assert!(
            text.contains("Failed"),
            "the terminal Failed event must be delivered; got: {text}"
        );
    }
}
