//! Tool-call extraction from model output.
//!
//! Parses the embedded `{"tool": ..., "input": ...}` JSON envelopes the runner
//! emits, both the first match ([`parse_tool_call`]) and every match in order
//! ([`parse_all_tool_calls`]).

use serde_json::Value;

/// Finds the first JSON object with a `"tool"` key anywhere in `text`.
///
/// Uses `serde_json` streaming deserialization: for each `{` byte position,
/// a `Deserializer` is created so that valid JSON is consumed and trailing
/// text is ignored, without a custom brace-counting scanner.
pub(crate) fn find_tool_call_json(text: &str) -> Option<serde_json::Value> {
    use serde::de::Deserialize as _;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' {
            let mut de = serde_json::Deserializer::from_str(&text[i..]);
            if let Ok(v) = serde_json::Value::deserialize(&mut de) {
                if v.get("tool").is_some() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Parses a tool call embedded in `text`, returning `(tool_name, input_json_string)`.
///
/// Looks for a JSON object with a `"tool"` key anywhere in the text.
/// Returns `None` when no tool call is detected.
pub(crate) fn parse_tool_call(text: &str) -> Option<(String, String)> {
    let v = find_tool_call_json(text)?;
    let tool_name = v.get("tool").and_then(Value::as_str)?.to_owned();
    let input = v
        .get("input")
        .map_or_else(|| "{}".to_owned(), std::string::ToString::to_string);
    Some((tool_name, input))
}

/// Parses all tool calls embedded in `text`, returning them in order.
///
/// Scans for every `{"tool": ..., "input": ...}` JSON object in the text.
/// Correctly handles nested JSON by tracking brace depth, so an `"input"`
/// that itself contains `{...}` does not produce spurious extra calls.
pub(crate) fn parse_all_tool_calls(text: &str) -> Vec<(String, String)> {
    use serde::Deserialize as _;
    let mut calls = Vec::new();
    let mut skip_until = 0usize;
    for (i, &b) in text.as_bytes().iter().enumerate() {
        if i < skip_until || b != b'{' {
            continue;
        }
        let slice = &text[i..];
        // Try to deserialize a JSON object starting at this position.
        let mut de = serde_json::Deserializer::from_str(slice);
        if let Ok(v) = serde_json::Value::deserialize(&mut de) {
            if let Some(tool_name) = v.get("tool").and_then(|t| t.as_str()) {
                let input = v
                    .get("input")
                    .map_or_else(|| "{}".to_owned(), std::string::ToString::to_string);
                calls.push((tool_name.to_owned(), input));
                skip_until = i + json_span(slice);
            }
        }
    }
    calls
}

/// Returns the byte length of the first JSON object (or value) starting at `s`.
///
/// Counts matching braces while ignoring escaped characters and strings.
fn json_span(s: &str) -> usize {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (i, b) in s.bytes().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        match b {
            b'\\' if in_string => escape = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::{find_tool_call_json, parse_all_tool_calls, parse_tool_call};

    // ── find_tool_call_json / parse_tool_call ─────────────────────────────────

    #[test]
    fn find_tool_call_json_returns_none_for_empty_string() {
        let result = find_tool_call_json("");
        assert!(result.is_none(), "empty string must yield None");
    }

    #[test]
    fn find_tool_call_json_handles_json_embedded_in_text() {
        let text = r#"Here is the call: {"tool":"read_file","input":{"path":"foo.txt"}} done."#;
        let result = find_tool_call_json(text);
        assert!(result.is_some(), "embedded JSON must be found");
        let v = result.unwrap();
        assert_eq!(v["tool"], "read_file");
        assert_eq!(v["input"]["path"], "foo.txt");
    }

    #[test]
    fn parse_tool_call_returns_none_for_plain_text() {
        let result = parse_tool_call("hello world, no JSON here");
        assert!(result.is_none(), "plain text must yield None");
    }

    #[test]
    fn parse_tool_call_returns_some_for_valid_tool_json() {
        let json = r#"{"tool":"bash","input":{"command":"ls"}}"#;
        let result = parse_tool_call(json);
        assert!(result.is_some(), "valid tool JSON must yield Some");
        let (tool_name, input_str) = result.unwrap();
        assert_eq!(tool_name, "bash");
        let input_val: serde_json::Value = serde_json::from_str(&input_str).unwrap();
        assert_eq!(input_val["command"], "ls");
    }

    #[test]
    fn parse_tool_call_returns_none_for_json_without_tool_key() {
        let json = r#"{"action":"bash","input":{"command":"ls"}}"#;
        let result = parse_tool_call(json);
        assert!(result.is_none(), "JSON without 'tool' key must yield None");
    }

    // ── parse_all_tool_calls ──────────────────────────────────────────────────

    #[test]
    fn parse_all_tool_calls_returns_empty_for_plain_text() {
        assert!(parse_all_tool_calls("hello world").is_empty());
    }

    #[test]
    fn parse_all_tool_calls_returns_single_call() {
        let text = r#"{"tool":"read_file","input":{"path":"foo.txt"}}"#;
        let calls = parse_all_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
    }

    #[test]
    fn parse_all_tool_calls_returns_multiple_calls_in_order() {
        let text = concat!(
            r#"{"tool":"read_file","input":{"path":"a.txt"}} "#,
            r#"{"tool":"grep_files","input":{"pattern":"foo"}} "#,
            r#"{"tool":"list_files","input":{}}"#,
        );
        let calls = parse_all_tool_calls(text);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "grep_files");
        assert_eq!(calls[2].0, "list_files");
    }

    #[test]
    fn parse_all_tool_calls_skips_past_consumed_json() {
        // Nested JSON in `input` must not produce a spurious extra call.
        let text = r#"{"tool":"write_file","input":{"path":"f","content":"{\"nested\":true}"}}"#;
        let calls = parse_all_tool_calls(text);
        assert_eq!(calls.len(), 1, "nested JSON must not produce extra calls");
        assert_eq!(calls[0].0, "write_file");
    }

    #[test]
    fn parse_all_tool_calls_embedded_in_prose() {
        let text = concat!(
            "Here are two reads:\n",
            r#"{"tool":"read_file","input":{"path":"a.txt"}}"#,
            " and ",
            r#"{"tool":"read_file","input":{"path":"b.txt"}}"#,
            "\nDone.",
        );
        let calls = parse_all_tool_calls(text);
        assert_eq!(calls.len(), 2);
    }
}
