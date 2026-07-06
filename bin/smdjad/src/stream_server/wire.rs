//! [`TurnEvent`] → NDJSON conversion for the live-forwarding loop.

use smedja_bellows::{StreamEvent, TurnEvent};

/// Convert a [`TurnEvent`] to `(turn_id, ndjson_line, is_terminal)`.
///
/// Returns `(None, _, _)` for events where the `turn_id` is unknown or not
/// relevant to the caller's filter (e.g. daemon-level events).
#[allow(clippy::too_many_lines)]
pub(crate) fn turn_event_to_ndjson(
    event: &TurnEvent,
    expected_turn_id: &str,
) -> (Option<String>, String, bool) {
    let ser = |ev: &StreamEvent| serde_json::to_string(ev).unwrap_or_default();
    match event {
        TurnEvent::AssistantDelta {
            content, turn_id, ..
        } => {
            let line = ser(&StreamEvent::Delta {
                text: content.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::ThinkingDelta {
            content, turn_id, ..
        } => {
            let line = ser(&StreamEvent::Thinking {
                text: content.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::ToolCalled {
            tool_name,
            input_summary,
            full_input,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::ToolCall {
                name: tool_name.clone(),
                input: input_summary.clone(),
                full: full_input.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::Completed {
            turn_id,
            output_tokens,
            input_tokens,
            traceparent,
            ..
        } => {
            if turn_id != expected_turn_id {
                return (Some(turn_id.clone()), String::new(), false);
            }
            let line = ser(&StreamEvent::Done {
                output_tok: *output_tokens,
                input_tok: *input_tokens,
                traceparent: traceparent.clone(),
            });
            (Some(turn_id.clone()), line, true)
        }
        TurnEvent::Failed {
            turn_id, reason, ..
        } => {
            if turn_id != expected_turn_id {
                return (Some(turn_id.clone()), String::new(), false);
            }
            let line = ser(&StreamEvent::Error {
                message: reason.clone(),
            });
            (Some(turn_id.clone()), line, true)
        }
        TurnEvent::Started {
            turn_id,
            correlation,
            ..
        } => {
            if let Some(ref name) = correlation.agent_name {
                let line = ser(&StreamEvent::Started {
                    agent_name: Some(name.clone()),
                });
                (Some(turn_id.clone()), line, false)
            } else {
                (None, String::new(), false)
            }
        }
        TurnEvent::QualitySnapshot {
            score,
            tdd_pass,
            clean_pass,
            file_advisories,
            skill_advisories,
            llm_reviewed,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::Quality {
                score: *score,
                tdd_pass: *tdd_pass,
                clean_pass: *clean_pass,
                file_advisories: file_advisories.clone(),
                skill_advisories: skill_advisories.clone(),
                llm_reviewed: *llm_reviewed,
                suggested_command: None,
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::CoworkRequest {
            approval_id,
            tool,
            step_n,
            args_display,
            reasoning,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::CoworkRequest {
                approval_id: approval_id.clone(),
                tool: tool.clone(),
                step_n: *step_n,
                args_display: args_display.clone(),
                reasoning: reasoning.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::TokenUsage {
            input_tok,
            output_tok,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::Usage {
                input_tok: *input_tok,
                output_tok: *output_tok,
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::ToolCallChunk {
            name,
            partial_input,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::ToolCallChunk {
                name: name.clone(),
                partial_input: partial_input.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::ToolCallUpdate {
            tool_call_id,
            tool_name,
            status,
            content,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::ToolCallUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                status: status.as_acp_str().to_owned(),
                content: content.clone(),
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::AuditProgress {
            iteration,
            total,
            activity,
            findings_so_far,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::AuditProgress {
                iteration: *iteration,
                total: *total,
                activity: activity.clone(),
                findings_so_far: *findings_so_far,
            });
            (turn_id.clone(), line, false)
        }
        TurnEvent::AuditReport {
            report,
            counts,
            report_path,
            turn_id,
            ..
        } => {
            let line = ser(&StreamEvent::AuditReport {
                report: report.clone(),
                counts: counts.clone(),
                report_path: report_path.clone(),
            });
            // Terminal: the audit stream ends once the report is delivered.
            (turn_id.clone(), line, true)
        }
        TurnEvent::HistoryReplaced { turn_id, .. } => (Some(turn_id.clone()), String::new(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_bellows::event::CorrelationCtx;

    #[test]
    fn turn_event_to_ndjson_delta_returns_correct_type() {
        let event = TurnEvent::AssistantDelta {
            content: "hello world".into(),
            turn_id: Some("t3".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t3");
        assert_eq!(tid.as_deref(), Some("t3"));
        assert!(line.contains(r#""type":"delta""#));
        assert!(line.contains("hello world"));
        assert!(!terminal);
    }

    #[test]
    fn turn_event_to_ndjson_completed_is_terminal() {
        let event = TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: "t4".into(),
            output_tokens: 42,
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t4");
        assert_eq!(tid.as_deref(), Some("t4"));
        assert!(line.contains(r#""type":"done""#));
        assert!(terminal);
    }

    #[test]
    fn turn_event_to_ndjson_completed_includes_traceparent_and_input_tok() {
        let event = TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: "t5".into(),
            output_tokens: 88,
            input_tokens: Some(412),
            traceparent: Some("00-abc123-def456-01".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t5");
        assert_eq!(tid.as_deref(), Some("t5"));
        assert!(terminal);
        assert!(
            line.contains(r#""input_tok":412"#),
            "expected input_tok in done line; got: {line}"
        );
        assert!(
            line.contains("abc123"),
            "expected traceparent in done line; got: {line}"
        );
    }

    #[test]
    fn turn_event_to_ndjson_started_with_agent_name_emits_started_line() {
        let event = TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "t-start".into(),
            correlation: CorrelationCtx {
                agent_name: Some("review".into()),
                ..CorrelationCtx::default()
            },
        };
        let (_tid, line, terminal) = turn_event_to_ndjson(&event, "t-start");
        assert!(
            line.contains(r#""type":"started""#),
            "started event must have type=started; got: {line}"
        );
        assert!(
            line.contains("review"),
            "agent_name must appear in started line; got: {line}"
        );
        assert!(!terminal, "started event is not terminal");
    }

    #[test]
    fn turn_event_to_ndjson_started_without_agent_name_emits_empty() {
        let event = TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "t-no-agent".into(),
            correlation: CorrelationCtx::default(),
        };
        let (_tid, line, _terminal) = turn_event_to_ndjson(&event, "t-no-agent");
        assert!(
            line.is_empty(),
            "started without agent_name must emit empty line; got: {line}"
        );
    }

    #[test]
    fn turn_event_to_ndjson_thinking_delta_returns_thinking_type() {
        let event = TurnEvent::ThinkingDelta {
            content: "let me reason about this".into(),
            turn_id: Some("t-think".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t-think");
        assert_eq!(tid.as_deref(), Some("t-think"));
        assert!(
            line.contains(r#""type":"thinking""#),
            "thinking delta must have type=thinking; got: {line}"
        );
        assert!(
            line.contains("let me reason"),
            "thinking content must appear in NDJSON; got: {line}"
        );
        assert!(!terminal, "thinking delta must not be a terminal event");
    }

    #[test]
    fn turn_event_to_ndjson_audit_progress_is_non_terminal_and_routed() {
        let event = TurnEvent::AuditProgress {
            iteration: 2,
            total: 12,
            activity: "read_file src/x.rs".into(),
            findings_so_far: 0,
            turn_id: Some("audit-9".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "audit-9");
        assert_eq!(tid.as_deref(), Some("audit-9"));
        assert!(
            line.contains(r#""type":"audit_progress""#),
            "must map to audit_progress; got: {line}"
        );
        assert!(line.contains("read_file src/x.rs"), "activity; got: {line}");
        assert!(!terminal, "progress heartbeats never terminate the stream");
    }

    #[test]
    fn turn_event_to_ndjson_audit_report_is_terminal() {
        let event = TurnEvent::AuditReport {
            report: "# Audit Report\n".into(),
            counts: serde_json::json!({"critical": 1}),
            report_path: None,
            turn_id: Some("audit-9".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "audit-9");
        assert_eq!(tid.as_deref(), Some("audit-9"));
        assert!(
            line.contains(r#""type":"audit_report""#),
            "must map to audit_report; got: {line}"
        );
        assert!(terminal, "the report terminates the audit stream");
    }

    #[test]
    fn turn_event_to_ndjson_tool_call_update_maps_status_and_diff() {
        use smedja_bellows::{ToolCallContent, ToolCallStatus};
        let event = TurnEvent::ToolCallUpdate {
            tool_call_id: "call-1".into(),
            tool_name: "edit_file".into(),
            status: ToolCallStatus::Completed,
            content: vec![ToolCallContent::Diff {
                path: "src/x.rs".into(),
                old_text: "a".into(),
                new_text: "b".into(),
            }],
            turn_id: Some("t-u".into()),
            correlation: CorrelationCtx::default(),
        };
        let (tid, line, terminal) = turn_event_to_ndjson(&event, "t-u");
        assert_eq!(tid.as_deref(), Some("t-u"));
        assert!(
            line.contains(r#""type":"tool_call_update""#),
            "must map to tool_call_update; got: {line}"
        );
        assert!(
            line.contains(r#""status":"completed""#),
            "status string must be mapped; got: {line}"
        );
        assert!(
            line.contains(r#""type":"diff""#) && line.contains("src/x.rs"),
            "diff content must appear; got: {line}"
        );
        assert!(!terminal);
    }
}
