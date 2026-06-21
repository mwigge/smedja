use tokio::sync::broadcast;

use crate::event::TurnEvent;

/// Broadcasts [`TurnEvent`]s to an arbitrary number of subscribers.
///
/// Each call to [`subscribe`](Dispatcher::subscribe) returns an independent
/// [`broadcast::Receiver`] that receives every event published after the
/// subscription is established.  The dispatcher is intentionally fire-and-forget:
/// [`publish`](Dispatcher::publish) never returns an error even when there are
/// no active receivers.
pub struct Dispatcher {
    sender: broadcast::Sender<TurnEvent>,
}

impl Dispatcher {
    /// Creates a new dispatcher.
    ///
    /// `capacity` is the maximum number of events that can be queued per
    /// receiver before older messages are dropped.  A value of `16` is
    /// sufficient for typical turn workloads; increase it if you expect
    /// bursts of many rapid events.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Subscribes to turn events.
    ///
    /// The returned receiver will receive every event published *after* this
    /// call returns.  Multiple independent subscribers can be created from a
    /// single dispatcher.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<TurnEvent> {
        self.sender.subscribe()
    }

    /// Publishes `event` to all current subscribers.
    ///
    /// Returns the number of receivers that were sent the event.
    /// If there are no active receivers the event is silently discarded and
    /// `0` is returned — this is not an error condition.
    pub fn publish(&self, event: TurnEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }
}
