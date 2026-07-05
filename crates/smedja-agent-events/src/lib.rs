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
///
/// Version 3 adds `input_tokens`, `output_tokens`, `latency_ms`, and
/// `traceparent` to [`AgentEvent::TurnEnd`]. All four are optional and
/// `skip_serializing_if` absent, so the wire form stays byte-compatible with
/// v2 receivers when the daemon has no figure to report.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

/// A single agent event in the push-socket stream.
///
/// Serialised with an internal `type` tag (`snake_case`), e.g.
/// `{ "type": "tool_call", ... }`. Fields a UI does not always have are
/// modelled as [`Option<String>`] so partial payloads remain valid.
// `Eq` is intentionally not derived: `TurnEnd::efficiency_ratio` is an `f64`,
// which is not `Eq`. `PartialEq` is sufficient for the wire round-trip tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        /// Cumulative tokens saved by the token economy so far (all sources),
        /// added in schema version 2. `None` on version-0/1 payloads that
        /// predate the field, so the receiver renders no segment rather than a
        /// misleading zero.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_saved: Option<u64>,
        /// Cumulative efficiency ratio `saved / (saved + billed_input)` so far,
        /// added in schema version 2. `None` on payloads that predate the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        efficiency_ratio: Option<f64>,
        /// Input (prompt) token count for the turn, added in schema version 3.
        /// `None` when the daemon has no figure to report.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        /// Output (completion) token count for the turn, added in schema
        /// version 3. `None` when the daemon has no figure to report.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Wall-clock turn latency in milliseconds, added in schema version 3.
        /// `None` when the daemon could not measure the turn duration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latency_ms: Option<u64>,
        /// W3C `traceparent` header from the turn's root span, added in schema
        /// version 3. `None` when tracing produced no traceparent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
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
// `Eq` is intentionally not derived: the wrapped `AgentEvent` carries an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
            tokens_saved: None,
            efficiency_ratio: None,
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
            traceparent: None,
        });
    }

    #[test]
    fn round_trip_turn_end_with_savings() {
        round_trip(AgentEvent::TurnEnd {
            turn_id: Some("t1".to_owned()),
            session_id: Some("s1".to_owned()),
            tokens_saved: Some(123_456),
            efficiency_ratio: Some(0.42),
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
            traceparent: None,
        });
    }

    #[test]
    fn round_trip_turn_end_with_v3_metrics() {
        // Exercise serialization of the schema-v3 token/latency/trace fields.
        round_trip(AgentEvent::TurnEnd {
            turn_id: Some("t1".to_owned()),
            session_id: Some("s1".to_owned()),
            tokens_saved: Some(123_456),
            efficiency_ratio: Some(0.42),
            input_tokens: Some(412),
            output_tokens: Some(88),
            latency_ms: Some(4200),
            traceparent: Some("00-abc123def456-0102030405060708-01".to_owned()),
        });
    }

    #[test]
    fn schema_version_is_three() {
        assert_eq!(CURRENT_SCHEMA_VERSION, 3);
    }

    #[test]
    fn legacy_turn_end_without_savings_fields_decodes() {
        // A schema-version-1 TurnEnd line predates tokens_saved/efficiency_ratio;
        // it must still decode with those fields defaulting to None.
        let line = r#"{"schema_version":1,"type":"turn_end","turn_id":"t1","session_id":"s1"}"#;
        let decoded = AgentEventEnvelope::from_json_line(line).expect("legacy v1 must decode");
        assert_eq!(decoded.schema_version, 1);
        assert_eq!(
            decoded.event,
            AgentEvent::TurnEnd {
                turn_id: Some("t1".to_owned()),
                session_id: Some("s1".to_owned()),
                tokens_saved: None,
                efficiency_ratio: None,
                input_tokens: None,
                output_tokens: None,
                latency_ms: None,
                traceparent: None,
            }
        );
    }

    #[test]
    fn turn_end_without_savings_omits_fields_on_wire() {
        // skip_serializing_if keeps the wire form identical to v1 when the
        // savings fields are absent, so older receivers are unaffected.
        let line = AgentEventEnvelope::new(AgentEvent::TurnEnd {
            turn_id: Some("t1".to_owned()),
            session_id: Some("s1".to_owned()),
            tokens_saved: None,
            efficiency_ratio: None,
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
            traceparent: None,
        })
        .to_json_line();
        assert!(!line.contains("tokens_saved"));
        assert!(!line.contains("efficiency_ratio"));
        assert!(!line.contains("input_tokens"));
        assert!(!line.contains("output_tokens"));
        assert!(!line.contains("latency_ms"));
        assert!(!line.contains("traceparent"));
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
