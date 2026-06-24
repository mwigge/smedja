//! Versioned, serde-derived wire schema for agent push-socket events.
//!
//! This crate is the single source of truth for the line-delimited JSON
//! contract spoken between the agent daemon emitter (`smdjad`) and the
//! terminal receiver (`st-agent`). Every event is wrapped in an
//! [`AgentEventEnvelope`] carrying a [`schema_version`](AgentEventEnvelope)
//! so receivers can detect and tolerate version drift.
//!
//! Wire format: one [`AgentEventEnvelope`] encoded as a single line of JSON
//! (no embedded newlines), terminated by `\n` on the socket.

use serde::{Deserialize, Serialize};

/// The schema version this build emits.
///
/// Bump this whenever the wire contract changes in a way receivers must
/// notice. Legacy payloads lacking a version field decode as version `0`.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// A single agent event in the push-socket stream.
///
/// Serialised with an internal `type` tag (`snake_case`), e.g.
/// `{ "type": "tool_call", ... }`. Fields a UI does not always have are
/// modelled as [`Option<String>`] so partial payloads remain valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A new agent turn has started.
    TurnStart {
        /// Identifier of the turn being started.
        turn_id: Option<String>,
        /// Owning session identifier.
        session_id: Option<String>,
    },
    /// The agent is invoking a tool.
    ToolCall {
        /// Turn this tool call belongs to.
        turn_id: Option<String>,
        /// Name of the tool being called.
        tool: Option<String>,
        /// Short human-readable summary of the call (e.g. arguments preview).
        summary: Option<String>,
    },
    /// The agent is requesting user approval before proceeding.
    ApprovalPrompt {
        /// Turn this prompt belongs to.
        turn_id: Option<String>,
        /// Tool or action awaiting approval.
        tool: Option<String>,
        /// Prompt text shown to the user.
        prompt: Option<String>,
    },
    /// A tool invocation has returned a result.
    ToolResult {
        /// Turn this result belongs to.
        turn_id: Option<String>,
        /// Name of the tool that produced the result.
        tool: Option<String>,
        /// Short human-readable summary of the result.
        summary: Option<String>,
        /// Whether the tool invocation succeeded.
        ok: Option<bool>,
    },
    /// The agent turn has ended.
    TurnEnd {
        /// Identifier of the turn that ended.
        turn_id: Option<String>,
        /// Owning session identifier.
        session_id: Option<String>,
    },
    /// An incremental chunk of streamed assistant output.
    StreamDelta {
        /// Turn this delta belongs to.
        turn_id: Option<String>,
        /// The text fragment to append to the rendered output.
        content: Option<String>,
    },
}

/// Versioned envelope wrapping a single [`AgentEvent`].
///
/// The [`schema_version`](Self::schema_version) field defaults to `0` so that
/// legacy fieldless payloads (emitted before versioning existed) still decode.
/// The event itself is flattened, so the wire form is a single flat JSON
/// object combining the version and the tagged event fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    /// Wire schema version. Defaults to `0` for legacy payloads.
    #[serde(default)]
    pub schema_version: u32,
    /// The wrapped event.
    #[serde(flatten)]
    pub event: AgentEvent,
}

impl AgentEventEnvelope {
    /// Wraps an [`AgentEvent`] with the current schema version.
    #[must_use]
    pub fn new(event: AgentEvent) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            event,
        }
    }

    /// Serialises this envelope to a single line of JSON (no trailing newline).
    ///
    /// # Panics
    ///
    /// Does not panic in practice: every field is a plain string/bool/integer,
    /// so serialisation cannot fail; the unreachable error path falls back to
    /// an empty JSON object.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Parses a single JSON line into an envelope.
    ///
    /// Returns [`None`] for unparseable input or an unknown event `type`,
    /// rather than panicking, so a malformed line never takes down a receiver.
    #[must_use]
    pub fn from_json_line(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentEvent, AgentEventEnvelope, CURRENT_SCHEMA_VERSION};

    fn round_trip(event: AgentEvent) {
        let envelope = AgentEventEnvelope::new(event);
        let line = envelope.to_json_line();
        assert!(!line.contains('\n'), "wire line must be newline-free");
        let decoded = AgentEventEnvelope::from_json_line(&line).expect("envelope must round-trip");
        assert_eq!(decoded, envelope);
        assert_eq!(decoded.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn round_trip_turn_start() {
        round_trip(AgentEvent::TurnStart {
            turn_id: Some("t1".to_owned()),
            session_id: Some("s1".to_owned()),
        });
    }

    #[test]
    fn round_trip_tool_call() {
        round_trip(AgentEvent::ToolCall {
            turn_id: Some("t1".to_owned()),
            tool: Some("read".to_owned()),
            summary: Some("read /etc/hosts".to_owned()),
        });
    }

    #[test]
    fn round_trip_approval_prompt() {
        round_trip(AgentEvent::ApprovalPrompt {
            turn_id: Some("t1".to_owned()),
            tool: Some("bash".to_owned()),
            prompt: Some("run rm -rf?".to_owned()),
        });
    }

    #[test]
    fn round_trip_tool_result() {
        round_trip(AgentEvent::ToolResult {
            turn_id: Some("t1".to_owned()),
            tool: Some("read".to_owned()),
            summary: Some("12 lines".to_owned()),
            ok: Some(true),
        });
    }

    #[test]
    fn round_trip_turn_end() {
        round_trip(AgentEvent::TurnEnd {
            turn_id: Some("t1".to_owned()),
            session_id: Some("s1".to_owned()),
        });
    }

    #[test]
    fn round_trip_stream_delta() {
        round_trip(AgentEvent::StreamDelta {
            turn_id: Some("t1".to_owned()),
            content: Some("partial output".to_owned()),
        });
    }

    #[test]
    fn legacy_line_without_version_decodes_as_zero() {
        let line = r#"{"type":"turn_start","turn_id":"t1","session_id":"s1"}"#;
        let decoded = AgentEventEnvelope::from_json_line(line).expect("legacy line must decode");
        assert_eq!(decoded.schema_version, 0);
        assert_eq!(
            decoded.event,
            AgentEvent::TurnStart {
                turn_id: Some("t1".to_owned()),
                session_id: Some("s1".to_owned()),
            }
        );
    }

    #[test]
    fn unknown_type_yields_none() {
        let line = r#"{"type":"does_not_exist","turn_id":"t1"}"#;
        assert!(AgentEventEnvelope::from_json_line(line).is_none());
    }

    #[test]
    fn unparseable_line_yields_none() {
        assert!(AgentEventEnvelope::from_json_line("not json at all").is_none());
    }
}
