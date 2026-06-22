//! `OpenAI` streaming chat-completion adapter.

use std::collections::HashMap;

use reqwest::Client;
use serde_json::json;
use tokio_stream::{wrappers::ReceiverStream, StreamExt as _};

use crate::{
    sse::parse_openai_line, AdapterError, CallOptions, Delta, DeltaStream, Message, Provider, Role,
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

/// `OpenAI`-compatible streaming chat-completion provider.
///
/// Sends requests to the `/v1/chat/completions` endpoint and translates the
/// SSE response into a [`DeltaStream`].
pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

impl OpenAiProvider {
    /// Creates a new [`OpenAiProvider`].
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    /// Creates a new [`OpenAiProvider`] with a pre-configured [`reqwest::Client`].
    pub fn with_client(
        client: Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

fn role_to_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[allow(clippy::too_many_lines)]
impl Provider for OpenAiProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let auth = format!("Bearer {}", self.api_key);
        let client = self.client.clone();

        // Build the messages array; prepend system if provided via `CallOptions`.
        let mut msg_array: Vec<serde_json::Value> = Vec::new();
        if let Some(sys) = &opts.system {
            msg_array.push(json!({"role": "system", "content": sys}));
        }
        for m in messages {
            msg_array.push(json!({
                "role": role_to_str(&m.role),
                "content": m.content,
            }));
        }

        let mut body = json!({
            "model": opts.model,
            "messages": msg_array,
            "stream": true,
        });
        if let Some(mt) = opts.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        if let Some(temp) = opts.temperature {
            body["temperature"] = json!(temp);
        }

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
            llm_span.set_attribute(opentelemetry::KeyValue::new(tel::GEN_AI_SYSTEM, "openai"));
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
            let request_start = std::time::Instant::now();

            let mut headers = reqwest::header::HeaderMap::new();
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&auth) {
                headers.insert(reqwest::header::AUTHORIZATION, val);
            }
            inject_traceparent(&mut headers);

            let resp = match client.post(&url).headers(headers).json(&body).send().await {
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
                llm_span.set_status(opentelemetry::trace::Status::error(format!(
                    "HTTP {status}"
                )));
                llm_span.end();
                let _ = tx
                    .send(Err(AdapterError::InvalidResponse(format!(
                        "HTTP {status}: {text}"
                    ))))
                    .await;
                return;
            }

            let mut bytes_stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut input_tok: Option<u32> = None;
            let mut output_tok: Option<u32> = None;
            let mut ttft_ms: Option<i64> = None;

            'outer: while let Some(chunk) = bytes_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(Err(AdapterError::Http(e))).await;
                        break 'outer;
                    }
                };

                buf.push_str(&String::from_utf8_lossy(&chunk));

                // Process all complete newline-terminated lines.
                while let Some(nl) = buf.find('\n') {
                    let line = buf[..nl].trim_end_matches('\r').to_owned();
                    buf.drain(..=nl);

                    if let Some(data) = line.strip_prefix("data: ") {
                        match parse_openai_line(data) {
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
                                } = delta
                                {
                                    input_tok = Some(input_tokens);
                                    output_tok = Some(output_tokens);
                                    if tx
                                        .send(Ok(Delta::Usage {
                                            input_tokens,
                                            output_tokens,
                                        }))
                                        .await
                                        .is_err()
                                    {
                                        break 'outer;
                                    }
                                } else if tx.send(Ok(delta)).await.is_err() {
                                    break 'outer;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let _ = tx.send(Err(e)).await;
                                break 'outer;
                            }
                        }
                    }
                }
            }

            // Flush any remaining partial line (no trailing newline).
            let leftover = buf.trim_end_matches('\r').trim_end_matches('\n');
            if let Some(data) = leftover.strip_prefix("data: ") {
                match parse_openai_line(data) {
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
                        } = delta
                        {
                            input_tok = Some(input_tokens);
                            output_tok = Some(output_tokens);
                            let _ = tx
                                .send(Ok(Delta::Usage {
                                    input_tokens,
                                    output_tokens,
                                }))
                                .await;
                        } else {
                            let _ = tx.send(Ok(delta)).await;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                    }
                }
            }

            if let Some(v) = input_tok {
                llm_span.set_attribute(opentelemetry::KeyValue::new(
                    tel::INPUT_TOKENS,
                    i64::from(v),
                ));
            }
            if let Some(v) = output_tok {
                llm_span.set_attribute(opentelemetry::KeyValue::new(
                    tel::OUTPUT_TOKENS,
                    i64::from(v),
                ));
            }
            if let Some(v) = ttft_ms {
                llm_span.set_attribute(opentelemetry::KeyValue::new(tel::TTFT_MS, v));
            }
            llm_span.set_status(opentelemetry::trace::Status::Ok);
            llm_span.end();
        });

        Box::pin(ReceiverStream::new(rx))
    }
}
