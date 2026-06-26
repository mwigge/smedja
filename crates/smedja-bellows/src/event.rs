/// Correlation context shared across every event variant.
///
/// These seven optional fields tie an event back to a conversation, a
/// distributed trace, and the emitting agent.  They are embedded once per
/// event variant via `#[serde(flatten)]`, so on the wire they appear as
/// top-level fields of the event object — identical to the historical layout
/// where each variant spelled them out individually.
///
/// Every field is omitted from serialised JSON when `None`, which keeps the
/// wire format compact and preserves backward compatibility with older
/// daemons that never emitted them.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CorrelationCtx {
    /// Conversation grouping identifier across multiple turns or agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// W3C trace-id for distributed tracing correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// W3C span-id for the span that produced this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Parent span identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    /// High-level operation label (e.g. `"chat"`, `"invoke_agent"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
    /// Name of the agent that emitted this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Terminal status of the operation: `"ok"` or `"error"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Events emitted during the lifecycle of an agent turn.
///
/// Each variant corresponds to a distinct point in the turn's progression —
/// from initiation through tool use and streaming deltas to final resolution.
///
/// All variants embed a [`CorrelationCtx`] (`conversation_id`, `trace_id`,
/// `span_id`, `parent_span_id`, `operation_name`, `agent_name`, `status`) via
/// `#[serde(flatten)]`, so those fields appear at the top level of the
/// serialised event object.  Tool-related variants additionally carry
/// `tool_call_id`.  Correlation fields are omitted from serialised JSON when
/// `None`, which keeps the wire format compact and preserves backward
/// compatibility with older daemons.
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
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// A tool was invoked during this turn.
    ToolCalled {
        /// The name of the tool that was called.
        tool_name: String,
        /// A short, human-readable description of the tool's input.
        input_summary: String,
        /// The full tool input (capped), for on-demand detail views. Optional so
        /// older producers and non-provider tool paths can omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full_input: Option<String>,
        /// Turn identifier; correlates this event with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
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
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// An incremental chunk of the model's internal reasoning (thinking tokens).
    ///
    /// Only emitted when the underlying model supports extended thinking.
    /// The TUI accumulates these into a collapsible "thinking" block that is
    /// shown while the turn is in flight and collapsed (but togglable) once
    /// the turn completes.
    ThinkingDelta {
        /// The incremental thinking-token text chunk.
        content: String,
        /// Turn identifier; correlates this delta with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
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
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// The turn failed.
    Failed {
        /// Identifier for the session this turn belongs to.
        session_id: String,
        /// Unique identifier for the turn that failed.
        turn_id: String,
        /// Human-readable description of why the turn failed.
        reason: String,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },
}

// ── Constructors ──────────────────────────────────────────────────────────────

impl TurnEvent {
    /// Construct a [`TurnEvent::Failed`] with a default (all-`None`)
    /// [`CorrelationCtx`].
    ///
    /// Use this instead of spelling out the correlation context at every call
    /// site.  Correlation fields can be set afterwards via struct-update syntax
    /// or by constructing the variant directly when they are needed.
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
            correlation: CorrelationCtx::default(),
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
            correlation: CorrelationCtx::default(),
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
            correlation: CorrelationCtx {
                conversation_id: Some("conv-abc".into()),
                trace_id: Some("trace-xyz".into()),
                span_id: Some("span-123".into()),
                operation_name: Some("invoke_agent".into()),
                agent_name: Some("tester".into()),
                ..CorrelationCtx::default()
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::Started { correlation, .. } = decoded {
            assert_eq!(correlation.conversation_id.as_deref(), Some("conv-abc"));
            assert_eq!(correlation.agent_name.as_deref(), Some("tester"));
            assert_eq!(correlation.trace_id.as_deref(), Some("trace-xyz"));
            assert_eq!(correlation.span_id.as_deref(), Some("span-123"));
            assert!(correlation.parent_span_id.is_none());
            assert!(correlation.status.is_none());
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
        if let TurnEvent::Started { correlation, .. } = ev {
            assert!(correlation.conversation_id.is_none());
            assert!(correlation.trace_id.is_none());
            assert!(correlation.agent_name.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn legacy_fieldless_event_decodes_to_all_none_correlation() {
        // Task 8.4 (b): legacy JSON without any correlation fields must decode
        // with every CorrelationCtx field set to None.
        let legacy = r#"{"Completed":{"session_id":"s","turn_id":"t","output_tokens":3}}"#;
        let ev: TurnEvent = serde_json::from_str(legacy).unwrap();
        if let TurnEvent::Completed {
            correlation,
            input_tokens,
            traceparent,
            ..
        } = ev
        {
            assert_eq!(correlation, CorrelationCtx::default());
            assert!(correlation.conversation_id.is_none());
            assert!(correlation.trace_id.is_none());
            assert!(correlation.span_id.is_none());
            assert!(correlation.parent_span_id.is_none());
            assert!(correlation.operation_name.is_none());
            assert!(correlation.agent_name.is_none());
            assert!(correlation.status.is_none());
            assert!(input_tokens.is_none());
            assert!(traceparent.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn fully_populated_event_serializes_correlation_fields_at_top_level() {
        // Task 8.4 (a): a fully-populated event must serialise with the seven
        // correlation fields flattened to the top level of the event object —
        // the same wire shape as the historical per-variant layout.
        let ev = TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "t".into(),
            correlation: CorrelationCtx {
                conversation_id: Some("conv".into()),
                trace_id: Some("tr".into()),
                span_id: Some("sp".into()),
                parent_span_id: Some("psp".into()),
                operation_name: Some("op".into()),
                agent_name: Some("agent".into()),
                status: Some("ok".into()),
            },
        };
        let value: serde_json::Value = serde_json::to_value(&ev).unwrap();
        let inner = value.get("Started").expect("Started object must exist");
        for field in [
            "conversation_id",
            "trace_id",
            "span_id",
            "parent_span_id",
            "operation_name",
            "agent_name",
            "status",
        ] {
            assert!(
                inner.get(field).is_some(),
                "{field} must appear at the top level of the event object; got: {value}"
            );
        }
        // There must be no nested `correlation` key — the context is flattened.
        assert!(
            inner.get("correlation").is_none(),
            "correlation must be flattened, not nested; got: {value}"
        );
    }

    #[test]
    fn tool_called_carries_tool_call_id() {
        let ev = TurnEvent::ToolCalled {
            tool_name: "bash".into(),
            input_summary: "ls".into(),
            full_input: None,
            turn_id: None,
            correlation: CorrelationCtx {
                trace_id: Some("t".into()),
                ..CorrelationCtx::default()
            },
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
            correlation: CorrelationCtx {
                conversation_id: Some("conv-1".into()),
                agent_name: Some("orchestrator".into()),
                status: Some("ok".into()),
                ..CorrelationCtx::default()
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::Completed {
            correlation,
            input_tokens,
            traceparent,
            ..
        } = decoded
        {
            assert_eq!(correlation.conversation_id.as_deref(), Some("conv-1"));
            assert_eq!(correlation.agent_name.as_deref(), Some("orchestrator"));
            assert_eq!(correlation.status.as_deref(), Some("ok"));
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
            correlation: CorrelationCtx {
                trace_id: Some("tr".into()),
                span_id: Some("sp".into()),
                status: Some("error".into()),
                ..CorrelationCtx::default()
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::Failed { correlation, .. } = decoded {
            assert_eq!(correlation.status.as_deref(), Some("error"));
            assert_eq!(correlation.trace_id.as_deref(), Some("tr"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn assistant_delta_correlation_fields_roundtrip() {
        let ev = TurnEvent::AssistantDelta {
            content: "hello".into(),
            turn_id: None,
            correlation: CorrelationCtx {
                conversation_id: Some("c".into()),
                ..CorrelationCtx::default()
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::AssistantDelta {
            content,
            correlation,
            ..
        } = decoded
        {
            assert_eq!(content, "hello");
            assert_eq!(correlation.conversation_id.as_deref(), Some("c"));
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
            correlation,
        } = ev
        {
            assert_eq!(session_id, "sess-fail");
            assert_eq!(turn_id, "turn-fail");
            assert_eq!(reason, "something went wrong");
            assert_eq!(
                correlation,
                CorrelationCtx::default(),
                "correlation must default to all-None"
            );
        } else {
            panic!("TurnEvent::fail must produce TurnEvent::Failed");
        }
    }

    #[test]
    fn thinking_delta_roundtrips_via_json() {
        let ev = TurnEvent::ThinkingDelta {
            content: "step one: reason about the problem".into(),
            turn_id: Some("t-think".into()),
            correlation: CorrelationCtx::default(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("ThinkingDelta"),
            "variant tag must appear in JSON; got: {json}"
        );
        assert!(
            json.contains("step one"),
            "content must be serialised; got: {json}"
        );
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::ThinkingDelta {
            content, turn_id, ..
        } = decoded
        {
            assert_eq!(content, "step one: reason about the problem");
            assert_eq!(turn_id.as_deref(), Some("t-think"));
        } else {
            panic!("wrong variant after roundtrip");
        }
    }

    #[test]
    fn thinking_delta_omits_none_fields() {
        let ev = TurnEvent::ThinkingDelta {
            content: "pondering".into(),
            turn_id: None,
            correlation: CorrelationCtx::default(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            !json.contains("turn_id"),
            "None turn_id must be omitted; got: {json}"
        );
        assert!(
            !json.contains("trace_id"),
            "None correlation fields must be omitted; got: {json}"
        );
    }
}
