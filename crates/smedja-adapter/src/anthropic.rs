//! Anthropic Messages API streaming adapter.

use reqwest::Client;
use serde_json::json;
use tokio_stream::{wrappers::ReceiverStream, StreamExt as _};

use crate::{
    otel::inject_traceparent, sse::parse_anthropic_event, AdapterError, CallOptions, Delta,
    DeltaStream, Message, Provider, Role,
};

/// Anthropic streaming chat-completion provider.
///
/// Sends requests to `https://api.anthropic.com/v1/messages` and translates
/// the Server-Sent Events response into a [`DeltaStream`].
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
}

impl AnthropicProvider {
    /// Creates a new [`AnthropicProvider`].
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
        }
    }

    /// Creates a new [`AnthropicProvider`] with a pre-configured [`reqwest::Client`].
    pub fn with_client(client: Client, api_key: impl Into<String>) -> Self {
        Self {
            client,
            api_key: api_key.into(),
        }
    }
}

fn role_to_str(role: &Role) -> &'static str {
    match role {
        // System messages are extracted to the top-level field before the
        // message array is built; this arm should never be reached in practice.
        Role::System | Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Builds the Anthropic request body from messages and call options.
fn build_body(messages: &[Message], opts: &CallOptions) -> serde_json::Value {
    let cache = opts.stable_prefix_len.is_some();

    // Anthropic requires the system prompt as a top-level field; extract it
    // from `opts.system` first, then fall back to the first System message.
    let system_content: Option<String> = opts.system.clone().or_else(|| {
        messages
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| m.content.clone())
    });

    // Filter non-system messages and apply cache_control to the stable-prefix boundary.
    let prefix_len = opts.stable_prefix_len.unwrap_or(0);
    let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != Role::System).collect();

    let msg_array: Vec<serde_json::Value> = non_system
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mark = cache && prefix_len > 0 && i + 1 == prefix_len;
            if mark {
                json!({
                    "role": role_to_str(&m.role),
                    "content": [{"type": "text", "text": m.content, "cache_control": {"type": "ephemeral"}}],
                })
            } else {
                json!({
                    "role": role_to_str(&m.role),
                    "content": m.content,
                })
            }
        })
        .collect();

    let mut body = json!({
        "model": opts.model,
        "messages": msg_array,
        "stream": true,
        "max_tokens": opts.max_tokens.unwrap_or(1024),
    });

    // System prompt: content-block array with cache_control when caching is enabled.
    if let Some(sys) = system_content {
        if cache {
            body["system"] =
                json!([{"type": "text", "text": sys, "cache_control": {"type": "ephemeral"}}]);
        } else {
            body["system"] = json!(sys);
        }
    }

    // Tools: mark the last definition with cache_control when caching is enabled.
    if let Some(tools) = &opts.tools {
        if !tools.is_empty() {
            if cache {
                let mut tools_with_cache: Vec<serde_json::Value> = tools.clone();
                let last = tools_with_cache.len() - 1;
                if let Some(obj) = tools_with_cache[last].as_object_mut() {
                    obj.insert("cache_control".to_owned(), json!({"type": "ephemeral"}));
                }
                body["tools"] = serde_json::Value::Array(tools_with_cache);
            } else {
                body["tools"] = serde_json::Value::Array(tools.clone());
            }
        }
    }

    if let Some(temp) = opts.temperature {
        body["temperature"] = json!(temp);
    }
    body
}

/// Drives the SSE receive loop, sending parsed [`Delta`] items into `tx`.
///
/// Returns `(input_tokens, output_tokens, ttft_ms)` extracted from the stream.
#[allow(clippy::too_many_lines)]
async fn run_sse_loop(
    resp: reqwest::Response,
    tx: &tokio::sync::mpsc::Sender<Result<Delta, AdapterError>>,
    request_start: std::time::Instant,
) -> (Option<u32>, Option<u32>, Option<i64>) {
    let mut bytes_stream = resp.bytes_stream();
    // Raw byte buffer: multibyte UTF-8 chars split across chunk boundaries are
    // only decoded once a complete line has arrived (see `drain_complete_lines`).
    let mut buf: Vec<u8> = Vec::new();
    // Track the current SSE `event:` type across lines.
    let mut current_event: Option<String> = None;
    // Track the active tool_use block (name) for input_json_delta chunks.
    let mut current_tool_name: Option<String> = None;
    let mut input_tok: Option<u32> = None;
    let mut output_tok: Option<u32> = None;
    let mut ttft_ms: Option<i64> = None;

    while let Some(chunk) = bytes_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(AdapterError::Http(e))).await;
                return (input_tok, output_tok, ttft_ms);
            }
        };

        for line in crate::sse::drain_complete_lines(&mut buf, &chunk) {
            if line.is_empty() {
                // Blank line separates SSE events; reset pending event type.
                current_event = None;
                continue;
            }

            if let Some(ev) = line.strip_prefix("event: ") {
                current_event = Some(ev.to_owned());
                continue;
            }

            if let Some(data) = crate::sse::strip_sse_data(&line) {
                let Some(ev) = &current_event else {
                    continue;
                };

                // Handle tool-use content block events before the generic parser.
                if ev == "content_block_start" {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                        if v.get("content_block")
                            .and_then(|b| b.get("type"))
                            .and_then(serde_json::Value::as_str)
                            == Some("tool_use")
                        {
                            current_tool_name = v
                                .get("content_block")
                                .and_then(|b| b.get("name"))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned);
                        }
                    }
                    continue;
                }

                if ev == "content_block_stop" {
                    current_tool_name = None;
                    continue;
                }

                if ev == "content_block_delta" {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(partial) = v
                            .get("delta")
                            .filter(|d| {
                                d.get("type").and_then(serde_json::Value::as_str)
                                    == Some("input_json_delta")
                            })
                            .and_then(|d| d.get("partial_json"))
                            .and_then(serde_json::Value::as_str)
                        {
                            let name = current_tool_name.clone().unwrap_or_default();
                            if tx
                                .send(Ok(Delta::ToolCallChunk {
                                    name,
                                    partial_input: partial.to_owned(),
                                }))
                                .await
                                .is_err()
                            {
                                return (input_tok, output_tok, ttft_ms);
                            }
                            continue;
                        }
                    }
                }

                match parse_anthropic_event(ev, data) {
                    Ok(Some(delta)) => {
                        if matches!(delta, Delta::Text(_)) && ttft_ms.is_none() {
                            ttft_ms = Some(
                                request_start
                                    .elapsed()
                                    .as_millis()
                                    .try_into()
                                    .unwrap_or(i64::MAX),
                            );
                        }
                        if let Delta::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                        } = delta
                        {
                            input_tok = Some(input_tokens);
                            output_tok = Some(output_tokens);
                            if tx
                                .send(Ok(Delta::Usage {
                                    input_tokens,
                                    output_tokens,
                                    cache_read_tokens,
                                }))
                                .await
                                .is_err()
                            {
                                return (input_tok, output_tok, ttft_ms);
                            }
                        } else if tx.send(Ok(delta)).await.is_err() {
                            return (input_tok, output_tok, ttft_ms);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return (input_tok, output_tok, ttft_ms);
                    }
                }
            }
        }
    }
    (input_tok, output_tok, ttft_ms)
}

impl Provider for AnthropicProvider {
    #[allow(clippy::too_many_lines)]
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        const BASE_URL: &str = "https://api.anthropic.com/v1/messages";
        let api_key = self.api_key.clone();
        let client = self.client.clone();
        let body = build_body(messages, opts);

        // Capture parent context so the LLM span is a child of the agent invoke span.
        let parent_cx = opentelemetry::Context::current();
        let model_name_for_span = body["model"].as_str().unwrap_or("").to_owned();
        let max_tokens_for_span: Option<i64> = body["max_tokens"].as_i64();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Delta, AdapterError>>(64);

        tokio::spawn(async move {
            use opentelemetry::{
                global,
                trace::{Span as _, Tracer as _},
            };
            use smedja_telemetry as tel;

            let tracer = global::tracer("smedja");
            let mut llm_span = tracer.start_with_context(tel::SPAN_LLM_CHAT, &parent_cx);
            llm_span.set_attribute(opentelemetry::KeyValue::new(
                tel::OPERATION_NAME,
                tel::OPERATION_CHAT,
            ));
            llm_span.set_attribute(opentelemetry::KeyValue::new(
                tel::GEN_AI_SYSTEM,
                "anthropic",
            ));
            llm_span.set_attribute(opentelemetry::KeyValue::new(
                tel::REQUEST_MODEL,
                model_name_for_span.clone(),
            ));
            if let Some(mt) = max_tokens_for_span {
                llm_span.set_attribute(opentelemetry::KeyValue::new(
                    "gen_ai.request.max_tokens",
                    mt,
                ));
            }

            // Apply capture policy for prompt content (section 7.1-7.3).
            let system_content: String = body
                .get("system")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            match tel::prompt_capture_mode() {
                tel::CaptureMode::Hash => {
                    llm_span.set_attribute(opentelemetry::KeyValue::new(
                        "smedja.prompt.hash",
                        tel::content_hash(&system_content),
                    ));
                }
                tel::CaptureMode::Scrubbed => {
                    llm_span.set_attribute(opentelemetry::KeyValue::new(
                        "gen_ai.prompt",
                        tel::scrub_and_summarise(&system_content),
                    ));
                }
                tel::CaptureMode::Full => {
                    let scrubbed = tel::scrub_and_summarise(&system_content);
                    llm_span.set_attribute(opentelemetry::KeyValue::new("gen_ai.prompt", scrubbed));
                }
            }

            let request_start = std::time::Instant::now();

            let mut headers = reqwest::header::HeaderMap::new();
            // Static header names/values: infallible for these known-good literals.
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&api_key) {
                headers.insert("x-api-key", val);
            }
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            headers.insert(
                "anthropic-version",
                reqwest::header::HeaderValue::from_static("2023-06-01"),
            );
            if body.get("system").and_then(|s| s.as_array()).is_some() {
                // Prompt caching is active; opt in to the beta feature.
                headers.insert(
                    "anthropic-beta",
                    reqwest::header::HeaderValue::from_static("prompt-caching-2024-07-31"),
                );
            }
            inject_traceparent(&mut headers);

            let resp = match client
                .post(BASE_URL)
                .headers(headers)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    llm_span.set_status(opentelemetry::trace::Status::error("HTTP request failed"));
                    llm_span.end();
                    let _ = tx.send(Err(AdapterError::Http(e))).await;
                    return;
                }
            };

            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs);
                llm_span.set_status(opentelemetry::trace::Status::error("rate limited"));
                llm_span.end();
                let _ = tx
                    .send(Err(AdapterError::RateLimited { retry_after }))
                    .await;
                return;
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let err = crate::classify_http_error(status, &text);
                llm_span.set_status(opentelemetry::trace::Status::error(format!(
                    "HTTP {status}"
                )));
                llm_span.end();
                let _ = tx.send(Err(err)).await;
                return;
            }

            let (in_tok, out_tok, ttft) = run_sse_loop(resp, &tx, request_start).await;
            if let Some(v) = in_tok {
                llm_span.set_attribute(opentelemetry::KeyValue::new(
                    tel::INPUT_TOKENS,
                    i64::from(v),
                ));
            }
            if let Some(v) = out_tok {
                llm_span.set_attribute(opentelemetry::KeyValue::new(
                    tel::OUTPUT_TOKENS,
                    i64::from(v),
                ));
            }
            if let Some(v) = ttft {
                llm_span.set_attribute(opentelemetry::KeyValue::new(tel::TTFT_MS, v));
            }
            // Record the response capture policy so backends know what to expect.
            llm_span.set_attribute(opentelemetry::KeyValue::new(
                "smedja.capture.responses",
                tel::response_capture_mode().as_str(),
            ));
            llm_span.set_status(opentelemetry::trace::Status::Ok);
            llm_span.end();
        });

        Box::pin(ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_traceparent_does_not_panic_with_no_active_span() {
        // The adapter only *uses* the global propagator via the facade; it never
        // installs one (that is the binary's responsibility). With no propagator
        // installed and no active span, injection must be a safe no-op.
        let mut headers = reqwest::header::HeaderMap::new();
        inject_traceparent(&mut headers);
        // No assertion on header presence — background context produces no traceparent.
        // The test passes as long as no panic occurs.
    }

    #[test]
    fn split_multibyte_delta_decoded_intact() {
        // The 4 bytes of 😀 are split across two byte chunks; byte-level buffering
        // must reassemble the character with no U+FFFD replacement char.
        let payload = "event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"😀\"}}\n";
        let bytes = payload.as_bytes();
        let split = payload.find('😀').unwrap() + 2; // mid 4-byte sequence
        let mut buf: Vec<u8> = Vec::new();
        let mut lines = crate::sse::drain_complete_lines(&mut buf, &bytes[..split]);
        lines.extend(crate::sse::drain_complete_lines(&mut buf, &bytes[split..]));

        assert!(
            lines.iter().all(|l| !l.contains('\u{FFFD}')),
            "no replacement char in any decoded line"
        );
        let data = lines
            .iter()
            .find_map(|l| crate::sse::strip_sse_data(l))
            .expect("a data: frame must be present");
        let delta =
            parse_anthropic_event("content_block_delta", data).expect("parse must not error");
        assert_eq!(delta, Some(Delta::Text("😀".to_owned())));
    }

    fn base_opts() -> CallOptions {
        CallOptions {
            model: "claude-opus-4-5".into(),
            max_tokens: Some(512),
            temperature: None,
            system: None,
            tools: None,
            provider_session_id: None,
            smedja_session_id: None,
            permission_mode: None,
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
            workspace: None,
            tool_gate: None,
        }
    }

    #[test]
    fn build_body_omits_tools_key_when_tools_is_none() {
        let opts = base_opts();
        let body = build_body(&[], &opts);
        assert!(
            body.get("tools").is_none(),
            "tools key must be absent when CallOptions::tools is None"
        );
    }

    #[test]
    fn build_body_omits_tools_key_when_tools_vec_is_empty() {
        let mut opts = base_opts();
        opts.tools = Some(vec![]);
        let body = build_body(&[], &opts);
        assert!(
            body.get("tools").is_none(),
            "tools key must be absent when the tools Vec is empty"
        );
    }

    #[test]
    fn build_body_injects_tools_when_non_empty() {
        let tool = serde_json::json!({
            "name": "bash",
            "description": "Run a bash command",
            "input_schema": { "type": "object" }
        });
        let mut opts = base_opts();
        opts.tools = Some(vec![tool.clone()]);

        let body = build_body(&[], &opts);
        let injected = body
            .get("tools")
            .expect("tools key must be present when tools Vec is non-empty");
        let arr = injected.as_array().expect("tools value must be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], tool);
    }

    #[test]
    fn build_body_preserves_tool_order() {
        let tool_a = serde_json::json!({ "name": "a" });
        let tool_b = serde_json::json!({ "name": "b" });
        let mut opts = base_opts();
        opts.tools = Some(vec![tool_a.clone(), tool_b.clone()]);

        let body = build_body(&[], &opts);
        let arr = body["tools"].as_array().unwrap();
        assert_eq!(arr[0], tool_a);
        assert_eq!(arr[1], tool_b);
    }

    // ── cache hint tests ──────────────────────────────────────────────────────

    fn cache_opts() -> CallOptions {
        CallOptions {
            stable_prefix_len: Some(0),
            system: Some("You are a test assistant.".to_owned()),
            ..base_opts()
        }
    }

    #[test]
    fn build_body_without_cache_hint_keeps_system_as_string() {
        let mut opts = base_opts();
        opts.system = Some("sys".to_owned());
        let body = build_body(&[], &opts);
        assert_eq!(body["system"], serde_json::json!("sys"));
    }

    #[test]
    fn build_body_with_cache_hint_formats_system_as_content_block() {
        let opts = cache_opts();
        let body = build_body(&[], &opts);
        let sys = body["system"]
            .as_array()
            .expect("system must be an array when caching");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_body_with_cache_hint_marks_last_tool() {
        let tool_a = serde_json::json!({ "name": "tool_a" });
        let tool_b = serde_json::json!({ "name": "tool_b" });
        let mut opts = cache_opts();
        opts.tools = Some(vec![tool_a, tool_b]);
        let body = build_body(&[], &opts);
        let arr = body["tools"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(
            arr[0].get("cache_control").is_none(),
            "first tool must not be marked"
        );
        assert_eq!(
            arr[1]["cache_control"]["type"], "ephemeral",
            "last tool must be marked"
        );
    }

    #[test]
    fn build_body_with_cache_hint_marks_message_at_prefix_boundary() {
        let mut opts = cache_opts();
        opts.stable_prefix_len = Some(1); // cache message at index 0
        let messages = vec![
            Message {
                role: Role::User,
                content: "initial context".to_owned(),
            },
            Message {
                role: Role::Assistant,
                content: "ok".to_owned(),
            },
        ];
        let body = build_body(&messages, &opts);
        let arr = body["messages"].as_array().unwrap();
        // Message at index 0 should be a content-block array with cache_control.
        assert!(
            arr[0]["content"].is_array(),
            "message at prefix boundary must use content-block format"
        );
        assert_eq!(arr[0]["content"][0]["cache_control"]["type"], "ephemeral");
        // Message at index 1 should be a plain string.
        assert!(
            arr[1]["content"].is_string(),
            "message after prefix boundary must be a plain string"
        );
    }

    #[test]
    fn build_body_with_prefix_zero_does_not_mark_any_message() {
        let mut opts = cache_opts();
        opts.stable_prefix_len = Some(0); // only system/tools cached
        let messages = vec![Message {
            role: Role::User,
            content: "hello".to_owned(),
        }];
        let body = build_body(&messages, &opts);
        let arr = body["messages"].as_array().unwrap();
        assert!(
            arr[0]["content"].is_string(),
            "no message should be marked when prefix_len == 0"
        );
    }
}
