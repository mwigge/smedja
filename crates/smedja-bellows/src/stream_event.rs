/// Typed wire protocol for the smdjad→TUI NDJSON stream.
///
/// Every line written to the stream socket is one of these variants
/// serialised as `{"type":"<snake_case_name>",...}`.  The TUI deserialises
/// each line back into a `StreamEvent` and pattern-matches on the variant.
///
/// The variants precisely reflect the existing wire format so the type can
/// be a drop-in for `serde_json::Value` field access at both ends.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Agent name notification at turn start.
    Started {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
    },

    /// Incremental assistant response text.
    Delta { text: String },

    /// Incremental extended-thinking text (collapsed in TUI by default).
    Thinking { text: String },

    /// A tool was invoked.
    ToolCall {
        name: String,
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full: Option<String>,
    },

    /// Turn completed successfully.
    Done {
        output_tok: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tok: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
    },

    /// Turn failed or stream error.
    Error { message: String },

    /// Tier-1 quality gate snapshot after a completed turn.
    Quality {
        score: u8,
        tdd_pass: bool,
        clean_pass: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        file_advisories: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skill_advisories: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        llm_reviewed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggested_command: Option<String>,
    },

    /// Events were evicted from the per-turn buffer before delivery.
    BufferOverflow { lost: u64 },

    /// A tool call is awaiting human approval at the cowork gate.
    ///
    /// The TUI renders the approval overlay immediately without polling.
    CoworkRequest {
        /// UUID for `cowork.approve` / `cowork.deny` / `cowork.modify`.
        approval_id: String,
        /// Tool name.
        tool: String,
        /// Step index within the current turn.
        step_n: u32,
        /// Human-readable serialised tool arguments.
        args_display: String,
        /// Agent's reasoning for invoking this tool.
        reasoning: String,
    },

    /// Mid-stream token usage update (informational; Done still carries final totals).
    Usage { input_tok: u32, output_tok: u32 },

    /// A partial chunk of a tool call's input arguments (display only).
    ///
    /// Clients that only care about complete tool calls can ignore this and wait
    /// for the terminal `ToolCall` event.
    ToolCallChunk { name: String, partial_input: String },

    /// Auto-compaction fired: the active context window was replaced with a
    /// summary.  Clients should insert a visible seam marker so users know
    /// where the model's memory restarted.
    HistoryReplaced {
        #[serde(default)]
        summary_tokens: usize,
    },

    /// Catchall for unknown future event types — never matched by the TUI.
    #[serde(other)]
    Unknown,
}

impl StreamEvent {
    /// Returns `true` if this event signals the end of a turn.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(ev: &StreamEvent) -> StreamEvent {
        let json = serde_json::to_string(ev).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn delta_roundtrips() {
        let ev = StreamEvent::Delta {
            text: "hello".into(),
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"delta""#));
        assert!(json.contains("hello"));
    }

    #[test]
    fn thinking_roundtrips() {
        let ev = StreamEvent::Thinking {
            text: "reasoning".into(),
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"thinking""#));
    }

    #[test]
    fn tool_call_omits_full_when_none() {
        let ev = StreamEvent::ToolCall {
            name: "bash".into(),
            input: "ls".into(),
            full: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            !json.contains("full"),
            "full must be omitted when None; got: {json}"
        );
        assert_eq!(roundtrip(&ev), ev);
    }

    #[test]
    fn tool_call_with_full_roundtrips() {
        let ev = StreamEvent::ToolCall {
            name: "bash".into(),
            input: "ls /tmp".into(),
            full: Some("ls -la /tmp".into()),
        };
        assert_eq!(roundtrip(&ev), ev);
    }

    #[test]
    fn done_omits_optional_fields_when_none() {
        let ev = StreamEvent::Done {
            output_tok: 42,
            input_tok: None,
            traceparent: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("input_tok"));
        assert!(!json.contains("traceparent"));
        assert_eq!(roundtrip(&ev), ev);
    }

    #[test]
    fn done_with_all_fields_roundtrips() {
        let ev = StreamEvent::Done {
            output_tok: 88,
            input_tok: Some(412),
            traceparent: Some("00-abc-def-01".into()),
        };
        assert_eq!(roundtrip(&ev), ev);
    }

    #[test]
    fn error_roundtrips() {
        let ev = StreamEvent::Error {
            message: "timeout".into(),
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"error""#));
    }

    #[test]
    fn quality_omits_empty_vecs_and_false_llm_reviewed() {
        let ev = StreamEvent::Quality {
            score: 100,
            tdd_pass: true,
            clean_pass: true,
            file_advisories: vec![],
            skill_advisories: vec![],
            llm_reviewed: false,
            suggested_command: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("file_advisories"));
        assert!(!json.contains("skill_advisories"));
        assert!(!json.contains("llm_reviewed"));
        assert_eq!(roundtrip(&ev), ev);
    }

    #[test]
    fn quality_with_advisories_roundtrips() {
        let ev = StreamEvent::Quality {
            score: 50,
            tdd_pass: false,
            clean_pass: true,
            file_advisories: vec!["main.rs 9000L".into()],
            skill_advisories: vec!["/security-review".into()],
            llm_reviewed: true,
            suggested_command: Some("/fix".into()),
        };
        assert_eq!(roundtrip(&ev), ev);
    }

    #[test]
    fn started_with_agent_name_roundtrips() {
        let ev = StreamEvent::Started {
            agent_name: Some("orchestrator".into()),
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"started""#));
    }

    #[test]
    fn started_without_agent_name_omits_field() {
        let ev = StreamEvent::Started { agent_name: None };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("agent_name"));
    }

    #[test]
    fn buffer_overflow_roundtrips() {
        let ev = StreamEvent::BufferOverflow { lost: 3 };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"buffer_overflow""#));
        assert!(json.contains('3'));
    }

    #[test]
    fn is_terminal_only_for_done_and_error() {
        assert!(StreamEvent::Done {
            output_tok: 1,
            input_tok: None,
            traceparent: None
        }
        .is_terminal());
        assert!(StreamEvent::Error {
            message: "x".into()
        }
        .is_terminal());
        assert!(!StreamEvent::Delta { text: "y".into() }.is_terminal());
        assert!(!StreamEvent::Started { agent_name: None }.is_terminal());
    }

    #[test]
    fn cowork_request_roundtrips() {
        let ev = StreamEvent::CoworkRequest {
            approval_id: "uuid-123".into(),
            tool: "bash".into(),
            step_n: 3,
            args_display: r#"{"cmd":"ls"}"#.into(),
            reasoning: "list workspace files".into(),
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"cowork_request""#));
        assert!(json.contains("uuid-123"));
        assert!(json.contains("bash"));
    }

    #[test]
    fn usage_roundtrips() {
        let ev = StreamEvent::Usage {
            input_tok: 100,
            output_tok: 50,
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"usage""#), "{json}");
        assert!(json.contains("100"), "{json}");
        assert!(json.contains("50"), "{json}");
    }

    #[test]
    fn tool_call_chunk_roundtrips() {
        let ev = StreamEvent::ToolCallChunk {
            name: "bash".into(),
            partial_input: r#"{"cmd":"ls"#.into(),
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"tool_call_chunk""#), "{json}");
        assert!(json.contains("bash"), "{json}");
    }

    #[test]
    fn history_replaced_roundtrips() {
        let ev = StreamEvent::HistoryReplaced {
            summary_tokens: 512,
        };
        assert_eq!(roundtrip(&ev), ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"history_replaced""#), "{json}");
        assert!(json.contains("512"), "{json}");
    }

    #[test]
    fn history_replaced_deserializes_from_server_wire_format() {
        // This is what smdjad actually emits (session_id present, but our
        // StreamEvent variant skips it — verify it doesn't fail to deserialize).
        let json = r#"{"type":"history_replaced","session_id":"sess-1","summary_tokens":300}"#;
        let ev: StreamEvent = serde_json::from_str(json).unwrap();
        assert!(
            matches!(
                ev,
                StreamEvent::HistoryReplaced {
                    summary_tokens: 300
                }
            ),
            "should deserialize as HistoryReplaced"
        );
    }

    #[test]
    fn unknown_event_type_deserializes_to_unknown_variant() {
        let json = r#"{"type":"future_event","data":"stuff"}"#;
        let ev: StreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev, StreamEvent::Unknown);
    }

    #[test]
    fn wire_format_matches_existing_ndjson_shape() {
        // Verify the exact JSON key names match what smdjad already emits.
        let delta: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&StreamEvent::Delta { text: "hi".into() }).unwrap(),
        )
        .unwrap();
        assert_eq!(delta["type"], "delta");
        assert_eq!(delta["text"], "hi");

        let done: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&StreamEvent::Done {
                output_tok: 5,
                input_tok: Some(10),
                traceparent: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(done["type"], "done");
        assert_eq!(done["output_tok"], 5);
        assert_eq!(done["input_tok"], 10);
    }
}
