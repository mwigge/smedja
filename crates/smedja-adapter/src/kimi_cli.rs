//! Kimi CLI provider for the `kimi` binary (Moonshot AI's Kimi Code CLI).
//!
//! Mirrors the [`crate::ClaudeCliProvider`] split: the `kimi` binary serves
//! subscription users (device-code OAuth via `kimi login`), while
//! [`crate::KimiProvider`] serves API-key users over the OpenAI-compatible
//! Moonshot endpoint. Prompt mode is `kimi -p <prompt> --output-format
//! stream-json`, which emits OpenAI-message-shaped JSONL events:
//!
//! ```text
//! {"role":"assistant","content":"..."}
//! {"role":"assistant","tool_calls":[{"type":"function","id":"...","function":{"name":"Bash","arguments":"{...}"}}]}
//! {"role":"tool","tool_call_id":"...","content":"..."}
//! {"role":"meta","type":"session.resume_hint","session_id":"session_..."}
//! ```
//!
//! Two divergences from the claude adapter, both forced by the kimi CLI
//! surface (verified against kimi-code 0.27.0):
//!
//! * The prompt travels as a `-p` argv element — the CLI has no stdin prompt
//!   path (`-p -` is taken literally) and no system-prompt flag, so the
//!   assembled system block is prepended to the rendered prompt. Prompts
//!   approaching `MAX_ARG_STRLEN` (128 KiB) will fail `execve` with `E2BIG`.
//! * Prompt mode auto-approves kimi's own tool calls (`--prompt` rejects
//!   `--yolo`/`--auto` because it already implies them) and exposes no hook
//!   mechanism, so smedja's PreToolUse approval gate cannot be installed.
//!   The trust model is therefore "same as running `kimi -p` by hand".

use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AcpProvider, AdapterError, CallOptions, Delta, DeltaStream, KimiProvider, Message,
    OpenAiCompatProvider, Provider, Role, SubprocessProvider, KIMI_ACP,
};

/// Runs the `kimi` CLI binary if available; falls back to the OpenAI-compatible
/// Moonshot API via [`KimiProvider`].
///
/// The CLI path drives `kimi acp` by default — the Agent Client Protocol
/// surfaces the agent's `session/request_permission` so its tool calls are
/// gated through smedja's approval flow. Set `SMEDJA_KIMI_ACP=off` to revert
/// to the ungated one-shot `kimi -p … --output-format stream-json` path.
pub enum KimiCliProvider {
    /// Drives the locally installed `kimi` binary as an ACP agent (gated).
    Acp(AcpProvider),
    /// Uses the legacy one-shot prompt mode (kimi self-approves its tools).
    Prompt,
    /// Delegates to the Moonshot HTTP API using an API key.
    Api(OpenAiCompatProvider),
}

/// True when `SMEDJA_KIMI_ACP` opts out of the ACP path.
fn acp_disabled() -> bool {
    std::env::var("SMEDJA_KIMI_ACP").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "none" | "disable" | "disabled"
        )
    })
}

impl KimiCliProvider {
    /// Selects the CLI (ACP by default) if the `kimi` binary is on `$PATH`,
    /// otherwise uses the environment API key (`MOONSHOT_API_KEY` /
    /// `KIMI_API_KEY`).
    ///
    /// Returns `None` if neither is available.
    #[must_use]
    pub fn detect() -> Option<Self> {
        if SubprocessProvider::available("kimi") {
            if acp_disabled() {
                tracing::warn!(
                    "SMEDJA_KIMI_ACP=off: kimi runs in one-shot prompt mode, which \
                     auto-approves its own tool calls WITHOUT the smedja gate"
                );
                Some(Self::Prompt)
            } else {
                Some(Self::Acp(AcpProvider::new(KIMI_ACP)))
            }
        } else {
            KimiProvider::detect().map(Self::Api)
        }
    }
}

impl Provider for KimiCliProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        match self {
            Self::Acp(p) => p.stream_chat(messages, opts),
            Self::Prompt => stream_kimi_cli(messages, opts),
            Self::Api(p) => p.stream_chat(messages, opts),
        }
    }
}

fn stream_kimi_cli(messages: &[Message], opts: &CallOptions) -> DeltaStream {
    // Render the FULL conversation (system block included — kimi has no
    // system-prompt flag) into the prompt. We do NOT use `-S/--session`
    // resume: like the claude adapter, the whole prompt is assembled
    // upstream so nothing depends on the CLI's own conversation store.
    let prompt = render_conversation(messages, opts.system.as_deref());
    let model = opts.model.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::spawn(async move {
        let mut command = tokio::process::Command::new("kimi");
        command
            .arg("-p")
            .arg(&prompt)
            .arg("--output-format")
            .arg("stream-json")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // So an interrupted turn (turn.cancel aborts the run task) kills
            // the child instead of leaking a runaway `kimi` process.
            .kill_on_drop(true);

        if !model.is_empty() {
            command.arg("-m").arg(&model);
        }

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
                        // Non-JSON lines (kimi's own tool output leaks to
                        // stdout in prompt mode) parse to None and are skipped.
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

/// Renders the conversation into a single prompt for `kimi -p`.
///
/// The system block (when present) leads the prompt because the kimi CLI has
/// no out-of-band system-prompt channel. A lone user turn is otherwise sent
/// verbatim; multi-turn histories become a labelled `Human:` / `Assistant:`
/// transcript so the CLI has the full context in one shot.
fn render_conversation(messages: &[Message], system: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(system) = system {
        if !system.trim().is_empty() {
            out.push_str("System: ");
            out.push_str(system);
            out.push_str("\n\n");
        }
    }
    let dialogue: Vec<&Message> = messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .collect();
    match dialogue.as_slice() {
        [] => {
            if let Some(m) = messages.last() {
                out.push_str(&m.content);
            }
        }
        [single] => out.push_str(&single.content),
        many => {
            for m in many {
                let label = match m.role {
                    Role::Assistant => "Assistant",
                    _ => "Human",
                };
                out.push_str(label);
                out.push_str(": ");
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
        }
    }
    out
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

/// Parses one JSONL event from kimi's `--output-format stream-json` output.
fn parse_line(line: &str) -> Option<Delta> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    match value.get("role").and_then(serde_json::Value::as_str)? {
        "assistant" => {
            if let Some(calls) = value
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
            {
                let call = calls.first()?;
                let function = call.get("function")?;
                let name = function
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                // `arguments` is a JSON-encoded string per the OpenAI wire
                // shape; fall back to the raw string when it isn't valid JSON.
                let input = function
                    .get("arguments")
                    .and_then(serde_json::Value::as_str)
                    .map_or(serde_json::Value::Null, |raw| {
                        serde_json::from_str(raw)
                            .unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
                    });
                return Some(Delta::ToolCall { name, input });
            }
            value
                .get("content")
                .and_then(serde_json::Value::as_str)
                .map(|text| Delta::Text(text.to_owned()))
        }
        "tool" => {
            let tool_use_id = value
                .get("tool_call_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let content = value
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some(Delta::ToolResult {
                tool_use_id,
                content,
            })
        }
        "meta" => value
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(|id| Delta::SessionId(id.to_owned())),
        _ => None,
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
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_moonshot = std::env::var("MOONSHOT_API_KEY").ok();
        let saved_kimi = std::env::var("KIMI_API_KEY").ok();
        std::env::remove_var("MOONSHOT_API_KEY");
        std::env::remove_var("KIMI_API_KEY");
        let has_cli = SubprocessProvider::available("kimi");
        let provider = KimiCliProvider::detect();
        if let Some(v) = saved_moonshot {
            std::env::set_var("MOONSHOT_API_KEY", v);
        }
        if let Some(v) = saved_kimi {
            std::env::set_var("KIMI_API_KEY", v);
        }
        if has_cli {
            // ACP is the default CLI path (gated tool calls).
            assert!(matches!(provider, Some(KimiCliProvider::Acp(_))));
        } else {
            assert!(provider.is_none());
        }
    }

    #[test]
    fn detect_honours_acp_opt_out() {
        let _guard = ENV_LOCK.lock().unwrap();
        if !SubprocessProvider::available("kimi") {
            return;
        }
        std::env::set_var("SMEDJA_KIMI_ACP", "off");
        let provider = KimiCliProvider::detect();
        std::env::remove_var("SMEDJA_KIMI_ACP");
        assert!(matches!(provider, Some(KimiCliProvider::Prompt)));
    }

    #[test]
    fn detect_returns_api_when_no_binary_but_key_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        if SubprocessProvider::available("kimi") {
            // CLI wins over API key; skip this case.
            return;
        }
        std::env::set_var("MOONSHOT_API_KEY", "test-key");
        let provider = KimiCliProvider::detect();
        std::env::remove_var("MOONSHOT_API_KEY");
        assert!(matches!(provider, Some(KimiCliProvider::Api(_))));
    }

    #[test]
    fn parse_line_extracts_text() {
        let line = r#"{"role":"assistant","content":"hello"}"#;
        assert_eq!(parse_line(line), Some(Delta::Text("hello".into())));
    }

    #[test]
    fn parse_line_extracts_tool_call_with_decoded_arguments() {
        let line = r#"{"role":"assistant","tool_calls":[{"type":"function","id":"tool_1","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]}"#;
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
        let line = r#"{"role":"tool","tool_call_id":"tool_1","content":"done\n"}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::ToolResult {
                tool_use_id: "tool_1".into(),
                content: "done\n".into(),
            })
        );
    }

    #[test]
    fn parse_line_extracts_session_id_from_resume_hint() {
        let line = r#"{"role":"meta","type":"session.resume_hint","session_id":"session_abc","command":"kimi -r session_abc","content":"To resume: kimi -r session_abc"}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::SessionId("session_abc".into()))
        );
    }

    #[test]
    fn parse_line_skips_non_json_tool_output_leaks() {
        // kimi's own tool output leaks to stdout in prompt mode.
        assert_eq!(parse_line("forge-test-123"), None);
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn render_conversation_single_user_turn_is_verbatim() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hello there".into(),
        }];
        assert_eq!(render_conversation(&msgs, None), "hello there");
    }

    #[test]
    fn render_conversation_prepends_system_block() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hi".into(),
        }];
        assert_eq!(
            render_conversation(&msgs, Some("be terse")),
            "System: be terse\n\nhi"
        );
    }

    #[test]
    fn render_conversation_multi_turn_is_labelled_transcript() {
        let msgs = vec![
            Message {
                role: Role::User,
                content: "my number is 7".into(),
            },
            Message {
                role: Role::Assistant,
                content: "noted".into(),
            },
            Message {
                role: Role::User,
                content: "what number?".into(),
            },
        ];
        assert_eq!(
            render_conversation(&msgs, None),
            "Human: my number is 7\n\nAssistant: noted\n\nHuman: what number?\n\n"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn cli_provider_streams_mock_kimi_via_prompt_argv() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!(
            "smedja-kimi-mock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let args_file = temp_dir.join("args.txt");
        let script_path = temp_dir.join("kimi");
        // Record argv, then emit a minimal stream-json transcript in kimi's
        // observed event shapes (plus a leaked non-JSON tool-output line).
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' 'leaked-tool-output'\nprintf '%s\\n' '{{\"role\":\"assistant\",\"content\":\"hello\"}}'\nprintf '%s\\n' '{{\"role\":\"meta\",\"type\":\"session.resume_hint\",\"session_id\":\"mock-session\"}}'\n",
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

        // Exercise the legacy prompt path directly — ACP is detection's
        // default, but the one-shot mode remains the SMEDJA_KIMI_ACP=off
        // fallback and must keep working.
        let provider = KimiCliProvider::Prompt;
        let opts = CallOptions {
            model: "kimi-mock".into(),
            max_tokens: None,
            temperature: None,
            system: Some("be terse".into()),
            tools: None,
            provider_session_id: Some("resume-123".into()),
            smedja_session_id: None,
            permission_mode: None,
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
            workspace: None,
            tool_gate: None,
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
        // The leaked non-JSON line must not surface as a delta.
        assert!(!deltas.contains(&Delta::Text("leaked-tool-output".into())));
        let args = std::fs::read_to_string(&args_file).unwrap();
        // The prompt travels as the `-p` argv value (kimi has no stdin path),
        // with the system block prepended (no system-prompt flag either).
        assert!(
            args.contains("System: be terse"),
            "system block must be prepended to the -p prompt; args were:\n{args}"
        );
        assert!(
            args.contains("--output-format") && args.contains("stream-json"),
            "stream-json output must be requested; args were:\n{args}"
        );
        assert!(
            args.contains("kimi-mock"),
            "model must be passed via -m; args were:\n{args}"
        );
        // `--prompt` rejects `--yolo`/`--auto` (prompt mode already implies
        // auto-approval), and `-S/--session` resume must never be passed.
        for forbidden in ["--yolo", "--auto", "--session", "resume-123"] {
            assert!(
                !args.contains(forbidden),
                "{forbidden} must not be passed; args were:\n{args}"
            );
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
