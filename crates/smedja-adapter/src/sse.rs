//! Pure SSE line-parsing functions for `OpenAI` and Anthropic streaming responses.
//!
//! These functions are intentionally free of I/O so they can be unit-tested
//! without a live HTTP connection.

use crate::{AdapterError, Delta};

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
}
