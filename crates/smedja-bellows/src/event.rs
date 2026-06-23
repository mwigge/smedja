/// Events emitted during the lifecycle of an agent turn.
///
/// Each variant corresponds to a distinct point in the turn's progression —
/// from initiation through tool use and streaming deltas to final resolution.
///
/// All variants carry optional correlation fields (`conversation_id`,
/// `trace_id`, `span_id`, `parent_span_id`, `operation_name`, `agent_name`,
/// `status`).  Tool-related variants additionally carry `tool_call_id`.
/// These fields are omitted from serialised JSON when `None`, which keeps the
/// wire format compact and preserves backward compatibility with older daemons.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TurnEvent {
    /// The turn has started.
    ///
    /// Carries the session and turn identifiers so subscribers can correlate
    /// subsequent events to a particular turn.
    Started {
        /// Identifier for the session this turn belongs to.
        session_id: String,
        /// Unique identifier for this turn within the session.
        turn_id: String,
        /// Conversation grouping identifier across multiple turns or agents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label (e.g. `"chat"`, `"invoke_agent"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation: `"ok"` or `"error"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },

    /// A tool was invoked during this turn.
    ToolCalled {
        /// The name of the tool that was called.
        tool_name: String,
        /// A short, human-readable description of the tool's input.
        input_summary: String,
        /// Turn identifier; correlates this event with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Conversation grouping identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// Tool-call identifier for correlating the call with its result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },

    /// The assistant produced a text delta (streaming output).
    AssistantDelta {
        /// The incremental text content emitted by the assistant.
        content: String,
        /// Turn identifier; correlates this delta with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Conversation grouping identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },

    /// The turn completed successfully.
    Completed {
        /// Identifier for the session this turn belongs to.
        session_id: String,
        /// Unique identifier for the turn that completed.
        turn_id: String,
        /// Number of output tokens generated during this turn.
        output_tokens: u32,
        /// Number of input tokens consumed during this turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u32>,
        /// W3C `traceparent` string for this turn's root span.
        ///
        /// Format: `"00-<trace_id_hex32>-<span_id_hex16>-01"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
        /// Conversation grouping identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },

    /// The turn failed.
    Failed {
        /// Identifier for the session this turn belongs to.
        session_id: String,
        /// Unique identifier for the turn that failed.
        turn_id: String,
        /// Human-readable description of why the turn failed.
        reason: String,
        /// Conversation grouping identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

// ── ControlEvent ──────────────────────────────────────────────────────────────

/// Low-frequency lifecycle events for an agent turn.
///
/// Subscribers that only care about turn lifecycle (started, completed, failed,
/// tool calls) should receive [`ControlEvent`] rather than [`TurnEvent`] to
/// avoid filtering thousands of high-frequency [`DeltaEvent`]s.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    /// The turn has started.
    Started {
        /// Identifier for the session this turn belongs to.
        session_id: String,
        /// Unique identifier for this turn within the session.
        turn_id: String,
        /// Conversation grouping identifier across multiple turns or agents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label (e.g. `"chat"`, `"invoke_agent"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation: `"ok"` or `"error"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },

    /// A tool was invoked during this turn.
    ToolCalled {
        /// The name of the tool that was called.
        tool_name: String,
        /// A short, human-readable description of the tool's input.
        input_summary: String,
        /// Turn identifier; correlates this event with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Conversation grouping identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// Tool-call identifier for correlating the call with its result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },

    /// The turn completed successfully.
    Completed {
        /// Identifier for the session this turn belongs to.
        session_id: String,
        /// Unique identifier for the turn that completed.
        turn_id: String,
        /// Number of output tokens generated during this turn.
        output_tokens: u32,
        /// Number of input tokens consumed during this turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u32>,
        /// W3C `traceparent` string for this turn's root span.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
        /// Conversation grouping identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },

    /// The turn failed.
    Failed {
        /// Identifier for the session this turn belongs to.
        session_id: String,
        /// Unique identifier for the turn that failed.
        turn_id: String,
        /// Human-readable description of why the turn failed.
        reason: String,
        /// Conversation grouping identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        /// W3C span-id for the span that produced this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        /// Parent span identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        /// High-level operation label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_name: Option<String>,
        /// Name of the agent that emitted this event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Terminal status of the operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

// ── DeltaEvent ────────────────────────────────────────────────────────────────

/// A high-frequency streaming text token emitted by the assistant.
///
/// Subscribers interested only in token-streaming should subscribe to a
/// [`DeltaEvent`] channel rather than [`TurnEvent`] to avoid receiving
/// low-frequency lifecycle overhead.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeltaEvent {
    /// Identifier for the session this delta belongs to.
    pub session_id: Option<String>,
    /// Turn identifier; correlates this delta with the enclosing turn.
    pub turn_id: Option<String>,
    /// The incremental text content emitted by the assistant.
    pub delta: String,
}

// ── TryFrom<TurnEvent> for ControlEvent ───────────────────────────────────────

/// Converts a [`TurnEvent`] into a [`ControlEvent`].
///
/// Returns `Err(TurnEvent::AssistantDelta { .. })` when the source event is an
/// `AssistantDelta`, since that variant has no [`ControlEvent`] counterpart.
impl TryFrom<TurnEvent> for ControlEvent {
    type Error = TurnEvent;

    fn try_from(event: TurnEvent) -> Result<Self, Self::Error> {
        match event {
            TurnEvent::Started {
                session_id,
                turn_id,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
            } => Ok(ControlEvent::Started {
                session_id,
                turn_id,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
            }),
            TurnEvent::ToolCalled {
                tool_name,
                input_summary,
                turn_id,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
                tool_call_id,
            } => Ok(ControlEvent::ToolCalled {
                tool_name,
                input_summary,
                turn_id,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
                tool_call_id,
            }),
            TurnEvent::Completed {
                session_id,
                turn_id,
                output_tokens,
                input_tokens,
                traceparent,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
            } => Ok(ControlEvent::Completed {
                session_id,
                turn_id,
                output_tokens,
                input_tokens,
                traceparent,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
            }),
            TurnEvent::Failed {
                session_id,
                turn_id,
                reason,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
            } => Ok(ControlEvent::Failed {
                session_id,
                turn_id,
                reason,
                conversation_id,
                trace_id,
                span_id,
                parent_span_id,
                operation_name,
                agent_name,
                status,
            }),
            delta @ TurnEvent::AssistantDelta { .. } => Err(delta),
        }
    }
}

// ── From<TurnEvent> for Option<DeltaEvent> ────────────────────────────────────

/// Converts a [`TurnEvent`] into an `Option<DeltaEvent>`.
///
/// Returns `Some` only when the source event is `AssistantDelta`.
/// All other variants produce `None`.
impl From<TurnEvent> for Option<DeltaEvent> {
    fn from(event: TurnEvent) -> Self {
        match event {
            TurnEvent::AssistantDelta {
                content,
                turn_id,
                conversation_id: _,
                trace_id: _,
                span_id: _,
                parent_span_id: _,
                operation_name: _,
                agent_name: _,
                status: _,
            } => Some(DeltaEvent {
                session_id: None,
                turn_id,
                delta: content,
            }),
            _ => None,
        }
    }
}

// ── Constructors ──────────────────────────────────────────────────────────────

impl TurnEvent {
    /// Construct a [`TurnEvent::Failed`] with all optional correlation fields set
    /// to `None`.
    ///
    /// Use this instead of spelling out nine `field: None` entries at every call
    /// site.  Correlation fields can be set via struct-update syntax or by
    /// constructing the variant directly when they are needed.
    #[must_use]
    pub fn fail(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        TurnEvent::Failed {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            reason: reason.into(),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_serializes_without_optional_fields() {
        // A Started event with only required fields must produce compact JSON
        // — None optional correlation fields must be absent from the output.
        let ev = TurnEvent::Started {
            session_id: "sess-1".into(),
            turn_id: "t-1".into(),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            !json.contains("conversation_id"),
            "None fields must be skipped in JSON; got: {json}"
        );
        assert!(
            !json.contains("trace_id"),
            "None fields must be skipped in JSON; got: {json}"
        );
        assert!(
            json.contains("sess-1"),
            "session_id must appear; got: {json}"
        );
    }

    #[test]
    fn started_with_correlation_fields_roundtrips() {
        let ev = TurnEvent::Started {
            session_id: "sess-2".into(),
            turn_id: "t-2".into(),
            conversation_id: Some("conv-abc".into()),
            trace_id: Some("trace-xyz".into()),
            span_id: Some("span-123".into()),
            parent_span_id: None,
            operation_name: Some("invoke_agent".into()),
            agent_name: Some("tester".into()),
            status: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::Started {
            conversation_id,
            agent_name,
            trace_id,
            span_id,
            parent_span_id,
            status,
            ..
        } = decoded
        {
            assert_eq!(conversation_id.as_deref(), Some("conv-abc"));
            assert_eq!(agent_name.as_deref(), Some("tester"));
            assert_eq!(trace_id.as_deref(), Some("trace-xyz"));
            assert_eq!(span_id.as_deref(), Some("span-123"));
            assert!(parent_span_id.is_none());
            assert!(status.is_none());
        } else {
            panic!("wrong variant after roundtrip");
        }
    }

    #[test]
    fn old_json_without_correlation_fields_deserializes_to_none() {
        // Simulates an older daemon that does not emit the new optional fields.
        // The new fields must default to None without error.
        let old_json = r#"{"Started":{"session_id":"sess-old","turn_id":"t-old"}}"#;
        let ev: TurnEvent = serde_json::from_str(old_json).unwrap();
        if let TurnEvent::Started {
            conversation_id,
            trace_id,
            agent_name,
            ..
        } = ev
        {
            assert!(conversation_id.is_none());
            assert!(trace_id.is_none());
            assert!(agent_name.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn tool_called_carries_tool_call_id() {
        let ev = TurnEvent::ToolCalled {
            tool_name: "bash".into(),
            input_summary: "ls".into(),
            turn_id: None,
            conversation_id: None,
            trace_id: Some("t".into()),
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
            tool_call_id: Some("call-42".into()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("call-42"),
            "tool_call_id must appear; got: {json}"
        );
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::ToolCalled { tool_call_id, .. } = decoded {
            assert_eq!(tool_call_id.as_deref(), Some("call-42"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn completed_correlation_fields_roundtrip() {
        let ev = TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: "t".into(),
            output_tokens: 7,
            input_tokens: Some(42),
            traceparent: Some("00-abc-def-01".into()),
            conversation_id: Some("conv-1".into()),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: Some("orchestrator".into()),
            status: Some("ok".into()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::Completed {
            conversation_id,
            agent_name,
            status,
            input_tokens,
            traceparent,
            ..
        } = decoded
        {
            assert_eq!(conversation_id.as_deref(), Some("conv-1"));
            assert_eq!(agent_name.as_deref(), Some("orchestrator"));
            assert_eq!(status.as_deref(), Some("ok"));
            assert_eq!(input_tokens, Some(42));
            assert_eq!(traceparent.as_deref(), Some("00-abc-def-01"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn failed_correlation_fields_roundtrip() {
        let ev = TurnEvent::Failed {
            session_id: "s".into(),
            turn_id: "t".into(),
            reason: "timeout".into(),
            conversation_id: None,
            trace_id: Some("tr".into()),
            span_id: Some("sp".into()),
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: Some("error".into()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::Failed {
            status, trace_id, ..
        } = decoded
        {
            assert_eq!(status.as_deref(), Some("error"));
            assert_eq!(trace_id.as_deref(), Some("tr"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn assistant_delta_correlation_fields_roundtrip() {
        let ev = TurnEvent::AssistantDelta {
            content: "hello".into(),
            turn_id: None,
            conversation_id: Some("c".into()),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::AssistantDelta {
            content,
            conversation_id,
            ..
        } = decoded
        {
            assert_eq!(content, "hello");
            assert_eq!(conversation_id.as_deref(), Some("c"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn turn_event_fail_constructor_sets_required_fields() {
        let ev = TurnEvent::fail("sess-fail", "turn-fail", "something went wrong");
        if let TurnEvent::Failed {
            session_id,
            turn_id,
            reason,
            conversation_id,
            trace_id,
            span_id,
            parent_span_id,
            operation_name,
            agent_name,
            status,
        } = ev
        {
            assert_eq!(session_id, "sess-fail");
            assert_eq!(turn_id, "turn-fail");
            assert_eq!(reason, "something went wrong");
            assert!(conversation_id.is_none(), "conversation_id must be None");
            assert!(trace_id.is_none(), "trace_id must be None");
            assert!(span_id.is_none(), "span_id must be None");
            assert!(parent_span_id.is_none(), "parent_span_id must be None");
            assert!(operation_name.is_none(), "operation_name must be None");
            assert!(agent_name.is_none(), "agent_name must be None");
            assert!(status.is_none(), "status must be None");
        } else {
            panic!("TurnEvent::fail must produce TurnEvent::Failed");
        }
    }

    // ── ControlEvent / DeltaEvent conversion tests ────────────────────────

    #[test]
    fn control_event_try_from_started_succeeds() {
        let turn_event = TurnEvent::Started {
            session_id: "sess-ctrl".into(),
            turn_id: "turn-ctrl".into(),
            conversation_id: Some("conv-1".into()),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let result = ControlEvent::try_from(turn_event);
        assert!(result.is_ok(), "TurnEvent::Started must convert to ControlEvent");
        if let Ok(ControlEvent::Started {
            session_id,
            turn_id,
            conversation_id,
            ..
        }) = result
        {
            assert_eq!(session_id, "sess-ctrl");
            assert_eq!(turn_id, "turn-ctrl");
            assert_eq!(conversation_id.as_deref(), Some("conv-1"));
        } else {
            panic!("expected ControlEvent::Started");
        }
    }

    #[test]
    fn control_event_try_from_assistant_delta_fails() {
        let turn_event = TurnEvent::AssistantDelta {
            content: "token".into(),
            turn_id: None,
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let result = ControlEvent::try_from(turn_event);
        assert!(
            result.is_err(),
            "TurnEvent::AssistantDelta must not convert to ControlEvent"
        );
        // The original event is returned as the error value.
        if let Err(TurnEvent::AssistantDelta { content, .. }) = result {
            assert_eq!(content, "token");
        } else {
            panic!("expected Err(TurnEvent::AssistantDelta)");
        }
    }

    #[test]
    fn delta_event_from_turn_event_assistant_delta_succeeds() {
        let turn_event = TurnEvent::AssistantDelta {
            content: "hello delta".into(),
            turn_id: Some("turn-delta".into()),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let result: Option<DeltaEvent> = turn_event.into();
        assert!(result.is_some(), "AssistantDelta must produce Some(DeltaEvent)");
        let delta = result.unwrap();
        assert_eq!(delta.delta, "hello delta");
        assert_eq!(delta.turn_id.as_deref(), Some("turn-delta"));
    }

    #[test]
    fn delta_event_from_turn_event_started_returns_none() {
        let turn_event = TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "t".into(),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        };
        let result: Option<DeltaEvent> = turn_event.into();
        assert!(
            result.is_none(),
            "TurnEvent::Started must produce None for DeltaEvent"
        );
    }
}
