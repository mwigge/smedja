//! Anthropic Messages API streaming adapter.

use reqwest::Client;
use serde_json::json;
use tokio_stream::{wrappers::ReceiverStream, StreamExt as _};

use crate::{
    sse::parse_anthropic_event, AdapterError, CallOptions, Delta, DeltaStream, Message, Provider,
    Role,
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
            let resp = match client
                .post(BASE_URL)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
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
