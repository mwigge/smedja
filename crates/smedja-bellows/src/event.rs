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
            ..
        } = decoded
        {
            assert_eq!(conversation_id.as_deref(), Some("conv-1"));
            assert_eq!(agent_name.as_deref(), Some("orchestrator"));
            assert_eq!(status.as_deref(), Some("ok"));
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
}
