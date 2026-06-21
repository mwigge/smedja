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
//! | [`LocalProvider`] | Local rs-llmctl instance (OpenAI-compatible) |

pub mod anthropic;
pub mod claude_cli;
pub mod codex_cli;
pub mod error;
pub mod local;
pub mod openai;
pub mod provider;
pub mod subprocess;
pub mod types;

pub(crate) mod sse;

pub use anthropic::AnthropicProvider;
pub use claude_cli::ClaudeCliProvider;
pub use codex_cli::CodexCliProvider;
pub use error::AdapterError;
pub use local::{LocalCapability, LocalProvider};
pub use openai::OpenAiProvider;
pub use provider::{DeltaStream, Provider};
pub use subprocess::SubprocessProvider;
pub use types::{CallOptions, Delta, Message, Role};
