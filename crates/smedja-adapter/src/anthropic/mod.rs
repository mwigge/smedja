//! Anthropic Messages API streaming adapter.

mod body;
mod sse_loop;

use reqwest::Client;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    otel::inject_traceparent, AdapterError, CallOptions, Delta, DeltaStream, Message, Provider,
};

use body::build_body;
use sse_loop::run_sse_loop;

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
            client: crate::streaming_http_client(),
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
}
