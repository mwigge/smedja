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

/// Lifecycle status of an ACP-shaped tool call.
///
/// Mirrors the orchestrator's tool lifecycle and the industry-ACP
/// `tool_call` / `tool_call_update` status field: a call starts `Pending`, moves
/// to `InProgress` once execution begins, and resolves to `Completed` or
/// `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolCallStatus {
    /// The call is queued (e.g. awaiting a permission decision).
    Pending,
    /// The tool is executing.
    InProgress,
    /// The tool finished successfully.
    Completed,
    /// The tool failed or was denied.
    Failed,
}

impl ToolCallStatus {
    /// The industry-ACP wire string (`pending | in_progress | completed |
    /// failed`).
    #[must_use]
    pub fn as_acp_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// One content item attached to a tool-call status update.
///
/// Currently only a proposed file diff for edit tools, which an ACP client
/// (Zed) renders inline so the human can modify-then-approve the change.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    /// A proposed file edit: `old_text` → `new_text` at `path`.
    Diff {
        /// Workspace-relative path of the file being edited.
        path: String,
        /// The file's current contents (empty for a new file).
        old_text: String,
        /// The proposed contents after the edit.
        new_text: String,
    },
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

    /// Tier-1 quality gate snapshot emitted after each completed turn.
    ///
    /// The score (0–100) is the composite of four deterministic gates, each
    /// contributing 25 points.  File and skill advisories are human-readable
    /// strings suitable for direct display in the quality panel.
    ///
    /// This variant is advisory-only: it never blocks the turn loop.  A score
    /// below 60 for two consecutive turns triggers a `CoworkGate` soft interrupt
    /// in the TUI.
    QualitySnapshot {
        /// Composite 0–100 quality score.
        score: u8,
        /// Whether the TDD backstop passed (25 pts).
        tdd_pass: bool,
        /// Whether the clean gate passed (25 pts).
        clean_pass: bool,
        /// Human-readable file-size advisory strings, one per flagged file.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        file_advisories: Vec<String>,
        /// Human-readable skill-inject advisory strings, one per missing skill.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skill_advisories: Vec<String>,
        /// Whether the score was produced by a Tier-2 LLM review (not just
        /// the deterministic Tier-1 gates).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        llm_reviewed: bool,
        /// Turn identifier; correlates this snapshot with the completed turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// Mid-stream token usage delta emitted per-chunk from providers that
    /// report usage before the final `Completed` event.
    ///
    /// `Completed` still carries the definitive cumulative totals; this variant
    /// is informational and may arrive multiple times per turn (e.g. Anthropic
    /// emits one `message_start` usage event and one `message_delta` event).
    TokenUsage {
        /// Input tokens reported by this usage chunk.
        input_tok: u32,
        /// Output tokens reported by this usage chunk.
        output_tok: u32,
        /// Turn identifier; correlates this event with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// A partial chunk of a tool call's input arguments (streaming display only).
    ///
    /// Emitted for each raw `input_json_delta` / `function.arguments` fragment
    /// before the complete `ToolCalled` event.  Consumers that only need
    /// complete tool calls can ignore this variant.
    ToolCallChunk {
        /// Tool name.
        name: String,
        /// Partial argument JSON fragment.
        partial_input: String,
        /// Turn identifier; correlates this event with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// Auto-compaction fired and the conversation history was replaced with a summary.
    ///
    /// Emitted after the summariser model runs and the vault entry is stored.
    /// Subscribers (e.g. the TUI) can use this to display a "context compacted"
    /// notice and know that the raw history before this point is no longer in
    /// the active context window.
    HistoryReplaced {
        /// Identifier for the session whose history was replaced.
        session_id: String,
        /// Turn identifier during which compaction fired.
        turn_id: String,
        /// Approximate token count of the summary stored to the vault.
        #[serde(default)]
        summary_tokens: usize,
    },

    /// A tool call is awaiting human approval at the cowork gate.
    ///
    /// Published by `CoworkGate::intercept` immediately after registering the
    /// pending approval, before suspending.  The TUI receives this via the
    /// NDJSON stream and presents the approval overlay without needing to poll
    /// `cowork.pending`.
    CoworkRequest {
        /// UUID assigned to this approval request; pass to `cowork.approve` /
        /// `cowork.deny` / `cowork.modify`.
        approval_id: String,
        /// Name of the tool awaiting approval.
        tool: String,
        /// Step index within the current turn.
        step_n: u32,
        /// Human-readable serialisation of the scrubbed tool arguments.
        args_display: String,
        /// Agent's reasoning for invoking this tool.
        reasoning: String,
        /// Turn identifier; used to route the event into the correct stream buffer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// A tool call's status transitioned (`pending → in_progress → completed |
    /// failed`), mirroring the orchestrator's tool lifecycle.
    ///
    /// Emitted after the initial [`TurnEvent::ToolCalled`] and correlated to it
    /// by `tool_call_id`. An ACP session stream maps it to a `tool_call_update`
    /// notification. Edit tools carry a [`ToolCallContent::Diff`] so an ACP
    /// client (Zed) can render the proposed change inline for modify-then-approve.
    ToolCallUpdate {
        /// Correlates with the `tool_call_id` of the originating `ToolCalled`.
        tool_call_id: String,
        /// The tool whose status changed.
        tool_name: String,
        /// The new lifecycle status.
        status: ToolCallStatus,
        /// Content items (e.g. a proposed diff for an edit tool). Empty for a
        /// plain status transition.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolCallContent>,
        /// Turn identifier; correlates this event with the enclosing turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// Progress heartbeat from the read-only repo/PR auditor's exploration loop.
    ///
    /// Published once per loop iteration so a streaming client (the TUI `/review`
    /// path) can render a live "reviewing… iteration N/M · examining X · Y
    /// findings" status instead of freezing for the minutes the loop runs. The
    /// `turn_id` carries the audit session id so the stream server routes it to
    /// the subscribing client. Advisory-only: never affects the loop.
    AuditProgress {
        /// 1-based index of the iteration this heartbeat was emitted for.
        iteration: u32,
        /// Upper bound on iterations for this run (the loop's `max_iterations`).
        total: u32,
        /// Short description of what the auditor is currently examining
        /// (e.g. `"read_file src/main.rs"` or `"compiling findings"`).
        activity: String,
        /// Running count of findings gathered so far.
        findings_so_far: u32,
        /// Audit session id; routes the event to the subscribing stream client.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Correlation context (trace, conversation, agent, status).
        #[serde(flatten)]
        correlation: CorrelationCtx,
    },

    /// Terminal audit result: the rendered findings report and severity counts.
    ///
    /// Mirrors how [`TurnEvent::QualitySnapshot`] flows — a single terminal event
    /// the TUI renders as the review report. Published once, after the audit loop
    /// finishes and findings are persisted; it ends the audit stream.
    AuditReport {
        /// The rendered markdown report (the same body the blocking RPC returns).
        report: String,
        /// Per-severity counts, keyed by severity slug
        /// (`{critical, high, medium, low, info}`).
        #[serde(default)]
        counts: serde_json::Value,
        /// Path the report was written to, when `--report` was requested.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report_path: Option<String>,
        /// Audit session id; routes the event to the subscribing stream client.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
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

    #[test]
    fn quality_snapshot_roundtrips_with_all_fields() {
        let ev = TurnEvent::QualitySnapshot {
            score: 75,
            tdd_pass: true,
            clean_pass: true,
            file_advisories: vec!["src/main.rs 7880 L (threshold 600)".into()],
            skill_advisories: vec!["/security-review — diff touches auth headers".into()],
            llm_reviewed: false,
            turn_id: Some("t-qs-1".into()),
            correlation: CorrelationCtx {
                conversation_id: Some("conv-qs".into()),
                ..CorrelationCtx::default()
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::QualitySnapshot {
            score,
            tdd_pass,
            clean_pass,
            file_advisories,
            skill_advisories,
            llm_reviewed,
            turn_id,
            correlation,
        } = decoded
        {
            assert_eq!(score, 75);
            assert!(tdd_pass);
            assert!(clean_pass);
            assert!(!llm_reviewed);
            assert_eq!(file_advisories.len(), 1);
            assert!(file_advisories[0].contains("7880"));
            assert_eq!(skill_advisories.len(), 1);
            assert!(skill_advisories[0].contains("/security-review"));
            assert_eq!(turn_id.as_deref(), Some("t-qs-1"));
            assert_eq!(correlation.conversation_id.as_deref(), Some("conv-qs"));
        } else {
            panic!("wrong variant after roundtrip");
        }
    }

    #[test]
    fn quality_snapshot_omits_empty_advisory_vecs() {
        let ev = TurnEvent::QualitySnapshot {
            score: 100,
            tdd_pass: true,
            clean_pass: true,
            file_advisories: vec![],
            skill_advisories: vec![],
            llm_reviewed: false,
            turn_id: None,
            correlation: CorrelationCtx::default(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            !json.contains("file_advisories"),
            "empty vecs must be omitted from JSON; got: {json}"
        );
        assert!(
            !json.contains("skill_advisories"),
            "empty vecs must be omitted from JSON; got: {json}"
        );
        assert!(
            !json.contains("turn_id"),
            "None turn_id must be omitted; got: {json}"
        );
    }

    #[test]
    fn quality_snapshot_deserializes_without_optional_fields() {
        // Older producers may omit advisory vecs and turn_id.
        let json = r#"{"QualitySnapshot":{"score":50,"tdd_pass":false,"clean_pass":true}}"#;
        let ev: TurnEvent = serde_json::from_str(json).unwrap();
        if let TurnEvent::QualitySnapshot {
            score,
            tdd_pass,
            clean_pass,
            file_advisories,
            skill_advisories,
            turn_id,
            ..
        } = ev
        {
            assert_eq!(score, 50);
            assert!(!tdd_pass);
            assert!(clean_pass);
            assert!(file_advisories.is_empty());
            assert!(skill_advisories.is_empty());
            assert!(turn_id.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    // --- WI-028: HistoryReplaced ---

    #[test]
    fn history_replaced_serializes_and_deserializes() {
        let ev = TurnEvent::HistoryReplaced {
            session_id: "sess-1".into(),
            turn_id: "turn-1".into(),
            summary_tokens: 512,
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(
            json.contains("\"HistoryReplaced\"")
                || json.contains("history_replaced")
                || json.contains("HistoryReplaced"),
            "must tag as HistoryReplaced"
        );
        let decoded: TurnEvent = serde_json::from_str(&json).expect("deserialize");
        if let TurnEvent::HistoryReplaced {
            session_id,
            turn_id,
            summary_tokens,
        } = decoded
        {
            assert_eq!(session_id, "sess-1");
            assert_eq!(turn_id, "turn-1");
            assert_eq!(summary_tokens, 512);
        } else {
            panic!("wrong variant");
        }
    }

    // --- Audit progress + report stream ---

    #[test]
    fn audit_progress_roundtrips_and_omits_none_turn_id() {
        let ev = TurnEvent::AuditProgress {
            iteration: 3,
            total: 12,
            activity: "read_file src/main.rs".into(),
            findings_so_far: 2,
            turn_id: None,
            correlation: CorrelationCtx::default(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("AuditProgress"), "variant tag; got: {json}");
        assert!(
            !json.contains("turn_id"),
            "None turn_id must be omitted; got: {json}"
        );
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::AuditProgress {
            iteration,
            total,
            activity,
            findings_so_far,
            ..
        } = decoded
        {
            assert_eq!(iteration, 3);
            assert_eq!(total, 12);
            assert_eq!(activity, "read_file src/main.rs");
            assert_eq!(findings_so_far, 2);
        } else {
            panic!("wrong variant after roundtrip");
        }
    }

    #[test]
    fn audit_report_roundtrips_with_counts_and_path() {
        let ev = TurnEvent::AuditReport {
            report: "# Audit Report\n\n## Summary\n".into(),
            counts: serde_json::json!({"critical": 1, "low": 2}),
            report_path: Some("/tmp/report.md".into()),
            turn_id: Some("audit-1".into()),
            correlation: CorrelationCtx::default(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::AuditReport {
            report,
            counts,
            report_path,
            turn_id,
            ..
        } = decoded
        {
            assert!(report.contains("## Summary"));
            assert_eq!(counts["critical"], 1);
            assert_eq!(report_path.as_deref(), Some("/tmp/report.md"));
            assert_eq!(turn_id.as_deref(), Some("audit-1"));
        } else {
            panic!("wrong variant after roundtrip");
        }
    }

    // --- ACP-shaped tool-call status stream ---

    #[test]
    fn tool_call_status_maps_to_acp_strings() {
        assert_eq!(ToolCallStatus::Pending.as_acp_str(), "pending");
        assert_eq!(ToolCallStatus::InProgress.as_acp_str(), "in_progress");
        assert_eq!(ToolCallStatus::Completed.as_acp_str(), "completed");
        assert_eq!(ToolCallStatus::Failed.as_acp_str(), "failed");
    }

    #[test]
    fn tool_call_update_roundtrips_with_diff_content() {
        let ev = TurnEvent::ToolCallUpdate {
            tool_call_id: "call-7".into(),
            tool_name: "edit_file".into(),
            status: ToolCallStatus::Completed,
            content: vec![ToolCallContent::Diff {
                path: "src/lib.rs".into(),
                old_text: "old".into(),
                new_text: "new".into(),
            }],
            turn_id: Some("t-9".into()),
            correlation: CorrelationCtx::default(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("ToolCallUpdate"), "variant tag; got: {json}");
        let decoded: TurnEvent = serde_json::from_str(&json).unwrap();
        if let TurnEvent::ToolCallUpdate {
            tool_call_id,
            status,
            content,
            turn_id,
            ..
        } = decoded
        {
            assert_eq!(tool_call_id, "call-7");
            assert_eq!(status, ToolCallStatus::Completed);
            assert_eq!(turn_id.as_deref(), Some("t-9"));
            assert_eq!(content.len(), 1);
            match &content[0] {
                ToolCallContent::Diff {
                    path,
                    old_text,
                    new_text,
                } => {
                    assert_eq!(path, "src/lib.rs");
                    assert_eq!(old_text, "old");
                    assert_eq!(new_text, "new");
                }
            }
        } else {
            panic!("wrong variant after roundtrip");
        }
    }

    #[test]
    fn tool_call_update_omits_empty_content() {
        let ev = TurnEvent::ToolCallUpdate {
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            status: ToolCallStatus::InProgress,
            content: vec![],
            turn_id: None,
            correlation: CorrelationCtx::default(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            !json.contains("content"),
            "empty content must be omitted; got: {json}"
        );
        assert!(
            !json.contains("turn_id"),
            "None turn_id must be omitted; got: {json}"
        );
    }
}
