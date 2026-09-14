//! Event conversion: the smdjad push-socket wire contract mapped onto the
//! renderer-facing [`PaneEvent`], plus the user's [`ApprovalDecision`].

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
        /// Turn this prompt belongs to, when the wire payload carries it.
        /// Receivers should key their session lookup by this turn (falling
        /// back to their current turn) so a late or reordered prompt is not
        /// filed under the wrong turn — or dropped when no turn is open.
        turn_id: Option<String>,
        /// Approval identifier to echo back via
        /// [`SmdjadClient::send_approval`](crate::SmdjadClient::send_approval).
        /// `None` on pre-v4 wire payloads; such prompts cannot be answered.
        approval_id: Option<String>,
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
    /// A pending approval was resolved elsewhere (another pane/client, or a
    /// gate timeout) — the daemon scans all gates by approval id, so this
    /// notice is session-agnostic. The matching local prompt must stop
    /// intercepting keys.
    ApprovalResolved {
        /// Approval identifier that was resolved.
        approval_id: String,
    },
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

        // Check the session-agnostic resolution notice against the raw JSON
        // first: the typed schema may predate the `ApprovalResolved` variant
        // (added by the daemon after schema v4), and this receiver must clear
        // resolved-elsewhere prompts with either build.
        if let Some(ev) = approval_resolved_from_raw(line) {
            return Some(ev);
        }

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
            AgentEvent::ApprovalPrompt {
                tool,
                prompt,
                approval_id,
                turn_id,
            } => {
                let prompt = prompt.unwrap_or_default();
                // smdjad puts the scrubbed args display in the prompt text;
                // when it parses as JSON, recover it as the structured args so
                // renderers (e.g. ApprovalGate) can show real arguments instead
                // of a hard-dropped Null.
                let args = serde_json::from_str(&prompt).unwrap_or(Value::Null);
                Self::ApprovalPrompt {
                    tool_name: tool.unwrap_or_default(),
                    args,
                    prompt,
                    turn_id,
                    approval_id,
                }
            }
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
                input_tokens,
                output_tokens,
                latency_ms,
                traceparent,
                ..
            } => Self::TurnEnd {
                input_tokens: input_tokens.unwrap_or(0),
                output_tokens: output_tokens.unwrap_or(0),
                latency_ms: latency_ms.unwrap_or(0),
                traceparent,
                tokens_saved,
                efficiency_ratio,
            },
            AgentEvent::StreamDelta { content, .. } => Self::StreamDelta {
                text: content.unwrap_or_default(),
            },
            // Forward compatibility: variants added to the wire schema after
            // this build decode here once the typed schema knows them.
            // `ApprovalResolved` is already caught from the raw line above, so
            // anything reaching this arm has no rendering yet and is ignored.
            #[allow(unreachable_patterns)]
            _ => return None,
        })
    }
}

/// Extracts an [`PaneEvent::ApprovalResolved`] from a raw wire line.
///
/// The daemon emits `{"type":"approval_resolved","approval_id":"…"}` when a
/// gate resolves session-agnostically. Parsed from the raw JSON so this
/// receiver copes whether or not the typed `smedja-agent-events` schema
/// carries the variant yet.
fn approval_resolved_from_raw(line: &str) -> Option<PaneEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("approval_resolved") {
        return None;
    }
    let approval_id = v
        .get("approval_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_owned();
    Some(PaneEvent::ApprovalResolved { approval_id })
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
