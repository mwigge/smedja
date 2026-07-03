//! Prompt rendering and `stream-json` line parsing for the Claude CLI.

use crate::{Delta, Message, Role};

/// Renders the conversation into a single prompt for `claude --print`.
///
/// A lone user turn is sent verbatim (the common single-turn case). Multi-turn
/// histories become a labelled `Human:` / `Assistant:` transcript so the CLI
/// has the full context in one shot — no dependency on its resume store.
/// System messages are delivered out of band and excluded here.
pub(crate) fn render_conversation(messages: &[Message]) -> String {
    let dialogue: Vec<&Message> = messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .collect();
    match dialogue.as_slice() {
        [] => messages
            .last()
            .map_or_else(String::new, |m| m.content.clone()),
        [single] => single.content.clone(),
        many => {
            let mut out = String::new();
            for m in many {
                let label = match m.role {
                    Role::Assistant => "Assistant",
                    _ => "Human",
                };
                out.push_str(label);
                out.push_str(": ");
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
            out
        }
    }
}

pub(crate) fn parse_line(line: &str) -> Option<Delta> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    if value.get("type").and_then(serde_json::Value::as_str) == Some("system")
        && value.get("subtype").and_then(serde_json::Value::as_str) == Some("init")
    {
        return value
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(|id| Delta::SessionId(id.to_owned()));
    }

    if value.get("type").and_then(serde_json::Value::as_str) == Some("result") {
        if let Some(session_id) = value.get("session_id").and_then(serde_json::Value::as_str) {
            return Some(Delta::SessionId(session_id.to_owned()));
        }
        if let Some(usage) = value.get("usage") {
            return Some(parse_usage(usage));
        }
    }

    let contents = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)?;

    for item in contents {
        match item.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(serde_json::Value::as_str) {
                    return Some(Delta::Text(text.to_owned()));
                }
            }
            Some("tool_use") => {
                let name = item
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let input = item
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                return Some(Delta::ToolCall { name, input });
            }
            Some("tool_result") => {
                let tool_use_id = item
                    .get("tool_use_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let content = item
                    .get("content")
                    .map_or_else(String::new, stringify_content);
                return Some(Delta::ToolResult {
                    tool_use_id,
                    content,
                });
            }
            _ => {}
        }
    }

    None
}

fn parse_usage(usage: &serde_json::Value) -> Delta {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    // The Claude CLI surfaces cache reads as `cache_read_input_tokens`.
    let cache_read_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    Delta::Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
    }
}

fn stringify_content(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Some(item.to_string()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_extracts_session_id_from_init() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc123"}"#;
        assert_eq!(parse_line(line), Some(Delta::SessionId("abc123".into())));
    }

    #[test]
    fn parse_line_extracts_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#;
        assert_eq!(parse_line(line), Some(Delta::Text("hello".into())));
    }

    #[test]
    fn parse_line_extracts_tool_call() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::ToolCall {
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            })
        );
    }

    #[test]
    fn parse_line_extracts_tool_result() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"u1","content":"done"}]}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::ToolResult {
                tool_use_id: "u1".into(),
                content: "done".into(),
            })
        );
    }

    #[test]
    fn parse_line_extracts_usage() {
        let line = r#"{"type":"result","usage":{"input_tokens":7,"output_tokens":11}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::Usage {
                input_tokens: 7,
                output_tokens: 11,
                cache_read_tokens: 0,
            })
        );
    }

    #[test]
    fn parse_line_extracts_cache_read_tokens() {
        let line = r#"{"type":"result","usage":{"input_tokens":7,"output_tokens":11,"cache_read_input_tokens":900}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::Usage {
                input_tokens: 7,
                output_tokens: 11,
                cache_read_tokens: 900,
            })
        );
    }

    #[test]
    fn render_conversation_single_user_turn_is_verbatim() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hello there".into(),
        }];
        assert_eq!(render_conversation(&msgs), "hello there");
    }

    #[test]
    fn render_conversation_multi_turn_is_labelled_transcript() {
        let msgs = vec![
            Message {
                role: Role::User,
                content: "my number is 7".into(),
            },
            Message {
                role: Role::Assistant,
                content: "noted".into(),
            },
            Message {
                role: Role::User,
                content: "what number?".into(),
            },
        ];
        let rendered = render_conversation(&msgs);
        assert_eq!(
            rendered,
            "Human: my number is 7\n\nAssistant: noted\n\nHuman: what number?\n\n"
        );
    }

    #[test]
    fn render_conversation_excludes_system_messages() {
        let msgs = vec![
            Message {
                role: Role::System,
                content: "be terse".into(),
            },
            Message {
                role: Role::User,
                content: "hi".into(),
            },
        ];
        // System filtered → single dialogue turn → verbatim.
        assert_eq!(render_conversation(&msgs), "hi");
    }
}
