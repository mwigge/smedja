//! Codex CLI provider — uses `codex exec` for non-interactive turns.

mod parse;

use std::process::Stdio;

use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AdapterError, CallOptions, Delta, DeltaStream, Message, OpenAiProvider, Provider,
    SubprocessProvider,
};

use parse::parse_codex_line;

/// Runs `codex exec` if the binary is on `$PATH`; falls back to [`OpenAiProvider`].
pub enum CodexCliProvider {
    /// Uses the locally installed `codex` CLI binary.
    Cli,
    /// Delegates to the `OpenAI` HTTP API using an API key.
    Api(OpenAiProvider),
}

impl CodexCliProvider {
    /// Selects CLI if the `codex` binary is on `$PATH`, otherwise uses the API key.
    ///
    /// Returns `None` if neither is available.
    #[must_use]
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

#[allow(clippy::too_many_lines)]
fn stream_codex_exec(messages: &[Message], opts: &CallOptions) -> DeltaStream {
    let prompt = messages
        .last()
        .map_or_else(String::new, |m| m.content.clone());
    let resume_id = opts.provider_session_id.clone();
    let model = opts.model.clone();
    let workspace = opts.workspace.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::spawn(async move {
        // Emit a synthetic session marker immediately so the daemon uses --last on the
        // next turn, even if the stream is interrupted before stdout closes.
        let _ = tx.send(Ok(Delta::SessionId("last".to_owned()))).await;

        let mut command = tokio::process::Command::new("codex");

        // Route to `codex exec resume` when a prior session exists.
        let is_resume = resume_id.as_deref().is_some_and(|s| !s.is_empty());
        if is_resume {
            command.arg("exec").arg("resume");
            if resume_id.as_deref() == Some("last") {
                command.arg("--last");
            } else if let Some(id) = resume_id.as_deref() {
                command.arg(id);
            }
        } else {
            command.arg("exec");
        }

        // `codex exec` runs autonomously — it has no per-tool approval hook like
        // claude, so smedja's permission mode maps to codex's sandbox level
        // instead. Auto keeps the full bypass; Plan makes codex read-only; every
        // other mode contains it to the workspace.
        //
        // `codex exec resume` does not accept `--sandbox`; only the bypass flag
        // is shared between the two sub-commands.
        command.arg("--json").arg("--skip-git-repo-check");
        // Bypass codex's own bwrap sandbox entirely. The smedja cowork gate
        // (smj tool-gate / PreToolUse hook on the claude side) and landlock
        // filesystem confinement are the actual approval boundary. Codex's
        // `--sandbox workspace-write` invokes bwrap which fails with EAFNOSUPPORT
        // when AF_NETLINK is blocked by an outer seccomp filter — the same
        // condition that prevents `unshare --net`. Bypass is safe here because
        // smdjad itself provides the workspace boundary.
        command.arg("--dangerously-bypass-approvals-and-sandbox");

        if !model.is_empty() {
            command.arg("-m").arg(&model);
        }

        command
            .arg(&prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(ref dir) = workspace {
            command.current_dir(dir);
        }

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
        let mut had_output = false;
        if let Some(stdout) = child.stdout.take() {
            use tokio::io::AsyncBufReadExt as _;
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(delta) = parse_codex_line(&line) {
                            had_output = true;
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
            Ok(status) if status.success() => {
                // codex exited cleanly but produced no output — surface stderr
                // so the user knows something went wrong (auth failure, unsupported
                // model, network error, etc.) rather than seeing a silent idle.
                if !had_output {
                    let stderr_text = read_stderr(stderr).await;
                    let detail = if stderr_text.trim().is_empty() {
                        "codex returned no output (check auth and model name)".to_owned()
                    } else {
                        format!("codex returned no output: {}", stderr_text.trim())
                    };
                    let _ = tx.send(Err(AdapterError::Request(detail))).await;
                }
            }
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
    use futures_util::StreamExt as _;

    use crate::Role;
    // Shared process-wide lock so env-mutating tests serialise across modules.
    use crate::TEST_ENV_LOCK as ENV_LOCK;

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
            smedja_session_id: None,
            permission_mode: None,
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
            workspace: None,
        }
    }

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: text.to_owned(),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn mock_codex_streams_plain_text() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("smedja-codex-mock-{}", std::process::id()));
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
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn session_id_emitted_at_stream_start() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp =
            std::env::temp_dir().join(format!("smedja-codex-sessionid-{}", std::process::id()));
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
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn resume_passes_last_flag() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("smedja-codex-resume-{}", std::process::id()));
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

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn model_flag_forwarded_to_command() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("smedja-codex-model-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        make_mock_codex(&tmp, "#!/bin/sh\nprintf \"args: $*\\n\"\n");

        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", tmp.display()));

        let mut opts = base_opts(None);
        opts.model = "o3-mini".to_owned();
        let provider = CodexCliProvider::Cli;
        let mut stream = provider.stream_chat(&[user_msg("hi")], &opts);
        let mut output = String::new();
        while let Some(item) = stream.next().await {
            if let Ok(Delta::Text(t)) = item {
                output.push_str(&t);
            }
        }

        std::env::set_var("PATH", old);
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            output.contains("-m") && output.contains("o3-mini"),
            "expected '-m o3-mini' in args; got: {output:?}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn empty_output_surfaces_error_not_silent_idle() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("smedja-codex-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Exits 0 but produces no stdout — simulates auth failure / unsupported model.
        make_mock_codex(&tmp, "#!/bin/sh\nexit 0\n");

        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", tmp.display()));

        let provider = CodexCliProvider::Cli;
        let mut stream = provider.stream_chat(&[user_msg("hi")], &base_opts(None));
        let mut got_error = false;
        while let Some(item) = stream.next().await {
            if item.is_err() {
                got_error = true;
            }
        }

        std::env::set_var("PATH", old);
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            got_error,
            "expected an error delta when codex exits 0 with no output"
        );
    }
}
