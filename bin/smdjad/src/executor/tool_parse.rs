//! Tool-call parsing helpers for the executor.
//!
//! Extracts `{"tool": ..., "input": ...}` JSON objects embedded in model
//! output, used by the orchestrator to detect tool calls in a turn.

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
