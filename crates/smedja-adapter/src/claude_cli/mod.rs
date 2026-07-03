//! Claude CLI provider for the `claude` binary.

mod gate;
mod parse;

use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AdapterError, AnthropicProvider, CallOptions, DeltaStream, Message, Provider,
    SubprocessProvider,
};

use gate::install_tool_gate;
use parse::{parse_line, render_conversation};

/// Runs the `claude` CLI binary if available; falls back to [`AnthropicProvider`].
pub enum ClaudeCliProvider {
    /// Uses the locally installed `claude` CLI binary.
    Cli,
    /// Delegates to the Anthropic HTTP API using an API key.
    Api(AnthropicProvider),
}

impl ClaudeCliProvider {
    /// Selects CLI if the `claude` binary is on `$PATH`, otherwise uses the API key.
    ///
    /// Returns `None` if neither is available.
    #[must_use]
    pub fn detect(api_key: Option<String>) -> Option<Self> {
        if SubprocessProvider::available("claude") {
            Some(Self::Cli)
        } else {
            api_key.map(|key| Self::Api(AnthropicProvider::new(key)))
        }
    }
}

impl Provider for ClaudeCliProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        match self {
            Self::Cli => stream_claude_cli(messages, opts),
            Self::Api(p) => p.stream_chat(messages, opts),
        }
    }
}

fn stream_claude_cli(messages: &[Message], opts: &CallOptions) -> DeltaStream {
    // Render the FULL conversation into the prompt and deliver it on stdin.
    // We do NOT use `--resume`: it depends on the CLI's own conversation store,
    // which is unreliable under the daemon's working directory / sandbox and
    // fails with "No conversation found" (exit 1) on the second turn. milliways
    // takes the same approach — assemble the whole prompt upstream, no resume.
    let prompt = render_conversation(messages);
    let model = opts.model.clone();
    let session_id = opts.smedja_session_id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::spawn(async move {
        // Mirror the proven milliways invocation. Notably:
        //  * NO `--bare`: that flag selects a credential path that ignores the
        //    logged-in Claude session and fails with "Not logged in".
        //  * The prompt is delivered on STDIN, not as a positional argv element —
        //    a large prompt (system preamble + context) as a single argv entry
        //    overflows MAX_ARG_STRLEN (128 KiB) and execve fails with E2BIG.
        let mut command = tokio::process::Command::new("claude");
        command
            .arg("--print")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Disable claude's own bwrap subprocess isolation. The smedja
            // landlock sandbox and the cowork PreToolUse gate hook (installed
            // below) already provide the confinement boundary. Without this,
            // bwrap fails with EAFNOSUPPORT when AF_NETLINK is blocked by an
            // outer seccomp filter (e.g. when smdjad itself runs inside a
            // Claude Code session).
            .env("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB", "0")
            // So an interrupted turn (turn.cancel aborts the run task) kills the
            // child instead of leaking a runaway `claude` process.
            .kill_on_drop(true);

        if !model.is_empty() {
            command.arg("--model").arg(&model);
        }

        // Install the PreToolUse approval hook so claude's own tool calls are
        // gated through smedja's permission policy (ask → approve/deny).
        install_tool_gate(&mut command, session_id.as_deref());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = tx.send(Err(AdapterError::Request(e.to_string()))).await;
                return;
            }
        };

        // Write the prompt to stdin and close it so claude (in --print mode with
        // no positional prompt) reads the request to completion.
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }

        let stderr = child.stderr.take();
        if let Some(stdout) = child.stdout.take() {
            use tokio::io::AsyncBufReadExt as _;
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(delta) = parse_line(&line) {
                            if tx.send(Ok(delta)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(AdapterError::Request(e.to_string()))).await;
                        break;
                    }
                }
            }
        }

        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let stderr_text = read_stderr(stderr).await;
                let detail = if stderr_text.trim().is_empty() {
                    status.to_string()
                } else {
                    format!("{status}: {}", stderr_text.trim())
                };
                let _ = tx.send(Err(AdapterError::Request(detail))).await;
            }
            Err(e) => {
                let _ = tx.send(Err(AdapterError::Request(e.to_string()))).await;
            }
        }
    });

    Box::pin(ReceiverStream::new(rx))
}

async fn read_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    use tokio::io::AsyncReadExt as _;
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = stderr.read_to_string(&mut buf).await;
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Delta;
    use futures_util::StreamExt as _;

    // Shared process-wide lock so env-mutating tests serialise across modules.
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    #[test]
    fn detect_returns_none_when_no_binary_and_no_key() {
        // Serialise with other $PATH-mutating tests, and read availability and
        // detect under the same lock so the two observations stay consistent
        // (otherwise a concurrent test can change $PATH between them).
        let _guard = ENV_LOCK.lock().unwrap();
        let has_cli = SubprocessProvider::available("claude");
        let provider = ClaudeCliProvider::detect(None);
        if has_cli {
            assert!(matches!(provider, Some(ClaudeCliProvider::Cli)));
        } else {
            assert!(provider.is_none());
        }
    }

    #[test]
    fn detect_returns_api_when_no_binary_but_key_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        if SubprocessProvider::available("claude") {
            // CLI wins over API key; skip this case.
            return;
        }
        let provider = ClaudeCliProvider::detect(Some("test-key".into()));
        assert!(matches!(provider, Some(ClaudeCliProvider::Api(_))));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn cli_provider_streams_mock_claude_via_stdin_without_resume() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!(
            "smedja-claude-mock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let args_file = temp_dir.join("args.txt");
        let stdin_file = temp_dir.join("stdin.txt");
        let script_path = temp_dir.join("claude");
        // Record argv and stdin, then emit a minimal stream-json transcript.
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\nprintf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"mock-session\"}}'\nprintf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"hello\"}}]}}}}'\n",
                args_file.display(),
                stdin_file.display()
            ),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script_path, permissions).unwrap();
        }

        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", temp_dir.display()));

        let provider = ClaudeCliProvider::detect(None).expect("mock claude should be detected");
        assert!(matches!(provider, ClaudeCliProvider::Cli));
        let opts = CallOptions {
            model: "claude-mock".into(),
            max_tokens: None,
            temperature: None,
            system: None,
            tools: None,
            provider_session_id: Some("resume-123".into()),
            smedja_session_id: None,
            permission_mode: None,
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
            workspace: None,
        };
        let messages = vec![Message {
            role: crate::Role::User,
            content: "hi".into(),
        }];

        let mut stream = provider.stream_chat(&messages, &opts);
        let mut deltas = Vec::new();
        while let Some(item) = stream.next().await {
            deltas.push(item.unwrap());
        }

        std::env::set_var("PATH", old_path);

        assert!(deltas.contains(&Delta::SessionId("mock-session".into())));
        assert!(deltas.contains(&Delta::Text("hello".into())));
        let args = std::fs::read_to_string(&args_file).unwrap();
        // `--bare` selects a credential path that ignores the logged-in session
        // ("Not logged in"); it must never be passed.
        assert!(
            !args.contains("--bare"),
            "--bare breaks auth and must not be used; args were:\n{args}"
        );
        // `--resume` depends on the CLI's own conversation store and fails under
        // the daemon (exit 1, "No conversation found"); the full conversation is
        // rendered into the prompt instead, so resume must never be passed even
        // when a provider_session_id is set.
        assert!(
            !args.contains("--resume"),
            "--resume must not be used; args were:\n{args}"
        );
        // The prompt must be delivered on stdin, not as a positional argv entry
        // (argv overflows MAX_ARG_STRLEN for large prompts → E2BIG).
        let stdin = std::fs::read_to_string(&stdin_file).unwrap();
        assert_eq!(stdin, "hi", "prompt must arrive on stdin");
        assert!(
            !args.contains("hi"),
            "prompt must not be passed as an argv element; args were:\n{args}"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
