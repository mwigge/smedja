//! Cross-cutting helpers shared by the daemon entrypoint, the turn
//! orchestrator, and the loop runner.
//!
//! This module imports only adapter/bellows/std types — never `handlers` or the
//! `main` entrypoint — so the orchestrator and executor depend on it rather than
//! depending upward on the binary's `main.rs` free functions.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use smedja_adapter::Delta;
use smedja_assayer::{AgentRole, Runner};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::{Dispatcher, TurnEvent};

use smedja_telemetry as tel;

/// Escapes the XML metacharacters `&`, `<`, and `>` in `s`.
#[must_use]
pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Returns the current Unix time as fractional seconds since the epoch.
#[must_use]
pub(crate) fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Resolves the daemon workspace root from the `SMEDJA_WORKSPACE` env var,
/// defaulting to the relative `"."` when unset.
///
/// This consolidates the inline `"."`-defaulting lookups; the startup default
/// (an absolute cwd) is resolved separately by `resolve_workspace_root` in
/// `main.rs` and is intentionally not changed here.
#[must_use]
pub(crate) fn workspace_root() -> std::path::PathBuf {
    std::env::var("SMEDJA_WORKSPACE")
        .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from)
}

/// Extracts `(trace_id, span_id)` strings from the currently active `OTel` span.
///
/// Returns `(None, None)` when no valid span is active (e.g. in tests without an
/// `OTel` pipeline installed). This mirrors the private `current_span_ids` helper
/// in `smedja_bellows`, which is not exported and therefore cannot be reused
/// directly without touching that crate.
#[must_use]
pub(crate) fn current_span_ids() -> (Option<String>, Option<String>) {
    use opentelemetry::trace::TraceContextExt as _;
    let cx = opentelemetry::Context::current();
    let sc = cx.span().span_context().clone();
    if sc.is_valid() {
        (
            Some(format!("{}", sc.trace_id())),
            Some(format!("{}", sc.span_id())),
        )
    } else {
        (None, None)
    }
}

/// Maps a session mode string to an [`AgentRole`] for routing purposes.
#[must_use]
pub(crate) fn parse_session_mode_to_role(mode: &str) -> Option<AgentRole> {
    match mode {
        "impl" | "code" => Some(AgentRole::Impl),
        "plan" => Some(AgentRole::Plan),
        "research" => Some(AgentRole::Research),
        "debug" => Some(AgentRole::Debug),
        "ask" | "explain" => Some(AgentRole::Ask),
        "test" => Some(AgentRole::Test),
        "review" => Some(AgentRole::Review),
        "sre" => Some(AgentRole::Sre),
        "data" | "sql" => Some(AgentRole::Data),
        "iac" | "infra" => Some(AgentRole::Iac),
        "orchestrator" => Some(AgentRole::Orchestrator),
        _ => None,
    }
}

/// Maps a [`Runner`] enum value to the short string used in the session-resume store.
#[must_use]
pub(crate) fn runner_session_key(runner: Runner) -> &'static str {
    match runner {
        Runner::Claude => "claude-cli",
        Runner::Codex => "codex-cli",
        Runner::Local => "local",
        Runner::Copilot => "copilot",
        Runner::Minimax => "minimax",
        Runner::Berget => "berget",
    }
}

/// Parses a user-supplied or stored runner string to a [`Runner`] enum value.
///
/// Accepts both canonical keys (`"claude-cli"`) and short aliases (`"claude"`).
#[must_use]
pub(crate) fn parse_runner_str(s: &str) -> Option<Runner> {
    match s {
        "claude" | "claude-cli" => Some(Runner::Claude),
        "codex" | "codex-cli" => Some(Runner::Codex),
        "local" => Some(Runner::Local),
        "copilot" => Some(Runner::Copilot),
        "minimax" => Some(Runner::Minimax),
        "berget" => Some(Runner::Berget),
        _ => None,
    }
}

/// Maximum number of tool-dispatch iterations in a single turn.
/// Override with `SMEDJA_MAX_TOOL_TURNS` (e.g. `SMEDJA_MAX_TOOL_TURNS=5`).
/// Values above 50 are clamped to 50 to prevent runaway LLM loops.
const MAX_TOOL_TURNS: usize = 10;

/// Returns the effective per-turn tool-iteration cap, honouring
/// `SMEDJA_MAX_TOOL_TURNS` (clamped to 50).
#[must_use]
pub(crate) fn effective_max_tool_turns() -> usize {
    std::env::var("SMEDJA_MAX_TOOL_TURNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(MAX_TOOL_TURNS, |n| n.min(50))
}

/// Hard wall-clock cap (in seconds) for a single agent turn, covering all
/// provider rotations and tool-loop iterations combined.
///
/// Override with `SMEDJA_TURN_TIMEOUT_S` (e.g. `SMEDJA_TURN_TIMEOUT_S=600`).
/// Defaults to 900 s (15 min).
#[must_use]
pub(crate) fn effective_agent_timeout_s() -> u64 {
    std::env::var("SMEDJA_TURN_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900) // 15-minute hard cap per turn
}

/// Error returned by [`drain_stream`], distinguishing rate-limit responses and
/// rotatable provider failures from other failures so callers can apply an
/// appropriate recovery strategy.
pub(crate) enum DrainError {
    /// The provider returned HTTP 429; back off for `retry_after` before retrying
    /// the **same** provider.
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },
    /// A retryable provider failure (quota exhausted, context-length exceeded, or
    /// provider down) that rotation to another eligible provider may recover.
    ///
    /// `kind` is the stable `smedja.error.kind` classification from
    /// [`smedja_adapter::AdapterError::kind`].
    Rotatable {
        kind: &'static str,
        retry_after: Option<std::time::Duration>,
    },
    /// Any other stream-level error; treat as fatal for this turn.
    Other(String),
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited { retry_after } => {
                write!(f, "rate limited by provider (retry after {retry_after:?})")
            }
            Self::Rotatable { kind, retry_after } => {
                write!(
                    f,
                    "rotatable provider failure ({kind}, retry after {retry_after:?})"
                )
            }
            Self::Other(s) => f.write_str(s),
        }
    }
}

/// Drains `stream`, accumulating text deltas into a single string.
///
/// Returns
/// `Ok((full_response, input_tokens, output_tokens, cache_read_tokens, provider_session_id))`
/// on success, or `Err(reason)` if the stream yields an error item. Each
/// `Delta::Text` chunk is forwarded to `dispatcher` as a
/// [`TurnEvent::AssistantDelta`]. `cache_read_tokens` is the maximum
/// provider-reported `cache_read_input_tokens` seen on the stream (Anthropic
/// emits usage across two events; taking the max captures the cache figure).
///
/// # Errors
///
/// Returns [`DrainError::RateLimited`] on an HTTP 429 response and
/// [`DrainError::Other`] for any other stream-level error.
pub(crate) async fn drain_stream(
    mut stream: smedja_adapter::DeltaStream,
    dispatcher: &Dispatcher,
    turn_id: Option<&str>,
    correlation: &CorrelationCtx,
) -> Result<(String, u32, u32, u32, Option<String>), DrainError> {
    let mut full_response = String::new();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    let mut cache_read_tokens = 0u32;
    let mut provider_session_id = None;
    loop {
        match stream.next().await {
            None => break,
            Some(Ok(Delta::Text(t))) => {
                full_response.push_str(&t);
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: t,
                    turn_id: turn_id.map(str::to_owned),
                    correlation: correlation.clone(),
                });
            }
            Some(Ok(Delta::Usage {
                input_tokens: i,
                output_tokens: n,
                cache_read_tokens: c,
            })) => {
                // Anthropic splits usage across message_start (input + cache) and
                // message_delta (output); a zero from one event must not clobber a
                // non-zero from the other, so accumulate the max per field.
                input_tokens = input_tokens.max(i);
                output_tokens = output_tokens.max(n);
                cache_read_tokens = cache_read_tokens.max(c);
            }
            Some(Ok(Delta::ToolCall { name, input })) => {
                // A human-readable one-line summary (the command, path, pattern…)
                // instead of raw truncated JSON.
                let input_summary = summarize_tool_input(&input);
                let _ = writeln!(full_response, "▶ {name}: {input_summary}");
                // The full input (capped) backs the on-demand detail view.
                let full_input: String = input.to_string().chars().take(4096).collect();
                // Publish ONLY the structured tool_call event; the UI renders the
                // card from it. Previously we ALSO published the same line as an
                // AssistantDelta, so every tool call rendered twice.
                dispatcher.publish(TurnEvent::ToolCalled {
                    tool_name: name,
                    input_summary,
                    full_input: Some(full_input),
                    turn_id: turn_id.map(str::to_owned),
                    correlation: CorrelationCtx {
                        operation_name: Some(tel::OPERATION_EXECUTE_TOOL.to_owned()),
                        ..correlation.clone()
                    },
                    tool_call_id: None,
                });
            }
            Some(Ok(Delta::ToolResult {
                tool_use_id: _,
                content,
            })) => {
                // A readable result summary (status + first-line preview) instead
                // of the opaque `✓ <tool_use_id> -> N chars`. Newline-framed so it
                // lands on its own line in the message panel rather than merging
                // into the surrounding assistant text.
                let line = summarize_tool_result(&content);
                full_response.push_str(&line);
                full_response.push('\n');
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: format!("\n{line}\n"),
                    turn_id: turn_id.map(str::to_owned),
                    correlation: correlation.clone(),
                });
            }
            Some(Ok(Delta::SessionId(id))) => {
                provider_session_id = Some(id);
            }
            Some(Err(smedja_adapter::AdapterError::RateLimited { retry_after })) => {
                return Err(DrainError::RateLimited { retry_after });
            }
            Some(Err(e)) => {
                // Retryable quota / context-length / provider-down failures
                // rotate to another provider; everything else is fatal for this
                // turn. Classification lives at the adapter boundary.
                if e.is_retryable() {
                    return Err(DrainError::Rotatable {
                        kind: e.kind(),
                        retry_after: None,
                    });
                }
                return Err(DrainError::Other(e.to_string()));
            }
        }
    }
    Ok((
        full_response,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        provider_session_id,
    ))
}

/// Collapses a string to a single trimmed line and caps it at `max` display
/// chars with an ellipsis — for compact tool summaries.
fn truncate_summary(s: &str, max: usize) -> String {
    let one_line: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() > max {
        let cut: String = trimmed.chars().take(max).collect();
        format!("{cut}…")
    } else {
        trimmed
    }
}

/// Distils a tool-call input JSON value into a human one-liner: the most
/// meaningful field (command / path / pattern / query / url …) when present,
/// else a compact `key=value` join. Keeps tool cards readable instead of dumping
/// raw truncated JSON.
fn summarize_tool_input(input: &serde_json::Value) -> String {
    const KEYS: &[&str] = &[
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
    ];
    if let Some(obj) = input.as_object() {
        for k in KEYS {
            if let Some(v) = obj.get(*k).and_then(serde_json::Value::as_str) {
                return truncate_summary(v, 100);
            }
        }
        let parts: Vec<String> = obj
            .iter()
            .take(4)
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect();
        if !parts.is_empty() {
            return truncate_summary(&parts.join(" "), 100);
        }
    }
    truncate_summary(&input.to_string(), 100)
}

/// Summarises a tool result as `↳ <status> · <first-line preview>` (or a char
/// count when there is no textual preview), classifying obvious failures.
fn summarize_tool_result(content: &str) -> String {
    let lc = content.to_lowercase();
    // Strong, unambiguous failure signatures — checked anywhere in the result so
    // errors like "EROFS: read-only file system" aren't mislabelled "ok".
    let is_err = lc.trim_start().starts_with("error")
        || lc.contains("permission denied")
        || lc.contains("read-only file system")
        || lc.contains("erofs")
        || lc.contains("operation not permitted")
        || lc.contains("no such file or directory")
        || lc.contains("command not found")
        || lc.contains("was blocked");
    let status = if is_err { "error" } else { "ok" };
    let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let preview = truncate_summary(first, 100);
    if preview.is_empty() {
        format!("↳ {status} · {} chars", content.chars().count())
    } else {
        format!("↳ {status} · {preview}")
    }
}

#[cfg(test)]
mod tests {
    use smedja_adapter::AdapterError;

    use super::*;

    fn error_stream(err: AdapterError) -> smedja_adapter::DeltaStream {
        Box::pin(futures_util::stream::iter(vec![Err(err)]))
    }

    #[test]
    fn summarize_tool_input_prefers_meaningful_field() {
        let v = serde_json::json!({"command": "find . -type f", "timeout": 5});
        assert_eq!(summarize_tool_input(&v), "find . -type f");
        let v2 = serde_json::json!({"file_path": "/a/b.rs"});
        assert_eq!(summarize_tool_input(&v2), "/a/b.rs");
    }

    #[test]
    fn summarize_tool_input_falls_back_to_key_values() {
        let v = serde_json::json!({"foo": "bar", "n": 3});
        let s = summarize_tool_input(&v);
        assert!(s.contains("foo=bar"), "{s}");
    }

    #[test]
    fn summarize_tool_result_classifies_and_previews() {
        let ok = summarize_tool_result("hello world\nmore");
        assert!(ok.starts_with("↳ ok · hello world"), "{ok}");
        let err = summarize_tool_result("error: nope");
        assert!(err.starts_with("↳ error ·"), "{err}");
        // Failure signatures anywhere in the body are caught, not just at the start.
        let erofs = summarize_tool_result("EROFS: read-only file system, mkdir '/x'");
        assert!(erofs.starts_with("↳ error ·"), "{erofs}");
        // No textual preview → fall back to a char count, never a tool_use_id.
        let empty = summarize_tool_result("");
        assert!(empty.starts_with("↳ ok · 0 chars"), "{empty}");
    }

    #[tokio::test]
    async fn drain_stream_maps_quota_error_to_rotatable() {
        let dispatcher = Dispatcher::new(8);
        let result = drain_stream(
            error_stream(AdapterError::QuotaExhausted(
                "insufficient_quota".to_owned(),
            )),
            &dispatcher,
            Some("turn-1"),
            &CorrelationCtx::default(),
        )
        .await;
        match result {
            Err(DrainError::Rotatable { kind, .. }) => {
                assert_eq!(kind, "quota_exhausted");
            }
            _ => panic!("quota error must map to DrainError::Rotatable"),
        }
    }

    #[tokio::test]
    async fn drain_stream_maps_context_length_error_to_rotatable() {
        let dispatcher = Dispatcher::new(8);
        let result = drain_stream(
            error_stream(AdapterError::ContextLengthExceeded(
                "prompt is too long".to_owned(),
            )),
            &dispatcher,
            Some("turn-1"),
            &CorrelationCtx::default(),
        )
        .await;
        match result {
            Err(DrainError::Rotatable { kind, .. }) => {
                assert_eq!(kind, "context_length_exceeded");
            }
            _ => panic!("context-length error must map to DrainError::Rotatable"),
        }
    }

    #[tokio::test]
    async fn drain_stream_maps_rate_limited_to_rate_limited() {
        let dispatcher = Dispatcher::new(8);
        let result = drain_stream(
            error_stream(AdapterError::RateLimited { retry_after: None }),
            &dispatcher,
            Some("turn-1"),
            &CorrelationCtx::default(),
        )
        .await;
        assert!(
            matches!(result, Err(DrainError::RateLimited { .. })),
            "429 must map to DrainError::RateLimited, not Rotatable"
        );
    }

    #[tokio::test]
    async fn drain_stream_maps_parse_error_to_other() {
        let dispatcher = Dispatcher::new(8);
        let result = drain_stream(
            error_stream(AdapterError::Parse("bad json".to_owned())),
            &dispatcher,
            Some("turn-1"),
            &CorrelationCtx::default(),
        )
        .await;
        assert!(
            matches!(result, Err(DrainError::Other(_))),
            "non-retryable parse error must map to DrainError::Other"
        );
    }
}
