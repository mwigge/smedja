//! Gemini streaming adapter — `GEMINI_API_KEY` HTTP adapter.
//!
//! Targets `https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent`
//! and translates the SSE response into a [`DeltaStream`].

use std::collections::HashMap;

use reqwest::Client;
use serde_json::json;
use tokio_stream::{wrappers::ReceiverStream, StreamExt as _};

use crate::{AdapterError, CallOptions, Delta, DeltaStream, Message, Provider, Role};

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

/// Gemini streaming chat-completion provider.
///
/// Sends requests to the Gemini `streamGenerateContent` endpoint using an API
/// key sourced from the `GEMINI_API_KEY` environment variable.
pub struct GeminiProvider {
    client: Client,
    api_key: String,
}

impl GeminiProvider {
    /// Creates a new [`GeminiProvider`] reading `GEMINI_API_KEY` from the
    /// environment.
    ///
    /// # Errors
    ///
    /// Returns an error string if `GEMINI_API_KEY` is not set.
    pub fn from_env() -> Result<Self, AdapterError> {
        let api_key = std::env::var("GEMINI_API_KEY").map_err(|_| {
            AdapterError::Request("GEMINI_API_KEY environment variable is not set".to_owned())
        })?;
        Ok(Self {
            client: Client::new(),
            api_key,
        })
    }

    /// Creates a new [`GeminiProvider`] with an explicit API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
        }
    }

    /// Creates a new [`GeminiProvider`] with a pre-configured [`reqwest::Client`].
    pub fn with_client(client: Client, api_key: impl Into<String>) -> Self {
        Self {
            client,
            api_key: api_key.into(),
        }
    }
}

/// Builds a Gemini `contents` array from messages.
///
/// Gemini does not support a separate system-prompt field at the top level of
/// the request body used here.  A leading `Role::System` message is treated as
/// the first user turn so that the conversation begins with user content as
/// Gemini requires.
fn build_contents(messages: &[Message], opts: &CallOptions) -> Vec<serde_json::Value> {
    let mut contents: Vec<serde_json::Value> = Vec::new();

    // Inject `opts.system` as the first user turn when present.
    if let Some(sys) = &opts.system {
        contents.push(json!({
            "role": "user",
            "parts": [{ "text": sys }]
        }));
    }

    for m in messages {
        let role = match m.role {
            Role::System | Role::User | Role::Tool => "user",
            Role::Assistant => "model",
        };
        contents.push(json!({
            "role": role,
            "parts": [{ "text": m.content }]
        }));
    }

    contents
}

/// Parses a single Gemini SSE data payload into a [`Delta`].
///
/// Gemini streams JSON objects whose candidates carry incremental text:
/// `{ "candidates": [{ "content": { "parts": [{ "text": "…" }] } }] }`.
///
/// Returns `None` when the payload carries no text (e.g. safety-only events).
///
/// # Errors
///
/// Returns [`AdapterError::Parse`] if the data is not valid JSON.
pub(crate) fn parse_gemini_line(data: &str) -> Result<Option<Delta>, AdapterError> {
    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| AdapterError::Parse(e.to_string()))?;

    // Usage metadata in the final chunk.
    if let Some(meta) = v.get("usageMetadata") {
        let raw_in = meta
            .get("promptTokenCount")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let raw_out = meta
            .get("candidatesTokenCount")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if raw_in > 0 || raw_out > 0 {
            return Ok(Some(Delta::Usage {
                input_tokens: u32::try_from(raw_in).unwrap_or(u32::MAX),
                output_tokens: u32::try_from(raw_out).unwrap_or(u32::MAX),
            }));
        }
    }

    // Incremental text from candidates[0].content.parts[0].text.
    if let Some(text) = v
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("text"))
        .and_then(serde_json::Value::as_str)
    {
        if !text.is_empty() {
            return Ok(Some(Delta::Text(text.to_owned())));
        }
    }

    Ok(None)
}

impl Provider for GeminiProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        let model = opts.model.clone();
        let api_key = self.api_key.clone();
        let client = self.client.clone();

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?key={api_key}&alt=sse"
        );

        let contents = build_contents(messages, opts);
        let mut body = json!({ "contents": contents });

        if let Some(mt) = opts.max_tokens {
            body["generationConfig"] = json!({ "maxOutputTokens": mt });
        }
        if let Some(temp) = opts.temperature {
            body["generationConfig"]
                .as_object_mut()
                .map(|o| o.insert("temperature".to_owned(), json!(temp)));
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Delta, AdapterError>>(64);

        tokio::spawn(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            inject_traceparent(&mut headers);

            let resp = match client.post(&url).headers(headers).json(&body).send().await {
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

            let mut bytes_stream = resp.bytes_stream();
            let mut buf = String::new();

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

                    if let Some(data) = line.strip_prefix("data: ") {
                        match parse_gemini_line(data) {
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

            // Flush any remaining partial line.
            let leftover = buf.trim_end_matches('\r').trim_end_matches('\n');
            if let Some(data) = leftover.strip_prefix("data: ") {
                match parse_gemini_line(data) {
                    Ok(Some(delta)) => {
                        let _ = tx.send(Ok(delta)).await;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                    }
                }
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta() {
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"Hello, world!"}],"role":"model"},"finishReason":"STOP"}]}"#;
        let result = parse_gemini_line(data).expect("parse must not error");
        assert_eq!(result, Some(Delta::Text("Hello, world!".to_owned())));
    }

    #[test]
    fn parse_usage_metadata() {
        let data = r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}}"#;
        let result = parse_gemini_line(data).expect("parse must not error");
        assert_eq!(
            result,
            Some(Delta::Usage {
                input_tokens: 10,
                output_tokens: 5
            })
        );
    }

    #[test]
    fn parse_empty_candidate_returns_none() {
        let data = r#"{"candidates":[{"content":{"parts":[{"text":""}]}}]}"#;
        let result = parse_gemini_line(data).expect("parse must not error");
        assert_eq!(result, None);
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        let result = parse_gemini_line("not-json");
        assert!(result.is_err(), "invalid JSON must return an error");
    }

    #[test]
    fn from_env_errors_when_key_absent() {
        let saved = std::env::var("GEMINI_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        let result = GeminiProvider::from_env();
        if let Some(v) = saved {
            std::env::set_var("GEMINI_API_KEY", v);
        }
        assert!(
            result.is_err(),
            "from_env must fail when GEMINI_API_KEY is absent"
        );
    }

    #[test]
    fn from_env_succeeds_when_key_present() {
        std::env::set_var("GEMINI_API_KEY", "test-key");
        let result = GeminiProvider::from_env();
        std::env::remove_var("GEMINI_API_KEY");
        assert!(result.is_ok());
    }

    #[test]
    fn build_contents_injects_system_from_opts() {
        let opts = CallOptions {
            model: "gemini-2.5-pro".into(),
            max_tokens: None,
            temperature: None,
            system: Some("Be helpful.".to_owned()),
            tools: None,
            provider_session_id: None,
            stable_prefix_len: None,
        };
        let contents = build_contents(&[], &opts);
        assert_eq!(contents.len(), 1);
        let first = &contents[0];
        assert_eq!(first["role"], "user");
        assert_eq!(first["parts"][0]["text"], "Be helpful.");
    }

    #[test]
    fn build_contents_maps_assistant_to_model_role() {
        let opts = CallOptions {
            model: "gemini-2.5-pro".into(),
            max_tokens: None,
            temperature: None,
            system: None,
            tools: None,
            provider_session_id: None,
            stable_prefix_len: None,
        };
        let messages = vec![Message {
            role: Role::Assistant,
            content: "I can help.".to_owned(),
        }];
        let contents = build_contents(&messages, &opts);
        assert_eq!(contents[0]["role"], "model");
    }
}
