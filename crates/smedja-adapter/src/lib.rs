//! HTTP streaming adapters for LLM providers (`OpenAI`, Anthropic, Gemini).
//!
//! # Overview
//!
//! Each provider implements the [`Provider`] trait, which exposes a single
//! method [`Provider::stream_chat`] that returns a [`DeltaStream`] — an
//! async stream of [`Delta`] items representing incremental model output.
//!
//! # Providers
//!
//! | Struct | API |
//! |--------|-----|
//! | [`OpenAiProvider`] | `OpenAI` chat completions (streaming) |
//! | [`AnthropicProvider`] | Anthropic Messages API (streaming) |
//! | [`GeminiProvider`] | Google Gemini streaming API |
//! | [`OpenCodeProvider`] | OpenCode OpenAI-compatible API |
//! | [`LocalProvider`] | Local rs-llmctl instance (OpenAI-compatible) |
//! | [`CopilotProvider`] | GitHub Copilot CLI or API |
//! | [`PoolsideProvider`] | Poolside CLI |
//! | [`MinimaxProvider`] | Minimax HTTP API |
//! | [`BergetProvider`] | Berget AI HTTP API |

pub mod anthropic;
pub mod bedrock;
pub mod claude_cli;
pub mod codex_cli;
pub mod copilot;
pub mod crush;
pub mod error;
pub mod gemini;
pub mod gpu;
pub mod local;
pub mod ollama;
pub mod openai;
pub mod openai_compat;
pub mod pool_cli;
pub mod poolside;
pub mod provider;
pub mod subprocess;
pub mod types;

pub(crate) mod otel;
pub(crate) mod sse;

/// Builds a [`reqwest::Client`] tuned for streaming LLM calls.
///
/// A plain `Client::new()` has no timeouts, so a stalled connection or a model
/// that stops emitting mid-stream hangs the turn forever. An overall
/// `.timeout()` is wrong for streaming (it would kill a legitimately long
/// response), so this sets a connect timeout plus a per-read idle timeout:
/// the stream is only aborted when *no* bytes arrive for `read_timeout`.
///
/// Falls back to an untimed client on the (practically impossible) TLS-init
/// failure rather than panicking in an infallible constructor.
pub(crate) fn streaming_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_mins(2))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// A single process-wide lock serialising tests that mutate global process
/// state (e.g. `PATH`). Per-module locks do not serialise across modules, so
/// CLI-provider tests in different modules raced on `PATH` under the parallel
/// test harness; sharing one lock removes that race.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub use anthropic::AnthropicProvider;
pub use bedrock::{AwsCredentials, BedrockProvider};
pub use claude_cli::ClaudeCliProvider;
pub use codex_cli::CodexCliProvider;
pub use copilot::CopilotProvider;
pub use crush::{
    code_compressor, command_compressor, compress_command_output, compress_command_output_with,
    compress_tool_result, dedup_lines, group_by_directory, smart_crusher, smart_filter,
    trim_code_block, truncate_lines, ContentPipeline, FilterEntry, FilterParams, FilterRegistry,
    FilterStrategy, Transform,
};
pub use error::{classify_http_error, AdapterError};
pub use gemini::GeminiProvider;
pub use gpu::{detect_gpu, fit_for, parse_gpu_snapshot, Fit, GpuSnapshot};
pub use local::{
    fetch_inventory, install_model, issue_swap_request, parse_model_inventory, record_local_swap,
    InstallOutcome, LocalCapability, LocalModel, LocalProvider, SwapOutcome,
};
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;
pub use openai_compat::{
    BergetProvider, DeepSeekProvider, GroqProvider, MinimaxProvider, OpenAiCompatProvider,
    OpenAiCompatSpec, OpenCodeProvider, PerplexityProvider, TogetherProvider, XAiProvider,
};
pub use pool_cli::PoolCliProvider;
pub use poolside::PoolsideProvider;
pub use provider::{DeltaStream, Provider};
pub use subprocess::SubprocessProvider;
pub use types::{CacheStrategy, CallOptions, Delta, Message, Role};
