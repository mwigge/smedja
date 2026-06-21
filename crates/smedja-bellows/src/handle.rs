use std::sync::Arc;

use crate::dispatcher::Dispatcher;
use crate::event::TurnEvent;

/// A handle to an in-progress agent turn.
///
/// Creating a `TurnHandle` via [`start`](TurnHandle::start) immediately
/// publishes a [`TurnEvent::Started`] event.  The handle exposes methods for
/// recording mid-turn activity ([`tool_called`](TurnHandle::tool_called),
/// [`delta`](TurnHandle::delta)) and for resolving the turn by consuming the
/// handle ([`complete`](TurnHandle::complete), [`fail`](TurnHandle::fail)).
pub struct TurnHandle {
    session_id: String,
    turn_id: String,
    dispatcher: Arc<Dispatcher>,
}

impl TurnHandle {
    /// Creates a new handle and immediately publishes [`TurnEvent::Started`].
    ///
    /// `session_id` and `turn_id` together uniquely identify the turn across
    /// all events emitted by this handle.
    #[must_use]
    pub fn start(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        let session_id = session_id.into();
        let turn_id = turn_id.into();

        dispatcher.publish(TurnEvent::Started {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        });

        Self {
            session_id,
            turn_id,
            dispatcher,
        }
    }

    /// Publishes a [`TurnEvent::ToolCalled`] event.
    ///
    /// Call this each time a tool is invoked during the turn.
    pub fn tool_called(&self, tool_name: impl Into<String>, input_summary: impl Into<String>) {
        self.dispatcher.publish(TurnEvent::ToolCalled {
            tool_name: tool_name.into(),
            input_summary: input_summary.into(),
        });
    }

    /// Publishes a [`TurnEvent::AssistantDelta`] event.
    ///
    /// Call this for each incremental text chunk produced by the assistant
    /// during streaming.
    pub fn delta(&self, content: impl Into<String>) {
        self.dispatcher.publish(TurnEvent::AssistantDelta {
            content: content.into(),
        });
    }

    /// Publishes [`TurnEvent::Completed`] and consumes the handle.
    ///
    /// `output_tokens` is the number of tokens generated during this turn.
    pub fn complete(self, output_tokens: u32) {
        self.dispatcher.publish(TurnEvent::Completed {
            session_id: self.session_id,
            turn_id: self.turn_id,
            output_tokens,
        });
    }

    /// Publishes [`TurnEvent::Failed`] and consumes the handle.
    ///
    /// `reason` is a human-readable explanation of the failure.
    pub fn fail(self, reason: impl Into<String>) {
        self.dispatcher.publish(TurnEvent::Failed {
            session_id: self.session_id,
            turn_id: self.turn_id,
            reason: reason.into(),
        });
    }
}
