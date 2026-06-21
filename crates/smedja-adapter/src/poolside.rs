//! Poolside provider — CLI only, no API-key fallback.

use crate::{CallOptions, DeltaStream, Message, Provider, SubprocessProvider};

/// Runs the `poolside` CLI binary if available.
pub struct PoolsideProvider(SubprocessProvider);

impl PoolsideProvider {
    /// Returns `Some(Self)` only if the `poolside` binary is on `$PATH`.
    pub fn detect() -> Option<Self> {
        if which::which("poolside").is_ok() {
            Some(Self(SubprocessProvider::new("poolside", vec![])))
        } else {
            None
        }
    }
}

impl Provider for PoolsideProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        self.0.stream_chat(messages, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_when_binary_absent() {
        // `poolside` is not expected to be installed in CI.
        if which::which("poolside").is_ok() {
            assert!(PoolsideProvider::detect().is_some());
        } else {
            assert!(PoolsideProvider::detect().is_none());
        }
    }
}
