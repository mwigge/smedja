//! `GenAI` semantic convention constants and span-builder helpers for smedja.
//!
//! All attribute key strings follow the OpenTelemetry `GenAI` semantic conventions
//! (<https://opentelemetry.io/docs/specs/semconv/gen-ai/>) with smedja-specific
//! extensions under the `smedja.*` namespace.

use opentelemetry::{Key, KeyValue};

// ─── GenAI semantic convention keys ───────────────────────────────────────────

/// `gen_ai.conversation.id` — stable identifier shared across all turns in one
/// interactive session or one autonomous loop slice.
pub const CONV_ID: Key = Key::from_static_str("gen_ai.conversation.id");

/// `gen_ai.agent.name` — identifies the role or agent type:
/// `orchestrator`, `tester`, `implementer`, `reviewer`, `fix`, `sre`, or
/// `interactive` for user-driven sessions.
pub const AGENT_NAME: Key = Key::from_static_str("gen_ai.agent.name");

/// `gen_ai.operation.name` — operation taxonomy value for the span.
/// Use `OPERATION_*` constants for consistent values.
pub const OPERATION_NAME: Key = Key::from_static_str("gen_ai.operation.name");

/// `gen_ai.system` — LLM provider identifier (e.g. `anthropic`, `openai`,
/// `ollama`).
pub const GEN_AI_SYSTEM: Key = Key::from_static_str("gen_ai.system");

/// `gen_ai.request.model` — the model name requested by the caller.
pub const REQUEST_MODEL: Key = Key::from_static_str("gen_ai.request.model");

/// `gen_ai.response.model` — the model name returned by the provider, when
/// different from the requested model.
pub const RESPONSE_MODEL: Key = Key::from_static_str("gen_ai.response.model");

/// `gen_ai.usage.input_tokens`
pub const INPUT_TOKENS: Key = Key::from_static_str("gen_ai.usage.input_tokens");

/// `gen_ai.usage.output_tokens`
pub const OUTPUT_TOKENS: Key = Key::from_static_str("gen_ai.usage.output_tokens");

/// `gen_ai.usage.cache_read_input_tokens` — tokens read from provider cache.
pub const CACHE_READ_TOKENS: Key = Key::from_static_str("gen_ai.usage.cache_read_input_tokens");

/// `gen_ai.usage.cache_creation_input_tokens` — tokens written to provider cache.
pub const CACHE_WRITE_TOKENS: Key =
    Key::from_static_str("gen_ai.usage.cache_creation_input_tokens");

/// `gen_ai.tool.call.id` — stable identifier for a specific tool call instance.
pub const TOOL_CALL_ID: Key = Key::from_static_str("gen_ai.tool.call.id");

/// `gen_ai.tool.name` — name of the tool being called.
pub const TOOL_NAME: Key = Key::from_static_str("gen_ai.tool.name");

/// `gen_ai.tool.type` — one of `function`, `extension`, `datastore`.
pub const TOOL_TYPE: Key = Key::from_static_str("gen_ai.tool.type");

// ─── Smedja-native keys ───────────────────────────────────────────────────────

/// `smedja.session.id` — the RPC session identifier.
pub const SESSION_ID: Key = Key::from_static_str("smedja.session.id");

/// `smedja.turn.id` — the specific turn (task) identifier.
pub const TURN_ID: Key = Key::from_static_str("smedja.turn.id");

/// `smedja.tier` — execution tier: `local`, `fast`, `deep`.
pub const TIER: Key = Key::from_static_str("smedja.tier");

/// `smedja.loop.id` — autonomous loop run identifier.
pub const LOOP_ID: Key = Key::from_static_str("smedja.loop.id");

/// `smedja.loop.role` — role within a loop: `tester`, `implementer`, `reviewer`,
/// `fix`, `verifier`.
pub const LOOP_ROLE: Key = Key::from_static_str("smedja.loop.role");

/// `smedja.loop.slice` — slice index within the loop run.
pub const LOOP_SLICE: Key = Key::from_static_str("smedja.loop.slice");

/// `smedja.loop.attempt` — retry attempt within a slice.
pub const LOOP_ATTEMPT: Key = Key::from_static_str("smedja.loop.attempt");

/// `smedja.llm.ttft_ms` — time to first token in milliseconds.
pub const TTFT_MS: Key = Key::from_static_str("smedja.llm.ttft_ms");

/// `smedja.error.kind` — classification of the failure.
pub const ERROR_KIND: Key = Key::from_static_str("smedja.error.kind");

/// `smedja.error.retryable` — whether the error is retryable.
pub const ERROR_RETRYABLE: Key = Key::from_static_str("smedja.error.retryable");

/// `smedja.error.count` — cumulative retry count for this operation.
pub const ERROR_COUNT: Key = Key::from_static_str("smedja.error.count");

/// `smedja.tool.args_hash` — SHA-256 prefix of serialized tool arguments.
pub const TOOL_ARGS_HASH: Key = Key::from_static_str("smedja.tool.args_hash");

/// `smedja.tool.result_hash` — SHA-256 prefix of serialized tool result.
pub const TOOL_RESULT_HASH: Key = Key::from_static_str("smedja.tool.result_hash");

/// `smedja.tool.result_tokens` — token count of the tool result.
pub const TOOL_RESULT_TOKENS: Key = Key::from_static_str("smedja.tool.result_tokens");

// ─── Operation name values ────────────────────────────────────────────────────

/// `gen_ai.operation.name` value for agent invocation spans.
pub const OPERATION_INVOKE_AGENT: &str = "invoke_agent";

/// `gen_ai.operation.name` value for LLM chat spans.
pub const OPERATION_CHAT: &str = "chat";

/// `gen_ai.operation.name` value for tool execution spans.
pub const OPERATION_EXECUTE_TOOL: &str = "execute_tool";

// ─── Span name constants ──────────────────────────────────────────────────────

/// Canonical span name for agent invocations.
pub const SPAN_AGENT_INVOKE: &str = "smedja.agent.invoke";

/// Canonical span name for LLM operations.
pub const SPAN_LLM_CHAT: &str = "smedja.llm.chat";

/// Canonical span name for tool executions.
pub const SPAN_TOOL_EXECUTE: &str = "smedja.tool.execute";

/// Canonical span name for loop verification steps.
pub const SPAN_LOOP_VERIFY: &str = "smedja.loop.verify";

// ─── Span builder helpers ─────────────────────────────────────────────────────

/// Returns the [`KeyValue`] pair for `gen_ai.conversation.id`.
#[must_use]
pub fn kv_conv_id(id: impl Into<opentelemetry::StringValue>) -> KeyValue {
    KeyValue::new(CONV_ID, id.into())
}

/// Returns the [`KeyValue`] pair for `gen_ai.agent.name`.
#[must_use]
pub fn kv_agent_name(name: impl Into<opentelemetry::StringValue>) -> KeyValue {
    KeyValue::new(AGENT_NAME, name.into())
}

/// Returns the [`KeyValue`] pair for `gen_ai.operation.name`.
#[must_use]
pub fn kv_operation(op: impl Into<opentelemetry::StringValue>) -> KeyValue {
    KeyValue::new(OPERATION_NAME, op.into())
}

/// Returns the [`KeyValue`] pair for `smedja.session.id`.
#[must_use]
pub fn kv_session_id(id: impl Into<opentelemetry::StringValue>) -> KeyValue {
    KeyValue::new(SESSION_ID, id.into())
}

/// Returns the [`KeyValue`] pair for `smedja.turn.id`.
#[must_use]
pub fn kv_turn_id(id: impl Into<opentelemetry::StringValue>) -> KeyValue {
    KeyValue::new(TURN_ID, id.into())
}

// ─── Capture policy ──────────────────────────────────────────────────────────

/// Content capture mode for prompts, responses, tool args, and tool results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureMode {
    /// Store a SHA-256 hex prefix only. Default — no PII risk.
    #[default]
    Hash,
    /// Store a scrubbed one-line summary. Secrets are redacted.
    Scrubbed,
    /// Store full content. Requires explicit opt-in via environment variable.
    Full,
}

impl CaptureMode {
    /// Resolves capture mode from an environment variable name.
    ///
    /// The variable must be set to `"full"` or `"1"` to enable full capture.
    /// `"scrubbed"` enables scrubbed mode. Anything else → [`Hash`](Self::Hash).
    #[must_use]
    pub fn from_env(var: &str) -> Self {
        match std::env::var(var).as_deref() {
            Ok("full" | "1") => Self::Full,
            Ok("scrubbed") => Self::Scrubbed,
            _ => Self::Hash,
        }
    }
}

/// Environment variable controlling prompt capture.
pub const ENV_CAPTURE_PROMPTS: &str = "SMEDJA_CAPTURE_PROMPTS";
/// Environment variable controlling response capture.
pub const ENV_CAPTURE_RESPONSES: &str = "SMEDJA_CAPTURE_RESPONSES";
/// Environment variable controlling tool argument capture.
pub const ENV_CAPTURE_TOOL_ARGS: &str = "SMEDJA_CAPTURE_TOOL_ARGS";
/// Environment variable controlling tool result capture.
pub const ENV_CAPTURE_TOOL_RESULTS: &str = "SMEDJA_CAPTURE_TOOL_RESULTS";

/// Returns the current capture mode for prompts.
#[must_use]
pub fn prompt_capture_mode() -> CaptureMode {
    CaptureMode::from_env(ENV_CAPTURE_PROMPTS)
}

/// Returns the current capture mode for LLM responses.
#[must_use]
pub fn response_capture_mode() -> CaptureMode {
    CaptureMode::from_env(ENV_CAPTURE_RESPONSES)
}

/// Returns the current capture mode for tool arguments.
#[must_use]
pub fn tool_args_capture_mode() -> CaptureMode {
    CaptureMode::from_env(ENV_CAPTURE_TOOL_ARGS)
}

/// Returns the current capture mode for tool results.
#[must_use]
pub fn tool_results_capture_mode() -> CaptureMode {
    CaptureMode::from_env(ENV_CAPTURE_TOOL_RESULTS)
}

// ─── Content fingerprinting ───────────────────────────────────────────────────

/// Returns a 16-character hex prefix of a hash of `content`.
///
/// Used as the `Hash` capture mode representation for prompts, responses,
/// and tool data. The prefix is long enough to distinguish entries while
/// remaining safe to log.
///
/// Note: uses `std::collections::hash_map::DefaultHasher` which is not
/// cryptographically stable across Rust releases. Use only for deduplication
/// and log correlation, not for security or integrity verification.
#[must_use]
pub fn content_hash(content: &str) -> String {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash_slice(content.as_bytes(), &mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Scrubs common secrets from `content` and returns a one-line summary.
///
/// Replaces API keys, bearer tokens, and Authorization header values with
/// `[REDACTED]`. Returns at most the first 120 characters of the first
/// non-blank, non-secret line.
#[must_use]
pub fn scrub_and_summarise(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            if line.contains("Authorization:")
                || line.contains("Bearer ")
                || line.to_lowercase().contains("api_key")
                || line.to_lowercase().contains("api-key")
                || line.to_lowercase().contains("secret")
                || line.starts_with("sk-")
                || line.starts_with("ant-")
            {
                "[REDACTED]"
            } else {
                line
            }
        })
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_keys_have_expected_names() {
        assert_eq!(CONV_ID.as_str(), "gen_ai.conversation.id");
        assert_eq!(AGENT_NAME.as_str(), "gen_ai.agent.name");
        assert_eq!(OPERATION_NAME.as_str(), "gen_ai.operation.name");
        assert_eq!(TOOL_CALL_ID.as_str(), "gen_ai.tool.call.id");
        assert_eq!(TTFT_MS.as_str(), "smedja.llm.ttft_ms");
    }

    #[test]
    fn operation_name_values_are_correct() {
        assert_eq!(OPERATION_INVOKE_AGENT, "invoke_agent");
        assert_eq!(OPERATION_CHAT, "chat");
        assert_eq!(OPERATION_EXECUTE_TOOL, "execute_tool");
    }

    #[test]
    fn kv_helpers_produce_correct_attributes() {
        let kv = kv_conv_id("sess-001");
        assert_eq!(kv.key.as_str(), "gen_ai.conversation.id");

        let kv2 = kv_agent_name("tester");
        assert_eq!(kv2.key.as_str(), "gen_ai.agent.name");
    }

    #[test]
    fn capture_mode_hash_is_default() {
        assert_eq!(CaptureMode::default(), CaptureMode::Hash);
    }

    #[test]
    fn capture_mode_from_env_full() {
        // SAFETY: single-threaded test; env var is cleaned up immediately after.
        std::env::set_var("SMEDJA_CAPTURE_PROMPTS_TEST_FULL", "full");
        assert_eq!(
            CaptureMode::from_env("SMEDJA_CAPTURE_PROMPTS_TEST_FULL"),
            CaptureMode::Full
        );
        std::env::remove_var("SMEDJA_CAPTURE_PROMPTS_TEST_FULL");
    }

    #[test]
    fn capture_mode_from_env_hash_by_default() {
        std::env::remove_var("SMEDJA_CAPTURE_PROMPTS_TEST_MISSING");
        assert_eq!(
            CaptureMode::from_env("SMEDJA_CAPTURE_PROMPTS_TEST_MISSING"),
            CaptureMode::Hash
        );
    }

    #[test]
    fn content_hash_is_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn scrub_redacts_authorization_bearer_lines() {
        let content = "Authorization: Bearer sk-ant-abc123\nsome normal text";
        let scrubbed = scrub_and_summarise(content);
        assert_eq!(scrubbed, "[REDACTED]");
    }

    #[test]
    fn scrub_returns_first_nonblank_line() {
        let content = "\n\nhello world\nmore text";
        let result = scrub_and_summarise(content);
        assert!(result.starts_with("hello world") || result == "hello world");
    }

    #[test]
    fn capture_mode_scrubbed() {
        // SAFETY: single-threaded test; env var is cleaned up immediately after.
        std::env::set_var("SMEDJA_CAPTURE_RESPONSES_TEST_SCRUB", "scrubbed");
        assert_eq!(
            CaptureMode::from_env("SMEDJA_CAPTURE_RESPONSES_TEST_SCRUB"),
            CaptureMode::Scrubbed
        );
        std::env::remove_var("SMEDJA_CAPTURE_RESPONSES_TEST_SCRUB");
    }

    #[test]
    fn capture_mode_from_env_numeric_one_means_full() {
        // SAFETY: single-threaded test; env var is cleaned up immediately after.
        std::env::set_var("SMEDJA_CAPTURE_TOOL_ARGS_TEST_ONE", "1");
        assert_eq!(
            CaptureMode::from_env("SMEDJA_CAPTURE_TOOL_ARGS_TEST_ONE"),
            CaptureMode::Full
        );
        std::env::remove_var("SMEDJA_CAPTURE_TOOL_ARGS_TEST_ONE");
    }

    #[test]
    fn smedja_keys_have_correct_namespaces() {
        assert_eq!(SESSION_ID.as_str(), "smedja.session.id");
        assert_eq!(TURN_ID.as_str(), "smedja.turn.id");
        assert_eq!(TIER.as_str(), "smedja.tier");
        assert_eq!(LOOP_ID.as_str(), "smedja.loop.id");
        assert_eq!(LOOP_ROLE.as_str(), "smedja.loop.role");
        assert_eq!(LOOP_SLICE.as_str(), "smedja.loop.slice");
        assert_eq!(LOOP_ATTEMPT.as_str(), "smedja.loop.attempt");
        assert_eq!(ERROR_KIND.as_str(), "smedja.error.kind");
        assert_eq!(ERROR_RETRYABLE.as_str(), "smedja.error.retryable");
        assert_eq!(ERROR_COUNT.as_str(), "smedja.error.count");
        assert_eq!(TOOL_ARGS_HASH.as_str(), "smedja.tool.args_hash");
        assert_eq!(TOOL_RESULT_HASH.as_str(), "smedja.tool.result_hash");
        assert_eq!(TOOL_RESULT_TOKENS.as_str(), "smedja.tool.result_tokens");
    }

    #[test]
    fn gen_ai_keys_have_correct_namespaces() {
        assert_eq!(GEN_AI_SYSTEM.as_str(), "gen_ai.system");
        assert_eq!(REQUEST_MODEL.as_str(), "gen_ai.request.model");
        assert_eq!(RESPONSE_MODEL.as_str(), "gen_ai.response.model");
        assert_eq!(INPUT_TOKENS.as_str(), "gen_ai.usage.input_tokens");
        assert_eq!(OUTPUT_TOKENS.as_str(), "gen_ai.usage.output_tokens");
        assert_eq!(
            CACHE_READ_TOKENS.as_str(),
            "gen_ai.usage.cache_read_input_tokens"
        );
        assert_eq!(
            CACHE_WRITE_TOKENS.as_str(),
            "gen_ai.usage.cache_creation_input_tokens"
        );
        assert_eq!(TOOL_NAME.as_str(), "gen_ai.tool.name");
        assert_eq!(TOOL_TYPE.as_str(), "gen_ai.tool.type");
    }

    #[test]
    fn span_name_constants_have_smedja_prefix() {
        assert!(SPAN_AGENT_INVOKE.starts_with("smedja."));
        assert!(SPAN_LLM_CHAT.starts_with("smedja."));
        assert!(SPAN_TOOL_EXECUTE.starts_with("smedja."));
        assert!(SPAN_LOOP_VERIFY.starts_with("smedja."));
    }

    #[test]
    fn kv_operation_produces_correct_key() {
        let kv = kv_operation(OPERATION_CHAT);
        assert_eq!(kv.key.as_str(), "gen_ai.operation.name");
    }

    #[test]
    fn kv_session_and_turn_produce_correct_keys() {
        let kv_s = kv_session_id("session-xyz");
        assert_eq!(kv_s.key.as_str(), "smedja.session.id");

        let kv_t = kv_turn_id("turn-abc");
        assert_eq!(kv_t.key.as_str(), "smedja.turn.id");
    }

    #[test]
    fn content_hash_differs_for_different_inputs() {
        let h1 = content_hash("hello");
        let h2 = content_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn scrub_does_not_redact_normal_lines() {
        let content = "this is a normal log line";
        let result = scrub_and_summarise(content);
        assert_eq!(result, "this is a normal log line");
    }

    #[test]
    fn scrub_truncates_to_120_chars() {
        let long = "a".repeat(200);
        let result = scrub_and_summarise(&long);
        assert_eq!(result.len(), 120);
    }
}
