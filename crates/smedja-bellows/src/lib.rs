//! `smedja-bellows` — turn lifecycle events and broadcast dispatcher.
//!
//! A turn starts, events fire as it progresses, and it ends with output or
//! failure.  This crate provides the typed event enum, a channel-based
//! dispatcher, and a [`TurnHandle`] that ties the lifecycle together.

pub mod dispatcher;
pub mod event;
pub mod handle;

pub use dispatcher::Dispatcher;
pub use event::TurnEvent;
pub use handle::TurnHandle;

/// Drains all immediately-available events from `rx` without blocking.
///
/// Uses [`tokio::sync::broadcast::Receiver::try_recv`] in a tight loop until
/// the channel is empty or closed.  Returns every [`TurnEvent`] collected.
/// When the channel is already empty this returns an empty [`Vec`] immediately.
///
/// Use this after an `rx.recv().await` has already returned the first event
/// to batch any additional queued events in the same processing round.
pub fn drain_ready(rx: &mut tokio::sync::broadcast::Receiver<TurnEvent>) -> Vec<TurnEvent> {
    let mut batch = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => batch.push(event),
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                tracing::warn!(dropped = n, "drain_ready: receiver lagged; events dropped");
                break;
            }
        }
    }
    batch
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Dispatcher, TurnEvent, TurnHandle};

    // ── Dispatcher tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatcher_publishes_to_subscriber() {
        let dispatcher = Dispatcher::new(16);
        let mut rx = dispatcher.subscribe();

        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "hello".to_owned(),
        });

        let event = rx.recv().await.expect("expected an event");
        assert_eq!(
            event,
            TurnEvent::AssistantDelta {
                content: "hello".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive_event() {
        let dispatcher = Dispatcher::new(16);
        let mut rx1 = dispatcher.subscribe();
        let mut rx2 = dispatcher.subscribe();

        let count = dispatcher.publish(TurnEvent::AssistantDelta {
            content: "broadcast".to_owned(),
        });
        assert_eq!(count, 2);

        let e1 = rx1.recv().await.expect("rx1 expected an event");
        let e2 = rx2.recv().await.expect("rx2 expected an event");

        let expected = TurnEvent::AssistantDelta {
            content: "broadcast".to_owned(),
        };
        assert_eq!(e1, expected);
        assert_eq!(e2, expected);
    }

    #[tokio::test]
    async fn publish_with_no_receivers_does_not_panic() {
        let dispatcher = Dispatcher::new(16);
        // Create and immediately drop a receiver so there are no active receivers.
        drop(dispatcher.subscribe());

        let count = dispatcher.publish(TurnEvent::AssistantDelta {
            content: "dropped".to_owned(),
        });
        assert_eq!(count, 0);
    }

    // ── TurnHandle tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn turn_handle_started_event_fires_on_creation() {
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();

        let _handle = TurnHandle::start("sess-1", "turn-1", Arc::clone(&dispatcher));

        let event = rx.recv().await.expect("expected Started event");
        assert_eq!(
            event,
            TurnEvent::Started {
                session_id: "sess-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn turn_handle_complete_fires_completed_event() {
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();

        let handle = TurnHandle::start("sess-2", "turn-2", Arc::clone(&dispatcher));
        // Drain the Started event so we can check Completed next.
        rx.recv().await.expect("expected Started event");

        handle.complete(42);

        let event = rx.recv().await.expect("expected Completed event");
        assert_eq!(
            event,
            TurnEvent::Completed {
                session_id: "sess-2".to_owned(),
                turn_id: "turn-2".to_owned(),
                output_tokens: 42,
            }
        );
    }

    #[tokio::test]
    async fn turn_handle_fail_fires_failed_event() {
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();

        let handle = TurnHandle::start("sess-3", "turn-3", Arc::clone(&dispatcher));
        rx.recv().await.expect("expected Started event");

        handle.fail("timeout");

        let event = rx.recv().await.expect("expected Failed event");
        assert_eq!(
            event,
            TurnEvent::Failed {
                session_id: "sess-3".to_owned(),
                turn_id: "turn-3".to_owned(),
                reason: "timeout".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn turn_handle_tool_called_fires_event() {
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();

        let handle = TurnHandle::start("sess-4", "turn-4", Arc::clone(&dispatcher));
        rx.recv().await.expect("expected Started event");

        handle.tool_called("bash", "ls -la /tmp");

        let event = rx.recv().await.expect("expected ToolCalled event");
        assert_eq!(
            event,
            TurnEvent::ToolCalled {
                tool_name: "bash".to_owned(),
                input_summary: "ls -la /tmp".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn turn_handle_delta_fires_event() {
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();

        let handle = TurnHandle::start("sess-5", "turn-5", Arc::clone(&dispatcher));
        rx.recv().await.expect("expected Started event");

        handle.delta("hello");

        let event = rx.recv().await.expect("expected AssistantDelta event");
        assert_eq!(
            event,
            TurnEvent::AssistantDelta {
                content: "hello".to_owned(),
            }
        );
    }

    // ── drain_ready tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn drain_ready_on_empty_returns_empty_vec() {
        let dispatcher = Dispatcher::new(16);
        let mut rx = dispatcher.subscribe();
        let batch = super::drain_ready(&mut rx);
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn drain_ready_collects_queued_events() {
        let dispatcher = Dispatcher::new(16);
        let mut rx = dispatcher.subscribe();

        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "a".to_owned(),
        });
        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "b".to_owned(),
        });
        dispatcher.publish(TurnEvent::AssistantDelta {
            content: "c".to_owned(),
        });

        let batch = super::drain_ready(&mut rx);
        assert_eq!(batch.len(), 3);
    }
}
