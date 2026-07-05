//! Pure SSE line-parsing functions for `OpenAI` and Anthropic streaming responses.
//!
//! These functions are intentionally free of I/O so they can be unit-tested
//! without a live HTTP connection.

use crate::{AdapterError, Delta};

/// Appends `chunk` to `buf` and returns every complete, newline-terminated line
/// as a decoded `String`, draining those lines from `buf`.
///
/// Bytes are accumulated at the byte level so that a multibyte UTF-8 character
/// split across a chunk boundary is not decoded until every one of its bytes has
/// arrived: only bytes up to and including a `\n` are decoded, and `\n` (`0x0A`)
/// can never fall inside a multibyte sequence. Any bytes after the final `\n` —
/// possibly a partial trailing character — stay in `buf` for the next chunk. A
/// single trailing `\r` is trimmed from each returned line.
pub(crate) fn drain_complete_lines(buf: &mut Vec<u8>, chunk: &[u8]) -> Vec<String> {
    buf.extend_from_slice(chunk);
    let mut lines = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = buf.drain(..=pos).collect();
        line.pop(); // drop the trailing '\n'
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        lines.push(String::from_utf8_lossy(&line).into_owned());
    }
    lines
}

/// Strips an SSE `data:` field prefix, tolerating the single optional space that
/// the SSE specification permits after the colon (`data: x` and `data:x` are
/// equivalent). Returns the field value, or `None` when the line is not a
/// `data:` field.
pub(crate) fn strip_sse_data(line: &str) -> Option<&str> {
    line.strip_prefix("data:")
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
}

/// Parses a single `OpenAI` SSE data payload (the text after the `data: ` prefix)
/// into a [`Delta`].
///
/// Returns `None` for the `[DONE]` sentinel, for chunks with no content, and for
/// chunks whose `usage` field is absent when there is also no
/// `choices[0].delta.content`.
///
/// # Errors
///
/// Returns [`AdapterError::Parse`] if the `data` string is not valid JSON or does
/// not match the expected `OpenAI` SSE schema.
pub(crate) fn parse_openai_line(data: &str) -> Result<Option<Delta>, AdapterError> {
    if data == "[DONE]" {
        return Ok(None);
    }

    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| AdapterError::Parse(e.to_string()))?;

    // Final chunk may carry a top-level `usage` object.
    if let Some(usage) = v.get("usage") {
        let raw_in = usage
            .get("prompt_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let raw_out = usage
            .get("completion_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        // OpenAI reports cache reads under `prompt_tokens_details.cached_tokens`.
        let raw_cache = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if raw_in > 0 || raw_out > 0 || raw_cache > 0 {
            // Token counts that exceed u32::MAX are treated as u32::MAX rather
            // than returning an error — realistic LLM responses never approach
            // four billion tokens.
            let input = u32::try_from(raw_in).unwrap_or(u32::MAX);
            let output = u32::try_from(raw_out).unwrap_or(u32::MAX);
            let cache_read = u32::try_from(raw_cache).unwrap_or(u32::MAX);
            return Ok(Some(Delta::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
            }));
        }
    }

    // Regular content delta.
    if let Some(content) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(serde_json::Value::as_str)
    {
        if !content.is_empty() {
            return Ok(Some(Delta::Text(content.to_owned())));
        }
    }

    Ok(None)
}

/// Extracts a partial tool-call argument chunk from an `OpenAI` SSE data payload.
///
/// Returns `Some((name, partial_args))` when the chunk carries `tool_calls[0].function.arguments`.
/// The `name` field is populated on the first chunk (where it appears alongside the `id`);
/// on subsequent chunks it is an empty string.
/// Returns `None` for `[DONE]`, non-tool-call chunks, or empty argument strings.
pub(crate) fn parse_openai_tool_call_chunk(data: &str) -> Option<(String, String)> {
    if data == "[DONE]" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    let tc = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("tool_calls"))
        .and_then(|tc| tc.get(0))?;
    let args = tc
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(serde_json::Value::as_str)?;
    if args.is_empty() {
        return None;
    }
    let name = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    Some((name, args.to_owned()))
}

/// Parses a paired Anthropic SSE event/data into a [`Delta`].
///
/// The Anthropic SSE format emits a separate `event:` line before each `data:`
/// line. Both values must be supplied together.
///
/// Returns `None` for event types that carry no actionable delta (e.g.
/// `content_block_start`, `ping`, `message_stop`).
///
/// # Errors
///
/// Returns [`AdapterError::Parse`] if `data` is not valid JSON or the expected
/// fields are missing for a known event type.
pub(crate) fn parse_anthropic_event(
    event_type: &str,
    data: &str,
) -> Result<Option<Delta>, AdapterError> {
    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| AdapterError::Parse(e.to_string()))?;

    match event_type {
        "content_block_delta" => {
            let text = v
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Delta::Text(text.to_owned())))
            }
        }
        "message_start" => {
            let usage = v.get("message").and_then(|m| m.get("usage"));
            let raw = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            // Anthropic reports cache reads as `cache_read_input_tokens`.
            let raw_cache = usage
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let input = u32::try_from(raw).unwrap_or(u32::MAX);
            let cache_read = u32::try_from(raw_cache).unwrap_or(u32::MAX);
            if input > 0 || cache_read > 0 {
                Ok(Some(Delta::Usage {
                    input_tokens: input,
                    output_tokens: 0,
                    cache_read_tokens: cache_read,
                }))
            } else {
                Ok(None)
            }
        }
        "message_delta" => {
            let raw = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let output = u32::try_from(raw).unwrap_or(u32::MAX);
            if output > 0 {
                Ok(Some(Delta::Usage {
                    input_tokens: 0,
                    output_tokens: output,
                    cache_read_tokens: 0,
                }))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── byte-buffer line splitting ────────────────────────────────────────────

    #[test]
    fn drain_complete_lines_keeps_split_multibyte_char_intact() {
        // The two bytes of 'é' (0xC3 0xA9) are split across two chunks. A
        // per-chunk lossy decode would emit U+FFFD; byte-level buffering must
        // reassemble the character intact.
        let full = "data: café\n".as_bytes();
        let boundary = full.len() - 2; // between 0xC3 and 0xA9
        let mut buf: Vec<u8> = Vec::new();
        let mut out = drain_complete_lines(&mut buf, &full[..boundary]);
        assert!(
            out.is_empty(),
            "no complete line before the newline arrives"
        );
        out.extend(drain_complete_lines(&mut buf, &full[boundary..]));
        assert_eq!(out, vec!["data: café".to_owned()]);
        assert!(!out[0].contains('\u{FFFD}'), "no replacement char");
    }

    #[test]
    fn drain_complete_lines_retains_partial_tail() {
        let mut buf: Vec<u8> = Vec::new();
        let out = drain_complete_lines(&mut buf, b"line one\npartial");
        assert_eq!(out, vec!["line one".to_owned()]);
        // "partial" (no newline yet) is retained for the next chunk.
        let out2 = drain_complete_lines(&mut buf, b" rest\n");
        assert_eq!(out2, vec!["partial rest".to_owned()]);
    }

    // ── SSE data: prefix ──────────────────────────────────────────────────────

    #[test]
    fn strip_sse_data_accepts_optional_space() {
        assert_eq!(strip_sse_data("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(strip_sse_data("data:{\"a\":1}"), Some("{\"a\":1}"));
        // Only a single leading space is consumed.
        assert_eq!(strip_sse_data("data:  x"), Some(" x"));
        assert_eq!(strip_sse_data("event: foo"), None);
    }

    // ── OpenAI ───────────────────────────────────────────────────────────────

    #[test]
    fn openai_text_delta_parsed() {
        let data = r#"{"id":"chatcmpl-abc","choices":[{"delta":{"content":"Hello"},"index":0}],"model":"gpt-4o"}"#;
        let result = parse_openai_line(data).expect("parse must not error");
        assert_eq!(result, Some(Delta::Text("Hello".to_owned())));
    }

    #[test]
    fn openai_done_returns_none() {
        let result = parse_openai_line("[DONE]").expect("parse must not error");
        assert_eq!(result, None);
    }

    #[test]
    fn openai_empty_delta_skipped() {
        // delta present but no "content" field → None
        let data =
            r#"{"id":"chatcmpl-abc","choices":[{"delta":{},"finish_reason":"stop"}],"usage":null}"#;
        let result = parse_openai_line(data).expect("parse must not error");
        assert_eq!(result, None);
    }

    #[test]
    fn openai_usage_chunk_parsed() {
        let data = r#"{"id":"chatcmpl-abc","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let result = parse_openai_line(data).expect("parse must not error");
        assert_eq!(
            result,
            Some(Delta::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
            })
        );
    }

    #[test]
    fn openai_usage_chunk_parses_cached_tokens() {
        let data = r#"{"id":"chatcmpl-abc","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":7}}}"#;
        let result = parse_openai_line(data).expect("parse must not error");
        assert_eq!(
            result,
            Some(Delta::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 7,
            })
        );
    }

    // ── Anthropic ────────────────────────────────────────────────────────────

    #[test]
    fn anthropic_text_delta_parsed() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result =
            parse_anthropic_event("content_block_delta", data).expect("parse must not error");
        assert_eq!(result, Some(Delta::Text("Hello".to_owned())));
    }

    #[test]
    fn anthropic_message_delta_with_output_tok() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":17}}"#;
        let result = parse_anthropic_event("message_delta", data).expect("parse must not error");
        assert_eq!(
            result,
            Some(Delta::Usage {
                input_tokens: 0,
                output_tokens: 17,
                cache_read_tokens: 0,
            })
        );
    }

    #[test]
    fn anthropic_unknown_event_returns_none() {
        let data = r#"{"type":"ping"}"#;
        let result = parse_anthropic_event("ping", data).expect("parse must not error");
        assert_eq!(result, None);
    }

    #[test]
    fn anthropic_message_start_with_input_tok() {
        let data =
            r#"{"type":"message_start","message":{"usage":{"input_tokens":42,"output_tokens":0}}}"#;
        let result = parse_anthropic_event("message_start", data).expect("parse must not error");
        assert_eq!(
            result,
            Some(Delta::Usage {
                input_tokens: 42,
                output_tokens: 0,
                cache_read_tokens: 0,
            })
        );
    }

    #[test]
    fn anthropic_message_start_parses_cache_read_tokens() {
        let data = r#"{"type":"message_start","message":{"usage":{"input_tokens":42,"cache_read_input_tokens":1000}}}"#;
        let result = parse_anthropic_event("message_start", data).expect("parse must not error");
        assert_eq!(
            result,
            Some(Delta::Usage {
                input_tokens: 42,
                output_tokens: 0,
                cache_read_tokens: 1000,
            })
        );
    }

    // ── parse_openai_tool_call_chunk ──────────────────────────────────────────

    #[test]
    fn openai_tool_call_chunk_with_name_on_first_chunk() {
        let data = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"bash","arguments":"{\""}}]},"finish_reason":null}]}"#;
        let result = parse_openai_tool_call_chunk(data);
        assert_eq!(result, Some(("bash".to_owned(), "{\"".to_owned())));
    }

    #[test]
    fn openai_tool_call_chunk_without_name_on_subsequent_chunk() {
        let data = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"cmd\":"}}]},"finish_reason":null}]}"#;
        let result = parse_openai_tool_call_chunk(data);
        // name is empty string (not present on subsequent chunks)
        assert!(result.is_some());
        let (name, partial) = result.unwrap();
        assert_eq!(name, "");
        assert!(partial.contains("cmd"));
    }

    #[test]
    fn openai_tool_call_chunk_returns_none_for_done() {
        assert_eq!(parse_openai_tool_call_chunk("[DONE]"), None);
    }

    #[test]
    fn openai_tool_call_chunk_returns_none_for_text_delta() {
        let data = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
        assert_eq!(parse_openai_tool_call_chunk(data), None);
    }

    #[test]
    fn openai_tool_call_chunk_returns_none_for_empty_arguments() {
        // First chunk sends empty arguments="" — not a useful chunk yet.
        let data = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"bash","arguments":""}}]}}]}"#;
        assert_eq!(parse_openai_tool_call_chunk(data), None);
    }
}
