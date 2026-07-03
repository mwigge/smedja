//! JSONL line parsing for `codex exec --json`.

use crate::Delta;

/// Parses a single JSONL line from `codex exec --json`.
///
/// Tries multiple known `OpenAI` Responses-API event shapes before falling back
/// to treating a non-empty, non-JSON line as plain text. Returns `None` for
/// blank lines and unrecognised JSON objects.
#[allow(clippy::too_many_lines)]
pub(crate) fn parse_codex_line(line: &str) -> Option<Delta> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // Non-JSON line — treat as plain text with a trailing newline.
        return Some(Delta::Text(format!("{trimmed}\n")));
    };

    // Pattern 1: {"delta":"text"} — response.output_text.delta event
    if let Some(text) = v.get("delta").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return Some(Delta::Text(text.to_owned()));
        }
    }

    // Pattern 2: OpenAI streaming {"choices":[{"delta":{"content":"text"}}]}
    if let Some(content) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(serde_json::Value::as_str)
    {
        if !content.is_empty() {
            return Some(Delta::Text(content.to_owned()));
        }
    }

    // Pattern 3: {"text":"..."} or {"content":"..."}
    if let Some(text) = v
        .get("text")
        .and_then(serde_json::Value::as_str)
        .or_else(|| v.get("content").and_then(serde_json::Value::as_str))
    {
        if !text.is_empty() {
            return Some(Delta::Text(text.to_owned()));
        }
    }

    // Pattern 4: OpenAI Responses API — completed function_call output item.
    // {"type":"response.output_item.done","item":{"type":"function_call","call_id":"...","name":"...","arguments":"..."}}
    if v.get("type").and_then(serde_json::Value::as_str) == Some("response.output_item.done") {
        let item = v.get("item")?;
        if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call") {
            let name = item.get("name").and_then(serde_json::Value::as_str)?;
            let raw_args = item
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("{}");
            let input = serde_json::from_str::<serde_json::Value>(raw_args)
                .unwrap_or(serde_json::Value::String(raw_args.to_owned()));
            return Some(Delta::ToolCall {
                name: name.to_owned(),
                input,
            });
        }
        // function_call_output — the tool result codex fed back to the model.
        if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output") {
            let call_id = item
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            let output = item
                .get("output")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            if !call_id.is_empty() {
                return Some(Delta::ToolResult {
                    tool_use_id: call_id,
                    content: output,
                });
            }
        }
    }

    // Pattern 5: {"type":"response.completed","response":{"usage":{"input_tokens":N,"output_tokens":M}}}
    if v.get("type").and_then(serde_json::Value::as_str) == Some("response.completed") {
        let usage = v.get("response").and_then(|r| r.get("usage"))?;
        let input = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        // OpenAI Responses API reports cache reads under
        // `input_tokens_details.cached_tokens`.
        let cache = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if input > 0 || output > 0 || cache > 0 {
            return Some(Delta::Usage {
                #[allow(clippy::cast_possible_truncation)] // token counts fit in u32
                input_tokens: input as u32,
                #[allow(clippy::cast_possible_truncation)]
                output_tokens: output as u32,
                #[allow(clippy::cast_possible_truncation)]
                cache_read_tokens: cache as u32,
            });
        }
    }

    // Pattern 6: codex exec --json new wire format (≥0.139).
    //
    // {"type":"item.started","item":{"type":"command_execution","command":"...","...}}
    // → ToolCall delta so the TUI can display the pending tool.
    //
    // {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
    // → Text delta (the assistant reply).
    //
    // {"type":"item.completed","item":{"type":"command_execution","aggregated_output":"..."}}
    // → ToolResult delta.
    //
    // {"type":"turn.completed","usage":{"input_tokens":N,"output_tokens":M,"cached_input_tokens":K}}
    // → Usage delta.
    if let Some(ev_type) = v.get("type").and_then(serde_json::Value::as_str) {
        match ev_type {
            "item.started" => {
                let item = v.get("item")?;
                if item.get("type").and_then(serde_json::Value::as_str) == Some("command_execution")
                {
                    let cmd = item
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    if !cmd.is_empty() {
                        return Some(Delta::ToolCall {
                            name: "shell".to_owned(),
                            input: serde_json::json!({ "command": cmd }),
                        });
                    }
                }
            }
            "item.completed" => {
                let item = v.get("item")?;
                match item.get("type").and_then(serde_json::Value::as_str) {
                    Some("agent_message") => {
                        let text = item
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        if !text.is_empty() {
                            return Some(Delta::Text(text.to_owned()));
                        }
                    }
                    Some("command_execution") => {
                        let output = item
                            .get("aggregated_output")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        let cmd = item
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?");
                        return Some(Delta::ToolResult {
                            tool_use_id: String::new(),
                            content: format!("[{cmd}]\n{output}"),
                        });
                    }
                    _ => {}
                }
            }
            "turn.completed" => {
                let usage = v.get("usage")?;
                let input = usage
                    .get("input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let cache = usage
                    .get("cached_input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                if input > 0 || output > 0 {
                    return Some(Delta::Usage {
                        #[allow(clippy::cast_possible_truncation)]
                        input_tokens: input as u32,
                        #[allow(clippy::cast_possible_truncation)]
                        output_tokens: output as u32,
                        #[allow(clippy::cast_possible_truncation)]
                        cache_read_tokens: cache as u32,
                    });
                }
            }
            _ => {}
        }
    }

    // Unrecognised JSON shape — skip.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codex_line_empty_returns_none() {
        assert!(parse_codex_line("").is_none());
        assert!(parse_codex_line("   ").is_none());
    }

    #[test]
    fn parse_codex_line_plain_text_returns_text_with_newline() {
        let delta = parse_codex_line("hello world");
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "hello world\n"));
    }

    #[test]
    fn parse_codex_line_delta_field() {
        let delta = parse_codex_line(r#"{"type":"response.output_text.delta","delta":"hi"}"#);
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "hi"));
    }

    #[test]
    fn parse_codex_line_openai_streaming() {
        let delta = parse_codex_line(r#"{"choices":[{"delta":{"content":"streamed text"}}]}"#);
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "streamed text"));
    }

    #[test]
    fn parse_codex_line_text_field() {
        let delta = parse_codex_line(r#"{"type":"message","text":"answer"}"#);
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "answer"));
    }

    #[test]
    fn parse_codex_line_content_field() {
        let delta = parse_codex_line(r#"{"content":"response"}"#);
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "response"));
    }

    #[test]
    fn parse_codex_line_unrecognised_json_returns_none() {
        assert!(parse_codex_line(r#"{"foo":"bar"}"#).is_none());
    }

    #[test]
    fn parse_codex_line_function_call_returns_tool_call() {
        let line = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#;
        let delta = parse_codex_line(line);
        match delta {
            Some(Delta::ToolCall { name, input }) => {
                assert_eq!(name, "shell");
                assert_eq!(input["cmd"].as_str(), Some("ls"));
            }
            other => panic!("expected ToolCall; got: {other:?}"),
        }
    }

    #[test]
    fn parse_codex_line_function_call_output_returns_tool_result() {
        let line = r#"{"type":"response.output_item.done","item":{"type":"function_call_output","call_id":"c1","output":"exit 0"}}"#;
        let delta = parse_codex_line(line);
        match delta {
            Some(Delta::ToolResult {
                tool_use_id,
                content,
            }) => {
                assert_eq!(tool_use_id, "c1");
                assert_eq!(content, "exit 0");
            }
            other => panic!("expected ToolResult; got: {other:?}"),
        }
    }

    #[test]
    fn parse_codex_line_response_completed_returns_usage() {
        let line = r#"{"type":"response.completed","response":{"usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let delta = parse_codex_line(line);
        match delta {
            Some(Delta::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
            }) => {
                assert_eq!(input_tokens, 100);
                assert_eq!(output_tokens, 50);
                assert_eq!(cache_read_tokens, 0);
            }
            other => panic!("expected Usage; got: {other:?}"),
        }
    }
}
