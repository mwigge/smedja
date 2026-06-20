//! The [`Provider`] trait and the [`DeltaStream`] type alias.

use std::pin::Pin;

use futures_core::Stream;

use crate::{AdapterError, CallOptions, Delta, Message};

/// A pinned, boxed, `Send`-able stream of [`Delta`] items.
pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<Delta, AdapterError>> + Send>>;

/// A provider capable of streaming chat-completion responses.
///
/// Implementations wrap a specific LLM API (`OpenAI`, Anthropic, Gemini, …) and
/// translate it into a uniform [`DeltaStream`].
pub trait Provider: Send + Sync {
    /// Starts a streaming chat-completion request.
    ///
    /// Returns a [`DeltaStream`] that yields [`Delta`] items as the model
    /// generates them.
    ///
    /// # Errors
    ///
    /// Errors from the underlying HTTP transport or SSE parsing are surfaced as
    /// [`AdapterError`] items inside the returned stream.
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream;
}
