//! Codex CLI provider — uses `codex exec` for non-interactive turns.

use std::process::Stdio;

use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AdapterError, CallOptions, Delta, DeltaStream, Message, OpenAiProvider, Provider,
    SubprocessProvider,
};

/// Runs `codex exec` if the binary is on `$PATH`; falls back to [`OpenAiProvider`].
pub enum CodexCliProvider {
    /// Uses the locally installed `codex` CLI binary.
    Cli,
    /// Delegates to the OpenAI HTTP API using an API key.
    Api(OpenAiProvider),
}

impl CodexCliProvider {
    /// Selects CLI if the `codex` binary is on `$PATH`, otherwise uses the API key.
    ///
    /// Returns `None` if neither is available.
    pub fn detect(api_key: Option<String>) -> Option<Self> {
        if SubprocessProvider::available("codex") {
            Some(Self::Cli)
        } else {
            api_key.map(|key| Self::Api(OpenAiProvider::new("https://api.openai.com", key)))
        }
    }
}

impl Provider for CodexCliProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        match self {
            Self::Cli => stream_codex_exec(messages, opts),
            Self::Api(p) => p.stream_chat(messages, opts),
        }
    }
}

fn stream_codex_exec(messages: &[Message], opts: &CallOptions) -> DeltaStream {
    let prompt = messages.last().map_or_else(String::new, |m| m.content.clone());
    let resume_id = opts.provider_session_id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::spawn(async move {
        // Emit a synthetic session marker immediately so the daemon uses --last on the
        // next turn, even if the stream is interrupted before stdout closes.
        let _ = tx.send(Ok(Delta::SessionId("last".to_owned()))).await;

        let mut command = tokio::process::Command::new("codex");

        // Route to `codex exec resume` when a prior session exists.
        if let Some(id) = resume_id.as_deref().filter(|s| !s.is_empty()) {
            command.arg("exec").arg("resume");
            if id == "last" {
                command.arg("--last");
            } else {
                command.arg(id);
            }
        } else {
            command.arg("exec");
        }

        command
            .arg("--json")
            .arg("--dangerously-bypass-approvals-and-sandbox")
            .arg(&prompt)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(Err(AdapterError::Request(format!(
                        "codex exec spawn failed: {e}"
                    ))))
                    .await;
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
                        if let Some(delta) = parse_codex_line(&line) {
                            if tx.send(Ok(delta)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx
                            .send(Err(AdapterError::Request(e.to_string())))
                            .await;
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
                let _ = tx
                    .send(Err(AdapterError::Request(detail)))
                    .await;
            }
            Err(e) => {
                let _ = tx
                    .send(Err(AdapterError::Request(e.to_string())))
                    .await;
            }
        }
    });

    Box::pin(ReceiverStream::new(rx))
}

async fn read_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut buf = String::new();
    use tokio::io::AsyncReadExt as _;
    let _ = stderr.read_to_string(&mut buf).await;
    buf
}

/// Parses a single JSONL line from `codex exec --json`.
///
/// Tries multiple known OpenAI Responses-API event shapes before falling back
/// to treating a non-empty, non-JSON line as plain text. Returns `None` for
/// blank lines and unrecognised JSON objects.
fn parse_codex_line(line: &str) -> Option<Delta> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // Non-JSON line — treat as plain text with a trailing newline.
        return Some(Delta::Text(format!("{trimmed}\n")));
    };

    // Pattern 1: {"delta":"text"} — response.output_text.delta event
    if let Some(text) = v.get("delta").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return Some(Delta::Text(text.to_owned()));
        }
    }

    // Pattern 2: OpenAI streaming {"choices":[{"delta":{"content":"text"}}]}
    if let Some(content) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(serde_json::Value::as_str)
    {
        if !content.is_empty() {
            return Some(Delta::Text(content.to_owned()));
        }
    }

    // Pattern 3: {"text":"..."} or {"content":"..."}
    if let Some(text) = v
        .get("text")
        .and_then(serde_json::Value::as_str)
        .or_else(|| v.get("content").and_then(serde_json::Value::as_str))
    {
        if !text.is_empty() {
            return Some(Delta::Text(text.to_owned()));
        }
    }

    // Unrecognised JSON shape — skip.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;
    use std::sync::Mutex;

    use crate::Role;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // --- detect ---

    #[test]
    fn detect_prefers_cli_when_binary_present() {
        if SubprocessProvider::available("codex") {
            assert!(matches!(
                CodexCliProvider::detect(None),
                Some(CodexCliProvider::Cli)
            ));
        }
    }

    #[test]
    fn detect_returns_api_when_no_binary_but_key_present() {
        if SubprocessProvider::available("codex") {
            return;
        }
        let provider = CodexCliProvider::detect(Some("test-key".into()));
        assert!(matches!(provider, Some(CodexCliProvider::Api(_))));
    }

    #[test]
    fn detect_returns_none_when_no_binary_and_no_key() {
        if SubprocessProvider::available("codex") {
            return;
        }
        assert!(CodexCliProvider::detect(None).is_none());
    }

    // --- parse_codex_line ---

    #[test]
    fn parse_codex_line_empty_returns_none() {
        assert!(parse_codex_line("").is_none());
        assert!(parse_codex_line("   ").is_none());
    }

    #[test]
    fn parse_codex_line_plain_text_returns_text_with_newline() {
        let delta = parse_codex_line("hello world");
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "hello world\n"));
    }

    #[test]
    fn parse_codex_line_delta_field() {
        let delta = parse_codex_line(r#"{"type":"response.output_text.delta","delta":"hi"}"#);
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "hi"));
    }

    #[test]
    fn parse_codex_line_openai_streaming() {
        let delta = parse_codex_line(
            r#"{"choices":[{"delta":{"content":"streamed text"}}]}"#,
        );
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "streamed text"));
    }

    #[test]
    fn parse_codex_line_text_field() {
        let delta = parse_codex_line(r#"{"type":"message","text":"answer"}"#);
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "answer"));
    }

    #[test]
    fn parse_codex_line_content_field() {
        let delta = parse_codex_line(r#"{"content":"response"}"#);
        assert!(matches!(delta, Some(Delta::Text(t)) if t == "response"));
    }

    #[test]
    fn parse_codex_line_unrecognised_json_returns_none() {
        assert!(parse_codex_line(r#"{"foo":"bar"}"#).is_none());
    }

    // --- mock binary integration tests ---

    fn make_mock_codex(dir: &std::path::Path, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = dir.join("codex");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn base_opts(session_id: Option<&str>) -> CallOptions {
        CallOptions {
            model: String::new(),
            max_tokens: None,
            temperature: None,
            system: None,
            tools: None,
            provider_session_id: session_id.map(str::to_owned),
            stable_prefix_len: None,
        }
    }

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: text.to_owned(),
        }
    }

    #[tokio::test]
    async fn mock_codex_streams_plain_text() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "smedja-codex-mock-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        make_mock_codex(&tmp, "#!/bin/sh\nprintf 'line one\\nline two\\n'\n");

        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", tmp.display()));

        let provider = CodexCliProvider::Cli;
        let mut stream = provider.stream_chat(&[user_msg("hi")], &base_opts(None));
        let mut texts = Vec::new();
        while let Some(item) = stream.next().await {
            if let Ok(Delta::Text(t)) = item {
                texts.push(t);
            }
        }

        std::env::set_var("PATH", old);
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            texts.iter().any(|t| t.contains("line one")),
            "expected plain text in stream; got {texts:?}"
        );
    }

    #[tokio::test]
    async fn session_id_emitted_at_stream_start() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "smedja-codex-sessionid-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        make_mock_codex(&tmp, "#!/bin/sh\nprintf 'hello\\n'\n");

        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", tmp.display()));

        let provider = CodexCliProvider::Cli;
        let mut stream = provider.stream_chat(&[user_msg("hi")], &base_opts(None));
        let first = stream.next().await;

        std::env::set_var("PATH", old);
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            matches!(first, Some(Ok(Delta::SessionId(ref id))) if id == "last"),
            "SessionId(\"last\") must be the first item; got: {first:?}"
        );
    }

    #[tokio::test]
    async fn resume_passes_last_flag() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "smedja-codex-resume-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        // Echo argv so we can verify --last appeared.
        make_mock_codex(&tmp, "#!/bin/sh\nprintf \"args: $*\\n\"\n");

        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", tmp.display()));

        let provider = CodexCliProvider::Cli;
        let mut stream = provider.stream_chat(&[user_msg("continue")], &base_opts(Some("last")));
        let mut output = String::new();
        while let Some(item) = stream.next().await {
            if let Ok(Delta::Text(t)) = item {
                output.push_str(&t);
            }
        }

        std::env::set_var("PATH", old);
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            output.contains("resume") && output.contains("--last"),
            "expected 'resume --last' in args; got: {output:?}"
        );
    }
}
