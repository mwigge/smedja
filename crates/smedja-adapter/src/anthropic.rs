//! Anthropic Messages API streaming adapter.

use std::collections::HashMap;

use reqwest::Client;
use serde_json::json;
use tokio_stream::{wrappers::ReceiverStream, StreamExt as _};

use crate::{
    sse::parse_anthropic_event, AdapterError, CallOptions, Delta, DeltaStream, Message, Provider,
    Role,
};

/// Injects a W3C `traceparent` header into `headers` using the current `OTel` context.
///
/// If no propagator has been installed (e.g. in tests without `OTel` setup), the
/// function is a no-op.  If the current span context is invalid (background
/// context), the propagator will not emit a `traceparent` value, so no header is
/// added.
fn inject_traceparent(headers: &mut reqwest::header::HeaderMap) {
    let cx = opentelemetry::Context::current();
    let mut map: HashMap<String, String> = HashMap::new();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut map);
    });
    for (k, v) in &map {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            headers.insert(name, value);
        }
    }
}

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
    // Anthropic requires the system prompt as a top-level field; extract it
    // from `opts.system` first, then fall back to the first System message.
    let system_content: Option<String> = opts.system.clone().or_else(|| {
        messages
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| m.content.clone())
    });

    let msg_array: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|m| {
            json!({
                "role": role_to_str(&m.role),
                "content": m.content,
            })
        })
        .collect();

    let mut body = json!({
        "model": opts.model,
        "messages": msg_array,
        "stream": true,
        "max_tokens": opts.max_tokens.unwrap_or(1024),
    });
    if let Some(sys) = system_content {
        body["system"] = json!(sys);
    }
    if let Some(tools) = &opts.tools {
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools.clone());
        }
    }
    if let Some(temp) = opts.temperature {
        body["temperature"] = json!(temp);
    }
    body
}

/// Drives the SSE receive loop, sending parsed [`Delta`] items into `tx`.
async fn run_sse_loop(
    resp: reqwest::Response,
    tx: &tokio::sync::mpsc::Sender<Result<Delta, AdapterError>>,
) {
    let mut bytes_stream = resp.bytes_stream();
    let mut buf = String::new();
    // Track the current SSE `event:` type across lines.
    let mut current_event: Option<String> = None;

    while let Some(chunk) = bytes_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(AdapterError::Http(e))).await;
                return;
            }
        };

        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_owned();
            buf.drain(..=nl);

            if line.is_empty() {
                // Blank line separates SSE events; reset pending event type.
                current_event = None;
                continue;
            }

            if let Some(ev) = line.strip_prefix("event: ") {
                current_event = Some(ev.to_owned());
                continue;
            }

            if let Some(data) = line.strip_prefix("data: ") {
                if let Some(ev) = &current_event {
                    match parse_anthropic_event(ev, data) {
                        Ok(Some(delta)) => {
                            if tx.send(Ok(delta)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
            }
        }
    }
}

impl Provider for AnthropicProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        const BASE_URL: &str = "https://api.anthropic.com/v1/messages";
        let api_key = self.api_key.clone();
        let client = self.client.clone();
        let body = build_body(messages, opts);

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Delta, AdapterError>>(64);

        tokio::spawn(async move {
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
                let _ = tx.send(Err(AdapterError::RateLimited { retry_after })).await;
                return;
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let _ = tx
                    .send(Err(AdapterError::InvalidResponse(format!(
                        "HTTP {status}: {text}"
                    ))))
                    .await;
                return;
            }

            run_sse_loop(resp, &tx).await;
        });

        Box::pin(ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_traceparent_does_not_panic_with_no_active_span() {
        // Without an active span the W3C propagator emits nothing; verify no crash.
        use opentelemetry_sdk::propagation::TraceContextPropagator;
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let mut headers = reqwest::header::HeaderMap::new();
        inject_traceparent(&mut headers);
        // No assertion on header presence — background context produces no traceparent.
        // The test passes as long as no panic occurs.
    }

    fn base_opts() -> CallOptions {
        CallOptions {
            model: "claude-opus-4-5".into(),
            max_tokens: Some(512),
            temperature: None,
            system: None,
            tools: None,
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
}
