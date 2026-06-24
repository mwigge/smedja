//! Cross-cutting helpers shared by the daemon entrypoint, the turn
//! orchestrator, and the loop runner.
//!
//! This module imports only adapter/bellows/std types — never `handlers` or the
//! `main` entrypoint — so the orchestrator and executor depend on it rather than
//! depending upward on the binary's `main.rs` free functions.

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
        "impl" => Some(AgentRole::Impl),
        "test" => Some(AgentRole::Test),
        "review" => Some(AgentRole::Review),
        "sre" => Some(AgentRole::Sre),
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
/// Returns `Ok((full_response, input_tokens, output_tokens, provider_session_id))`
/// on success, or `Err(reason)` if the stream yields an error item. Each
/// `Delta::Text` chunk is forwarded to `dispatcher` as a
/// [`TurnEvent::AssistantDelta`].
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
) -> Result<(String, u32, u32, Option<String>), DrainError> {
    let mut full_response = String::new();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
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
            })) => {
                input_tokens = i;
                output_tokens = n;
            }
            Some(Ok(Delta::ToolCall { name, input })) => {
                let input_summary: String = input.to_string().chars().take(120).collect();
                let line = format!("▶ {name}({input_summary})");
                full_response.push_str(&line);
                full_response.push('\n');
                dispatcher.publish(TurnEvent::ToolCalled {
                    tool_name: name,
                    input_summary,
                    turn_id: turn_id.map(str::to_owned),
                    correlation: CorrelationCtx {
                        operation_name: Some(tel::OPERATION_EXECUTE_TOOL.to_owned()),
                        ..correlation.clone()
                    },
                    tool_call_id: None,
                });
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: line,
                    turn_id: turn_id.map(str::to_owned),
                    correlation: correlation.clone(),
                });
            }
            Some(Ok(Delta::ToolResult {
                tool_use_id,
                content,
            })) => {
                let line = format!("✓ {tool_use_id} -> {} chars", content.chars().count());
                full_response.push_str(&line);
                full_response.push('\n');
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: line,
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
        provider_session_id,
    ))
}

#[cfg(test)]
mod tests {
    use smedja_adapter::AdapterError;

    use super::*;

    fn error_stream(err: AdapterError) -> smedja_adapter::DeltaStream {
        Box::pin(futures_util::stream::iter(vec![Err(err)]))
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
