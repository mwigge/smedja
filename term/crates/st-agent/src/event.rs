//! Events published by smdjad on a pane subscription and the user's approval
//! decision type.

use serde_json::Value;
use smedja_agent_events::AgentEventEnvelope;
use tracing::warn;

// ─────────────────────────────────────────────────────────────────────────────
// Events from smdjad
// ─────────────────────────────────────────────────────────────────────────────

/// Events published by smdjad on a pane subscription.
#[derive(Debug, Clone)]
pub enum PaneEvent {
    /// An agent turn has started.
    TurnStart {
        session_id: String,
        turn_id: String,
        tier: String,
        model: String,
        /// W3C trace-id for distributed tracing correlation.
        trace_id: Option<String>,
        /// W3C span-id from the span that produced this event.
        span_id: Option<String>,
    },
    /// The agent is invoking a tool.
    ToolCall {
        tool_name: String,
        args_summary: String,
        /// Tool-call identifier for correlating the call with its result.
        tool_call_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        trace_id: Option<String>,
        /// W3C span-id from the span that produced this event.
        span_id: Option<String>,
    },
    /// The agent requires interactive approval before executing a tool.
    ApprovalPrompt {
        tool_name: String,
        args: Value,
        prompt: String,
    },
    /// A tool invocation has completed.
    ToolResult { tool_name: String, outcome: String },
    /// The agent turn has finished; token and latency counters are attached.
    TurnEnd {
        input_tokens: u64,
        output_tokens: u64,
        latency_ms: u64,
        /// W3C `traceparent` header from the turn's root span, if available.
        traceparent: Option<String>,
        /// Cumulative tokens saved by the token economy so far, when reported.
        tokens_saved: Option<u64>,
        /// Cumulative efficiency ratio so far, when reported.
        efficiency_ratio: Option<f64>,
    },
    /// Incremental text from the model stream.
    StreamDelta { text: String },
}

impl PaneEvent {
    /// Deserialises a [`PaneEvent`] from a raw JSON line received from smdjad.
    ///
    /// The line is decoded through [`smedja_agent_events::AgentEventEnvelope`],
    /// the single source of truth for the push-socket wire contract, and the
    /// typed [`AgentEvent`] is mapped onto the renderer-facing [`PaneEvent`].
    ///
    /// Returns `None` for unparseable input or an unknown event type — a
    /// malformed line never panics or takes down the receiver. Fields that the
    /// wire schema does not carry (model tier, token counts, latency) default
    /// to empty/zero so existing renderer code keeps working.
    #[must_use]
    pub fn from_json_line(line: &str) -> Option<Self> {
        use smedja_agent_events::AgentEvent;

        let Some(envelope) = AgentEventEnvelope::from_json_line(line) else {
            warn!(line, "unparseable or unknown smdjad agent event");
            return None;
        };

        Some(match envelope.event {
            AgentEvent::TurnStart {
                turn_id,
                session_id,
            } => Self::TurnStart {
                session_id: session_id.unwrap_or_default(),
                turn_id: turn_id.unwrap_or_default(),
                tier: String::new(),
                model: String::new(),
                trace_id: None,
                span_id: None,
            },
            AgentEvent::ToolCall {
                turn_id,
                tool,
                summary,
            } => Self::ToolCall {
                tool_name: tool.unwrap_or_default(),
                args_summary: summary.unwrap_or_default(),
                tool_call_id: turn_id,
                trace_id: None,
                span_id: None,
            },
            AgentEvent::ApprovalPrompt { tool, prompt, .. } => Self::ApprovalPrompt {
                tool_name: tool.unwrap_or_default(),
                args: Value::Null,
                prompt: prompt.unwrap_or_default(),
            },
            AgentEvent::ToolResult {
                tool, summary, ok, ..
            } => Self::ToolResult {
                tool_name: tool.unwrap_or_default(),
                outcome: summary.unwrap_or_else(|| {
                    if ok.unwrap_or(false) {
                        "ok".to_owned()
                    } else {
                        String::new()
                    }
                }),
            },
            AgentEvent::TurnEnd {
                tokens_saved,
                efficiency_ratio,
                ..
            } => Self::TurnEnd {
                input_tokens: 0,
                output_tokens: 0,
                latency_ms: 0,
                traceparent: None,
                tokens_saved,
                efficiency_ratio,
            },
            AgentEvent::StreamDelta { content, .. } => Self::StreamDelta {
                text: content.unwrap_or_default(),
            },
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Approval decision
// ─────────────────────────────────────────────────────────────────────────────

/// The user's decision on an [`ApprovalGate`](crate::ApprovalGate) prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The user approved the pending tool call.
    Approve,
    /// The user denied the pending tool call.
    Deny,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope_line(event: smedja_agent_events::AgentEvent) -> String {
        smedja_agent_events::AgentEventEnvelope::new(event).to_json_line()
    }

    #[test]
    fn pane_event_deserialise_turn_start() {
        let line = envelope_line(smedja_agent_events::AgentEvent::TurnStart {
            turn_id: Some("t1".into()),
            session_id: Some("s1".into()),
        });
        let event = PaneEvent::from_json_line(&line).expect("should parse");
        if let PaneEvent::TurnStart {
            session_id,
            turn_id,
            tier,
            model,
            ..
        } = event
        {
            assert_eq!(session_id, "s1");
            assert_eq!(turn_id, "t1");
            // The wire schema does not carry tier/model; they default to empty.
            assert_eq!(tier, "");
            assert_eq!(model, "");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_deserialise_tool_call() {
        let line = envelope_line(smedja_agent_events::AgentEvent::ToolCall {
            turn_id: Some("t1".into()),
            tool: Some("bash".into()),
            summary: Some("ls -la".into()),
        });
        let event = PaneEvent::from_json_line(&line).expect("should parse");
        if let PaneEvent::ToolCall {
            tool_name,
            args_summary,
            ..
        } = event
        {
            assert_eq!(tool_name, "bash");
            assert_eq!(args_summary, "ls -la");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_deserialise_approval_prompt() {
        let line = envelope_line(smedja_agent_events::AgentEvent::ApprovalPrompt {
            turn_id: Some("t1".into()),
            tool: Some("rm".into()),
            prompt: Some("Allow deletion?".into()),
        });
        let event = PaneEvent::from_json_line(&line).expect("should parse");
        if let PaneEvent::ApprovalPrompt {
            tool_name, prompt, ..
        } = event
        {
            assert_eq!(tool_name, "rm");
            assert_eq!(prompt, "Allow deletion?");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_deserialise_tool_result() {
        let line = envelope_line(smedja_agent_events::AgentEvent::ToolResult {
            turn_id: Some("t1".into()),
            tool: Some("read".into()),
            summary: Some("12 lines".into()),
            ok: Some(true),
        });
        let event = PaneEvent::from_json_line(&line).expect("should parse");
        if let PaneEvent::ToolResult { tool_name, outcome } = event {
            assert_eq!(tool_name, "read");
            assert_eq!(outcome, "12 lines");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_deserialise_turn_end() {
        let line = envelope_line(smedja_agent_events::AgentEvent::TurnEnd {
            turn_id: Some("t1".into()),
            session_id: Some("s1".into()),
            tokens_saved: None,
            efficiency_ratio: None,
        });
        let event = PaneEvent::from_json_line(&line).expect("should parse");
        assert!(matches!(event, PaneEvent::TurnEnd { .. }));
    }

    #[test]
    fn pane_event_turn_end_carries_savings_figure() {
        let line = envelope_line(smedja_agent_events::AgentEvent::TurnEnd {
            turn_id: Some("t1".into()),
            session_id: Some("s1".into()),
            tokens_saved: Some(5000),
            efficiency_ratio: Some(0.3),
        });
        let event = PaneEvent::from_json_line(&line).expect("should parse");
        match event {
            PaneEvent::TurnEnd {
                tokens_saved,
                efficiency_ratio,
                ..
            } => {
                assert_eq!(tokens_saved, Some(5000));
                assert_eq!(efficiency_ratio, Some(0.3));
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn pane_event_deserialise_stream_delta() {
        let line = envelope_line(smedja_agent_events::AgentEvent::StreamDelta {
            turn_id: Some("t1".into()),
            content: Some("partial".into()),
        });
        let event = PaneEvent::from_json_line(&line).expect("should parse");
        if let PaneEvent::StreamDelta { text } = event {
            assert_eq!(text, "partial");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_unknown_type_returns_none() {
        assert!(PaneEvent::from_json_line(r#"{"type":"nope"}"#).is_none());
        assert!(PaneEvent::from_json_line("not json").is_none());
    }

    /// A legacy payload lacking a `schema_version` field still decodes via the
    /// envelope's `#[serde(default)]` version handling, and maps to the right
    /// variant — exercising backward compatibility on the receive path.
    #[test]
    fn pane_event_decodes_legacy_versionless_line() {
        let line = r#"{"type":"turn_start","turn_id":"t0","session_id":"old"}"#;
        let event = PaneEvent::from_json_line(line).expect("legacy line must decode");
        if let PaneEvent::TurnStart {
            session_id,
            turn_id,
            ..
        } = event
        {
            assert_eq!(session_id, "old");
            assert_eq!(turn_id, "t0");
        } else {
            panic!("expected TurnStart");
        }
    }
}
