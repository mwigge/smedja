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
pub mod berget;
pub mod claude_cli;
pub mod codex_cli;
pub mod copilot;
pub mod crush;
pub mod error;
pub mod gemini;
pub mod local;
pub mod minimax;
pub mod openai;
pub mod opencode;
pub mod poolside;
pub mod provider;
pub mod subprocess;
pub mod types;

pub(crate) mod otel;
pub(crate) mod sse;

pub use anthropic::AnthropicProvider;
pub use berget::BergetProvider;
pub use claude_cli::ClaudeCliProvider;
pub use codex_cli::CodexCliProvider;
pub use copilot::CopilotProvider;
pub use crush::{
    compress_command_output, compress_tool_result, trim_code_block, CodeCompressor,
    CommandCompressor, ContentPipeline, SmartCrusher, Transform,
};
pub use error::AdapterError;
pub use gemini::GeminiProvider;
pub use local::{LocalCapability, LocalProvider};
pub use minimax::MinimaxProvider;
pub use openai::OpenAiProvider;
pub use opencode::OpenCodeProvider;
pub use poolside::PoolsideProvider;
pub use provider::{DeltaStream, Provider};
pub use subprocess::SubprocessProvider;
pub use types::{CallOptions, Delta, Message, Role};
