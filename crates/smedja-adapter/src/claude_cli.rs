//! Claude CLI provider for the `claude` binary.

use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AdapterError, AnthropicProvider, CallOptions, Delta, DeltaStream, Message, Provider, Role,
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

/// POSIX single-quotes a string for safe embedding in a shell command (claude
/// runs the hook command through a shell).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Removes a temp file on drop so per-invocation settings / system-prompt files
/// don't accumulate. Held by the spawned task until the child exits, so claude
/// has finished reading the file before it's unlinked.
struct TempFileGuard(std::path::PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A unique temp path so concurrent claude invocations never share a file.
fn unique_temp_path(stem: &str, ext: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("{stem}-{}-{nanos}.{ext}", std::process::id()))
}

/// True when `SMEDJA_TOOL_GATE` is set to an off-like value (the intentional
/// escape hatch that runs claude with no approval gate).
fn tool_gate_disabled() -> bool {
    std::env::var("SMEDJA_TOOL_GATE").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "none" | "disable" | "disabled"
        )
    })
}

/// True when `SMEDJA_TOOL_GATE_FALLBACK=open` — reverts the *expected-but-
/// unavailable* gate case to the old fail-OPEN behaviour (claude runs unguarded).
/// Default is fail-CLOSED.
fn tool_gate_fallback_open() -> bool {
    std::env::var("SMEDJA_TOOL_GATE_FALLBACK").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "open" | "1" | "true" | "allow"
        )
    })
}

/// What the `PreToolUse` gate hook should do for a claude invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatePlan {
    /// Install the real interactive `smj tool-gate` hook.
    Interactive,
    /// A gate was expected but `smj`/session is missing: install a deny-all hook
    /// (fail closed) so claude cannot run tools unguarded.
    DenyAll,
    /// No hook — the intentional escape hatch, an unattributable/Auto run, or an
    /// explicit fail-open override.
    Open,
}

/// Decides the gate plan from the resolved inputs. Pure so the fail-closed
/// contract is unit-testable.
///
/// A gate is *expected* whenever the permission mode is not `auto` (in `auto`
/// every tool is allowed anyway, so a missing gate is not a hole). When a gate
/// is expected but cannot be installed (`smj` not on `$PATH` or no session id to
/// attribute approvals to), the plan is `DenyAll` — unless the caller opted back
/// into fail-open via `SMEDJA_TOOL_GATE_FALLBACK=open`.
fn plan_tool_gate(
    disabled: bool,
    fallback_open: bool,
    has_session: bool,
    mode: Option<&str>,
    smj_available: bool,
) -> GatePlan {
    if disabled {
        return GatePlan::Open;
    }
    if has_session && smj_available {
        return GatePlan::Interactive;
    }
    let is_auto = mode.is_some_and(|m| m.eq_ignore_ascii_case("auto"));
    if is_auto || fallback_open {
        GatePlan::Open
    } else {
        GatePlan::DenyAll
    }
}

/// Writes a claude `--settings` file carrying a single `PreToolUse` hook and
/// attaches it to `command`. Returns a guard that unlinks the file on drop.
fn attach_settings(
    command: &mut tokio::process::Command,
    hook_command: &str,
) -> Option<TempFileGuard> {
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": hook_command,
                    "timeout": 1800,
                }],
            }],
        }
    });
    let path = unique_temp_path("smedja-claude-settings", "json");
    if std::fs::write(&path, settings.to_string()).is_err() {
        tracing::warn!("failed to write claude --settings file; tool gate not installed");
        return None;
    }
    command.arg("--settings").arg(&path);
    Some(TempFileGuard(path))
}

/// Installs smedja's `PreToolUse` approval hook on a `claude` command so each of
/// claude's own tool calls is gated through the daemon's permission policy
/// (`smj tool-gate` → `cowork.gate_tool`, which blocks on the user when the
/// policy says "ask"). The smedja session id is passed via `SMEDJA_SESSION_ID`
/// so the hook knows which session's gate to consult.
///
/// `SMEDJA_TOOL_GATE=off` disables the gate entirely (logged escape hatch). When
/// a gate is *expected* (mode is not `auto`) but `smj`/session is missing, a
/// deny-all hook is installed instead of silently running unguarded — override
/// with `SMEDJA_TOOL_GATE_FALLBACK=open`. Returns a guard for the settings file.
fn install_tool_gate(
    command: &mut tokio::process::Command,
    session_id: Option<&str>,
    permission_mode: Option<&str>,
) -> Option<TempFileGuard> {
    let disabled = tool_gate_disabled();
    let has_session = session_id.is_some_and(|s| !s.is_empty());
    let smj = which::which("smj").ok();
    match plan_tool_gate(
        disabled,
        tool_gate_fallback_open(),
        has_session,
        permission_mode,
        smj.is_some(),
    ) {
        GatePlan::Open => {
            if disabled {
                tracing::warn!(
                    "SMEDJA_TOOL_GATE disabled: claude tool calls run WITHOUT the smedja approval gate"
                );
            }
            None
        }
        GatePlan::DenyAll => {
            tracing::error!(
                has_session,
                smj_on_path = smj.is_some(),
                mode = permission_mode.unwrap_or("ask"),
                "smedja approval gate expected but unavailable (smj or session missing); \
                 installing a fail-CLOSED deny-all PreToolUse hook. Set \
                 SMEDJA_TOOL_GATE_FALLBACK=open to run claude unguarded instead."
            );
            // A self-contained hook that emits a PreToolUse deny for every tool.
            const DENY_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"smedja approval gate unavailable (smj/session missing); denied fail-closed. Set SMEDJA_TOOL_GATE_FALLBACK=open to override."}}"#;
            let hook_command = format!("printf '%s' {}", shell_quote(DENY_JSON));
            attach_settings(command, &hook_command)
        }
        GatePlan::Interactive => {
            // Bake the absolute smj path into the hook so it resolves regardless
            // of claude's own PATH.
            let smj = smj?;
            let hook_command = format!("{} tool-gate", shell_quote(&smj.to_string_lossy()));
            let guard = attach_settings(command, &hook_command);
            if guard.is_some() {
                if let Some(sid) = session_id {
                    command.env("SMEDJA_SESSION_ID", sid);
                }
            }
            guard
        }
    }
}

/// Writes the assembled system block to a temp file and appends it to claude's
/// default system prompt via `--append-system-prompt-file`. Delivered as a file
/// (not `--append-system-prompt <arg>`) because the block — role packs,
/// workspace/role skills, methodology, project context — can exceed
/// `MAX_ARG_STRLEN` (128 KiB) and overflow `execve` with `E2BIG`.
///
/// Returns a guard that unlinks the file on drop, or `None` when there is no
/// system block to deliver.
fn install_system_prompt(
    command: &mut tokio::process::Command,
    system: Option<&str>,
) -> Option<TempFileGuard> {
    let system = system?;
    if system.trim().is_empty() {
        return None;
    }
    let path = unique_temp_path("smedja-claude-system", "txt");
    if std::fs::write(&path, system).is_err() {
        tracing::warn!("failed to write claude system-prompt file; system block not delivered");
        return None;
    }
    command.arg("--append-system-prompt-file").arg(&path);
    Some(TempFileGuard(path))
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
    let permission_mode = opts.permission_mode.clone();
    let system = opts.system.clone();
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
        // gated through smedja's permission policy (ask → approve/deny). Deliver
        // the assembled system block via --append-system-prompt-file. Both guards
        // are held until the child exits so the temp files outlive claude reading
        // them, then are unlinked on drop.
        let _gate_guard = install_tool_gate(
            &mut command,
            session_id.as_deref(),
            permission_mode.as_deref(),
        );
        let _system_guard = install_system_prompt(&mut command, system.as_deref());

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

/// Renders the conversation into a single prompt for `claude --print`.
///
/// A lone user turn is sent verbatim (the common single-turn case). Multi-turn
/// histories become a labelled `Human:` / `Assistant:` transcript so the CLI
/// has the full context in one shot — no dependency on its resume store.
/// System messages are delivered out of band and excluded here.
fn render_conversation(messages: &[Message]) -> String {
    let dialogue: Vec<&Message> = messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .collect();
    match dialogue.as_slice() {
        [] => messages
            .last()
            .map_or_else(String::new, |m| m.content.clone()),
        [single] => single.content.clone(),
        many => {
            let mut out = String::new();
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
            out
        }
    }
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
    // The Claude CLI surfaces cache reads as `cache_read_input_tokens`.
    let cache_read_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    Delta::Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
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
                cache_read_tokens: 0,
            })
        );
    }

    #[test]
    fn parse_line_extracts_cache_read_tokens() {
        let line = r#"{"type":"result","usage":{"input_tokens":7,"output_tokens":11,"cache_read_input_tokens":900}}"#;
        assert_eq!(
            parse_line(line),
            Some(Delta::Usage {
                input_tokens: 7,
                output_tokens: 11,
                cache_read_tokens: 900,
            })
        );
    }

    #[test]
    fn render_conversation_single_user_turn_is_verbatim() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hello there".into(),
        }];
        assert_eq!(render_conversation(&msgs), "hello there");
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
        let rendered = render_conversation(&msgs);
        assert_eq!(
            rendered,
            "Human: my number is 7\n\nAssistant: noted\n\nHuman: what number?\n\n"
        );
    }

    #[test]
    fn render_conversation_excludes_system_messages() {
        let msgs = vec![
            Message {
                role: Role::System,
                content: "be terse".into(),
            },
            Message {
                role: Role::User,
                content: "hi".into(),
            },
        ];
        // System filtered → single dialogue turn → verbatim.
        assert_eq!(render_conversation(&msgs), "hi");
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

    // ── gate plan (fail-closed contract) ─────────────────────────────────────

    #[test]
    fn plan_open_when_disabled() {
        // The escape hatch always fails open, regardless of session/smj/mode.
        assert_eq!(
            plan_tool_gate(true, false, true, Some("ask"), true),
            GatePlan::Open
        );
        assert_eq!(
            plan_tool_gate(true, false, false, Some("ask"), false),
            GatePlan::Open
        );
    }

    #[test]
    fn plan_interactive_when_session_and_smj_present() {
        for mode in [Some("ask"), Some("accept_edits"), Some("plan"), None] {
            assert_eq!(
                plan_tool_gate(false, false, true, mode, true),
                GatePlan::Interactive,
                "mode {mode:?} with session+smj must install the real gate"
            );
        }
    }

    #[test]
    fn plan_deny_all_when_gate_expected_but_smj_missing() {
        // Expected-but-missing (mode != auto, smj absent) must fail CLOSED.
        assert_eq!(
            plan_tool_gate(false, false, true, Some("ask"), false),
            GatePlan::DenyAll
        );
    }

    #[test]
    fn plan_deny_all_when_gate_expected_but_session_missing() {
        assert_eq!(
            plan_tool_gate(false, false, false, Some("ask"), true),
            GatePlan::DenyAll
        );
    }

    #[test]
    fn plan_open_when_auto_mode_even_if_gate_missing() {
        // Auto allows everything anyway, so a missing gate is not a hole.
        assert_eq!(
            plan_tool_gate(false, false, false, Some("auto"), false),
            GatePlan::Open
        );
        assert_eq!(
            plan_tool_gate(false, false, true, Some("Auto"), false),
            GatePlan::Open
        );
    }

    #[test]
    fn plan_open_when_fallback_override_set() {
        // Explicit opt-back-in to the old fail-open behaviour.
        assert_eq!(
            plan_tool_gate(false, true, true, Some("ask"), false),
            GatePlan::Open
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation across concurrent tests
    async fn system_block_reaches_claude_via_append_system_prompt_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!(
            "smedja-claude-sys-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let args_file = temp_dir.join("args.txt");
        let captured_system = temp_dir.join("captured-system.txt");
        let script_path = temp_dir.join("claude");
        // Record argv and, while the run is live, copy the appended system-prompt
        // file to a known path so the test can read it after the guard unlinks it.
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--append-system-prompt-file\" ]; then cp \"$a\" '{}'; fi\n  prev=\"$a\"\ndone\ncat > /dev/null\nprintf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}]}}}}'\n",
                args_file.display(),
                captured_system.display()
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
        let system_block = "SMEDJA-SYSTEM-BLOCK: role packs, skills, methodology, project context";
        let opts = CallOptions {
            model: String::new(),
            max_tokens: None,
            temperature: None,
            system: Some(system_block.to_owned()),
            tools: None,
            provider_session_id: None,
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
        while stream.next().await.is_some() {}

        std::env::set_var("PATH", old_path);

        let args = std::fs::read_to_string(&args_file).unwrap();
        assert!(
            args.contains("--append-system-prompt-file"),
            "system block must be delivered via --append-system-prompt-file; args were:\n{args}"
        );
        let delivered = std::fs::read_to_string(&captured_system)
            .expect("the appended system-prompt file must exist during the run");
        assert_eq!(
            delivered, system_block,
            "the exact assembled system block must reach claude"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
