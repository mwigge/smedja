//! SSE receive loop for the Anthropic Messages API stream.

use tokio_stream::StreamExt as _;

use crate::{sse::parse_anthropic_event, AdapterError, Delta};

/// Drives the SSE receive loop, sending parsed [`Delta`] items into `tx`.
///
/// Returns `(input_tokens, output_tokens, ttft_ms)` extracted from the stream.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_sse_loop(
    resp: reqwest::Response,
    tx: &tokio::sync::mpsc::Sender<Result<Delta, AdapterError>>,
    request_start: std::time::Instant,
) -> (Option<u32>, Option<u32>, Option<i64>) {
    let mut bytes_stream = resp.bytes_stream();
    let mut buf = String::new();
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
