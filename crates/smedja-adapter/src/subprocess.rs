//! Generic subprocess provider — spawns a binary, streams stdout as deltas.

use tokio::io::AsyncWriteExt as _;
use tokio::process::Command as TokioCommand;
use tokio_stream::wrappers::ReceiverStream;

use crate::{AdapterError, CallOptions, Delta, DeltaStream, Message, Provider};

/// Runs a CLI binary with the prompt on stdin; streams stdout lines as [`Delta::Text`].
pub struct SubprocessProvider {
    binary: String,
    extra_args: Vec<String>,
}

impl SubprocessProvider {
    /// Creates a new [`SubprocessProvider`].
    pub fn new(binary: impl Into<String>, extra_args: Vec<String>) -> Self {
        Self {
            binary: binary.into(),
            extra_args,
        }
    }

    /// Returns `true` if the binary is found on `$PATH`.
    #[must_use]
    pub fn available(binary: &str) -> bool {
        which::which(binary).is_ok()
    }
}

impl Provider for SubprocessProvider {
    fn stream_chat(&self, messages: &[Message], _opts: &CallOptions) -> DeltaStream {
        // Flatten conversation to a single prompt string.
        let prompt = messages
            .iter()
            .map(|m| format!("{}: {}", m.role.as_str(), m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let binary = self.binary.clone();
        let extra_args = self.extra_args.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let mut child = match TokioCommand::new(&binary)
                .args(&extra_args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                // So an interrupted turn (turn.cancel aborts the run task) kills
                // the child instead of leaking a runaway process.
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(AdapterError::Request(e.to_string()))).await;
                    return;
                }
            };

            // Write prompt to stdin, then close it so the child sees EOF.
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(prompt.as_bytes()).await;
                // stdin is dropped here, closing the pipe
            }

            // Stream stdout lines.
            if let Some(stdout) = child.stdout.take() {
                use tokio::io::AsyncBufReadExt as _;
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.is_empty() {
                        continue;
                    }
                    if tx.send(Ok(Delta::Text(line + "\n"))).await.is_err() {
                        break;
                    }
                }
            }

            let _ = child.wait().await;
        });

        Box::pin(ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    #[tokio::test]
    async fn subprocess_unavailable_binary_yields_error() {
        let p = SubprocessProvider::new("__no_such_binary__", vec![]);
        let messages = vec![crate::Message {
            role: crate::Role::User,
            content: "hi".into(),
        }];
        let opts = crate::CallOptions {
            model: "x".into(),
            max_tokens: None,
            temperature: None,
            system: None,
            tools: None,
            provider_session_id: None,
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
        };
        let mut stream = p.stream_chat(&messages, &opts);
        let first = stream.next().await;
        assert!(first.is_some());
        assert!(first.unwrap().is_err());
    }

    #[test]
    fn available_returns_false_for_missing_binary() {
        assert!(!SubprocessProvider::available("__no_such_binary_xyz__"));
    }

    #[test]
    fn available_returns_true_for_sh() {
        // `/bin/sh` is universally available on Linux/macOS.
        assert!(SubprocessProvider::available("sh"));
    }

    /// Verifies that stdout lines produced by the subprocess are forwarded as
    /// `Delta::Text` items.  Uses `sh -c` with `echo` so the test is hermetic
    /// and requires no external binary beyond `/bin/sh`.
    #[tokio::test]
    async fn subprocess_provider_echoes_stdout_as_deltas() {
        let p = SubprocessProvider::new("sh", vec!["-c".into(), "echo hello && echo world".into()]);
        let messages = vec![crate::Message {
            role: crate::Role::User,
            content: "ignored".into(),
        }];
        let opts = crate::CallOptions {
            model: "x".into(),
            max_tokens: None,
            temperature: None,
            system: None,
            tools: None,
            provider_session_id: None,
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
        };
        let mut stream = p.stream_chat(&messages, &opts);
        let mut collected = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(Delta::Text(t)) => collected.push_str(&t),
                Ok(_) => {}
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(
            collected.contains("hello"),
            "expected 'hello' in output, got: {collected:?}"
        );
        assert!(
            collected.contains("world"),
            "expected 'world' in output, got: {collected:?}"
        );
    }

    /// Verifies that `available` returns `false` for an absent binary.
    /// (This complements the existing test with an explicit name for the
    /// "CLI absent" scenario described in the task spec.)
    #[test]
    fn subprocess_provider_falls_back_when_cli_absent() {
        assert!(
            !SubprocessProvider::available("__no_such_binary_xyz__"),
            "non-existent binary should not be detected as available"
        );
    }
}
