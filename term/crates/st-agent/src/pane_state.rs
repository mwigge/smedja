//! Per-pane live agent state consumed by the status bar renderer.

use std::sync::Arc;

use crate::event::PaneEvent;

// ─────────────────────────────────────────────────────────────────────────────
// PaneAgentState
// ─────────────────────────────────────────────────────────────────────────────

/// Per-pane live agent state consumed by the status bar renderer.
#[derive(Debug, Clone, Default)]
pub struct PaneAgentState {
    /// Tier string from the most recent `TurnStart` event (e.g. `"pro"`).
    pub tier: Option<String>,
    /// Model identifier from the most recent `TurnStart` event.
    pub model: Option<String>,
    /// Short description of what the agent is currently doing.
    pub active_task: Option<String>,
    /// True while an agent turn is in progress.
    pub is_agent_turn: bool,
    /// Input token count from the most recent `TurnEnd` event.
    pub last_input_tokens: Option<u64>,
    /// Output token count from the most recent `TurnEnd` event.
    pub last_output_tokens: Option<u64>,
    /// Turn latency in milliseconds from the most recent `TurnEnd` event.
    pub last_latency_ms: Option<u64>,
    /// W3C `traceparent` from the most recent `TurnEnd` event.
    pub last_traceparent: Option<String>,
    /// Cumulative tokens saved by the token economy, from the most recent
    /// `TurnEnd` that reported it. `None` until a figure arrives, so the
    /// status-bar segment renders nothing rather than a misleading zero.
    pub tokens_saved: Option<u64>,
    /// Cumulative efficiency ratio, from the most recent `TurnEnd` that
    /// reported it.
    pub efficiency_ratio: Option<f64>,
}

impl PaneAgentState {
    /// Applies a [`PaneEvent::TurnEnd`] to this state.
    ///
    /// Updates the per-turn token/latency counters and accumulates the
    /// cumulative token-economy figures. A `TurnEnd` that reports no savings
    /// figure leaves the previously accumulated value untouched, so a turn with
    /// no cache/compression activity never resets the gauge to a misleading
    /// zero. A non-`TurnEnd` event is ignored.
    pub fn apply_turn_end(&mut self, event: &PaneEvent) {
        let PaneEvent::TurnEnd {
            input_tokens,
            output_tokens,
            latency_ms,
            traceparent,
            tokens_saved,
            efficiency_ratio,
        } = event
        else {
            return;
        };
        self.is_agent_turn = false;
        self.last_input_tokens = Some(*input_tokens);
        self.last_output_tokens = Some(*output_tokens);
        self.last_latency_ms = Some(*latency_ms);
        self.last_traceparent.clone_from(traceparent);
        if let Some(saved) = *tokens_saved {
            self.tokens_saved = Some(saved);
        }
        if let Some(ratio) = *efficiency_ratio {
            self.efficiency_ratio = Some(ratio);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SharedPaneState
// ─────────────────────────────────────────────────────────────────────────────

/// Thread-safe, cheaply-cloneable wrapper around [`PaneAgentState`].
///
/// The status bar modules hold a clone of this and read it on every render
/// cycle; the event-loop task holds the same `Arc` and writes to it as events
/// arrive.
#[derive(Clone, Default)]
pub struct SharedPaneState(pub Arc<tokio::sync::RwLock<PaneAgentState>>);

impl SharedPaneState {
    /// Creates a new [`SharedPaneState`] backed by a default [`PaneAgentState`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_turn_end_accumulates_savings_into_state() {
        let mut state = PaneAgentState::default();
        state.apply_turn_end(&PaneEvent::TurnEnd {
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 100,
            traceparent: None,
            tokens_saved: Some(4242),
            efficiency_ratio: Some(0.41),
        });
        assert_eq!(state.last_input_tokens, Some(10));
        assert_eq!(state.tokens_saved, Some(4242));
        assert_eq!(state.efficiency_ratio, Some(0.41));
    }

    #[test]
    fn apply_turn_end_keeps_prior_savings_when_absent() {
        let mut state = PaneAgentState {
            tokens_saved: Some(100),
            efficiency_ratio: Some(0.5),
            ..PaneAgentState::default()
        };
        // A later TurnEnd that does not report savings must not clobber the
        // accumulated figure with None (no misleading reset to zero).
        state.apply_turn_end(&PaneEvent::TurnEnd {
            input_tokens: 1,
            output_tokens: 1,
            latency_ms: 1,
            traceparent: None,
            tokens_saved: None,
            efficiency_ratio: None,
        });
        assert_eq!(state.tokens_saved, Some(100));
        assert_eq!(state.efficiency_ratio, Some(0.5));
    }

    #[test]
    fn shared_pane_state_is_clone() {
        let s = SharedPaneState::new();
        let _s2 = s.clone();
    }

    #[test]
    fn pane_agent_state_has_new_token_fields() {
        let mut state = PaneAgentState::default();
        assert!(state.last_input_tokens.is_none());
        assert!(state.last_output_tokens.is_none());
        assert!(state.last_latency_ms.is_none());
        assert!(state.last_traceparent.is_none());
        state.last_input_tokens = Some(412);
        state.last_output_tokens = Some(88);
        state.last_latency_ms = Some(4200);
        state.last_traceparent = Some("00-abc-01".to_owned());
        assert_eq!(state.last_input_tokens, Some(412));
        assert_eq!(state.last_output_tokens, Some(88));
    }
}
