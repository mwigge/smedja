//! Claude CLI provider for the `claude` binary.

use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AdapterError, AnthropicProvider, CallOptions, Delta, DeltaStream, Message, Provider,
    SubprocessProvider,
};

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
    let prompt = messages
        .last()
        .map_or_else(String::new, |message| message.content.clone());
    let resume_id = opts.provider_session_id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::spawn(async move {
        let mut command = tokio::process::Command::new("claude");
        command
            .arg("--print")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--bare")
            .arg("--dangerously-skip-permissions");
        if let Some(id) = resume_id.filter(|id| !id.is_empty()) {
            command.arg("--resume").arg(id);
        }
        command
            .arg(prompt)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = tx.send(Err(AdapterError::Request(e.to_string()))).await;
                return;
            }
        };

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

fn parse_line(line: &str) -> Option<Delta> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    if value.get("type").and_then(serde_json::Value::as_str) == Some("system")
        && value.get("subtype").and_then(serde_json::Value::as_str) == Some("init")
    {
        return value
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(|id| Delta::SessionId(id.to_owned()));
    }

    if value.get("type").and_then(serde_json::Value::as_str) == Some("result") {
        if let Some(session_id) = value.get("session_id").and_then(serde_json::Value::as_str) {
            return Some(Delta::SessionId(session_id.to_owned()));
        }
        if let Some(usage) = value.get("usage") {
            return Some(parse_usage(usage));
        }
    }

    let contents = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)?;

    for item in contents {
        match item.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(serde_json::Value::as_str) {
                    return Some(Delta::Text(text.to_owned()));
                }
            }
            Some("tool_use") => {
                let name = item
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let input = item
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                return Some(Delta::ToolCall { name, input });
            }
            Some("tool_result") => {
                let tool_use_id = item
                    .get("tool_use_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let content = item
                    .get("content")
                    .map_or_else(String::new, stringify_content);
                return Some(Delta::ToolResult {
                    tool_use_id,
                    content,
                });
            }
            _ => {}
        }
    }

    None
}

fn parse_usage(usage: &serde_json::Value) -> Delta {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    Delta::Usage {
        input_tokens,
        output_tokens,
    }
}

fn stringify_content(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Some(item.to_string()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    // Shared process-wide lock so env-mutating tests serialise across modules.
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    #[test]
    fn detect_returns_none_when_no_binary_and_no_key() {
        // Assumes `claude` is not on $PATH in CI. If it is, this picks Cli instead.
        let provider = ClaudeCliProvider::detect(None);
        if SubprocessProvider::available("claude") {
            assert!(matches!(provider, Some(ClaudeCliProvider::Cli)));
        } else {
            assert!(provider.is_none());
        }
    }

    #[test]
    fn detect_returns_api_when_no_binary_but_key_present() {
        if SubprocessProvider::available("claude") {
            // CLI wins over API key; skip this case.
            return;
        }
        let provider = ClaudeCliProvider::detect(Some("test-key".into()));
        assert!(matches!(provider, Some(ClaudeCliProvider::Api(_))));
    }

    #[test]
    fn parse_line_extracts_session_id_from_init() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc123"}"#;
        assert_eq!(parse_line(line), Some(Delta::SessionId("abc123".into())));
    }

    #[test]
    fn parse_line_extracts_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#;
        assert_eq!(parse_line(line), Some(Delta::Text("hello".into())));
    }

    #[test]
    fn parse_line_extracts_tool_call() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::ToolCall {
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            })
        );
    }

    #[test]
    fn parse_line_extracts_tool_result() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"u1","content":"done"}]}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::ToolResult {
                tool_use_id: "u1".into(),
                content: "done".into(),
            })
        );
    }

    #[test]
    fn parse_line_extracts_usage() {
        let line = r#"{"type":"result","usage":{"input_tokens":7,"output_tokens":11}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::Usage {
                input_tokens: 7,
                output_tokens: 11,
            })
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn cli_provider_streams_mock_claude_and_passes_resume() {
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
        let script_path = temp_dir.join("claude");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"mock-session\"}}'\nprintf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"hello\"}}]}}}}'\n",
                args_file.display()
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
            stable_prefix_len: None,
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
        assert!(args.contains("--resume"));
        assert!(args.contains("resume-123"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
