//! smedja's `PreToolUse` approval hook installation for the Claude CLI.

/// POSIX single-quotes a string for safe embedding in a shell command (claude
/// runs the hook command through a shell).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Installs smedja's `PreToolUse` approval hook on a `claude` command so each of
/// claude's own tool calls is gated through the daemon's permission policy
/// (`smj tool-gate` → `cowork.gate_tool`, which blocks on the user when the
/// policy says "ask"). The smedja session id is passed via `SMEDJA_SESSION_ID`
/// so the hook knows which session's gate to consult.
///
/// No-op when `SMEDJA_TOOL_GATE=off`, `smj` is not on `$PATH`, or there is no
/// session to attribute approvals to — the hook then "fails open" (claude runs
/// unimpeded) rather than bricking the agent.
pub(crate) fn install_tool_gate(command: &mut tokio::process::Command, session_id: Option<&str>) {
    let disabled = std::env::var("SMEDJA_TOOL_GATE").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "none" | "disable" | "disabled"
        )
    });
    if disabled {
        return;
    }
    let Some(session_id) = session_id.filter(|s| !s.is_empty()) else {
        return;
    };
    // Bake the absolute smj path into the hook so it resolves regardless of
    // claude's own PATH. If smj can't be found, skip the hook (fail open).
    let Ok(smj) = which::which("smj") else {
        return;
    };
    let hook_command = format!("{} tool-gate", shell_quote(&smj.to_string_lossy()));
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
    let path = std::env::temp_dir().join("smedja-claude-settings.json");
    if std::fs::write(&path, settings.to_string()).is_err() {
        return;
    }
    command.arg("--settings").arg(&path);
    command.env("SMEDJA_SESSION_ID", session_id);
}
