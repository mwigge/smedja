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
pub mod claude_cli;
pub mod codex_cli;
pub mod copilot;
pub mod crush;
pub mod error;
pub mod gemini;
pub mod local;
pub mod openai;
pub mod openai_compat;
pub mod poolside;
pub mod provider;
pub mod subprocess;
pub mod types;

pub(crate) mod otel;
pub(crate) mod sse;

/// A single process-wide lock serialising tests that mutate global process
/// state (e.g. `PATH`). Per-module locks do not serialise across modules, so
/// CLI-provider tests in different modules raced on `PATH` under the parallel
/// test harness; sharing one lock removes that race.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub use anthropic::AnthropicProvider;
pub use claude_cli::ClaudeCliProvider;
pub use codex_cli::CodexCliProvider;
pub use copilot::CopilotProvider;
pub use crush::{
    code_compressor, command_compressor, compress_command_output, compress_tool_result,
    smart_crusher, trim_code_block, ContentPipeline, Transform,
};
pub use error::{classify_http_error, AdapterError};
pub use gemini::GeminiProvider;
pub use local::{LocalCapability, LocalProvider};
pub use openai::OpenAiProvider;
pub use openai_compat::{
    BergetProvider, MinimaxProvider, OpenAiCompatProvider, OpenAiCompatSpec, OpenCodeProvider,
};
pub use poolside::PoolsideProvider;
pub use provider::{DeltaStream, Provider};
pub use subprocess::SubprocessProvider;
pub use types::{CacheStrategy, CallOptions, Delta, Message, Role};
