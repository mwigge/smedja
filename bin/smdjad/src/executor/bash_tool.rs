//! The `bash` / `run_command` handler: policy checks, sandbox confinement, and
//! command-output filtering on the return path.

use std::sync::Arc;

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::executor::config::bash_config;
use crate::executor::confined_root_for;
use crate::executor::fs_tools::role_allows_write_bash;
use crate::executor::output_filter::filter_command_output;
use crate::sandbox::SandboxExecutor;

/// Executes a `bash` / `run_command` tool call under the workspace policy and
/// sandbox, then applies command-aware output filtering to the result.
pub(crate) async fn run_bash(
    tool_name: &str,
    input: &Value,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
) -> String {
    let cmd = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    let bash_cfg = bash_config(workspace);

    // Blocked patterns — checked before any spawn, all permission modes.
    for pat in bash_cfg.blocked_patterns.unwrap_or_default() {
        if cmd.contains(&*pat) {
            return format!("error: command blocked by policy (matched pattern: {pat})");
        }
    }

    // Per-call env map — validate keys against the security blocklist.
    const ENV_BLOCKLIST: &[&str] = &[
        "PATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "HOME",
        "USER",
        "SHELL",
    ];
    let env_extra: Option<std::collections::HashMap<String, String>> =
        input.get("env").and_then(Value::as_object).map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        });
    if let Some(ref env) = env_extra {
        for key in env.keys() {
            if ENV_BLOCKLIST.contains(&key.as_str()) || key.starts_with("SMEDJA_") {
                return format!("error: env key '{key}' is not allowed");
            }
        }
    }
    // Per-call timeout overrides workspace default; workspace default overrides compile-time default.
    let timeout_secs = input
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .or(bash_cfg.timeout_secs);
    let stdin_bytes: Option<Vec<u8>> = input
        .get("stdin")
        .and_then(Value::as_str)
        .map(|s| s.as_bytes().to_vec());

    // Enforce read-only mode for review sessions.
    if session.is_some_and(|s| !role_allows_write_bash(s)) {
        let arity = smedja_assayer::classify_bash(cmd);
        if arity == smedja_assayer::BashArity::Write {
            return "permission denied: review mode sessions cannot execute write commands"
                .to_owned();
        }
    }

    // SandboxExecutor: confine execution to the resolved confined root
    // (the active worktree when a task owns one, else the workspace).
    // Exempt tools never reach this arm. The fallback contract
    // (auto/required/off) is enforced inside `run_confined`.
    let sandbox = SandboxExecutor::new();
    let raw = if SandboxExecutor::is_exempt(tool_name) {
        crate::exec_bash_ext(cmd, workspace, timeout_secs, env_extra, stdin_bytes).await
    } else {
        let confined_root = confined_root_for(workspace);
        let cmd_owned = cmd.to_owned();
        let ws = workspace.to_owned();
        sandbox
            .run_confined(cmd, &confined_root, || async move {
                crate::exec_bash_ext(&cmd_owned, &ws, timeout_secs, env_extra, stdin_bytes).await
            })
            .await
    };

    // Command-aware text filtering on the return path (in-process; no
    // shell hooks, no subprocess). Compresses verbose output before it
    // enters working memory, tees the full text to the vault for
    // recovery, and records tokens saved. The success/failure contract
    // is unaffected — only the body text is compressed.
    filter_command_output(cmd, raw, workspace, session, ingot, vault).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::executor::execute_tool;

    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    // --- WI-014: bash blocked_patterns (dispatch path) ---

    #[tokio::test]
    async fn bash_blocked_pattern_match_returns_error() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("workspace.toml"),
            "[tools.bash]\nblocked_patterns = [\"rm -rf /\"]\n",
        )
        .unwrap();

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        let result = execute_tool(
            "bash",
            r#"{"command":"rm -rf / --no-preserve-root"}"#,
            dir.path(),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("blocked by policy"),
            "blocked command must return policy error, got: {result}"
        );
    }

    #[tokio::test]
    async fn bash_non_blocked_command_not_affected() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("workspace.toml"),
            "[tools.bash]\nblocked_patterns = [\"rm -rf /\"]\n",
        )
        .unwrap();

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        let result = execute_tool(
            "bash",
            r#"{"command":"echo hello"}"#,
            dir.path(),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("hello"),
            "non-blocked command must execute normally, got: {result}"
        );
    }

    // --- WI-019: bash timeout_secs, env map, stdin ---

    #[tokio::test]
    async fn bash_env_blocklisted_key_rejected() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        let result = execute_tool(
            "bash",
            r#"{"command":"echo hi","env":{"PATH":"/evil"}}"#,
            dir.path(),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("not allowed"),
            "blocklisted env key must return error, got: {result}"
        );
    }

    #[tokio::test]
    async fn bash_env_injected_into_command() {
        let dir = tempfile::tempdir().unwrap();
        let env: std::collections::HashMap<String, String> =
            [("MY_VAR".into(), "smedja_test".into())].into();
        let result = crate::exec_bash_ext("echo $MY_VAR", dir.path(), None, Some(env), None).await;
        assert!(
            result.contains("smedja_test"),
            "injected env var must appear in output, got: {result}"
        );
    }

    #[tokio::test]
    async fn bash_stdin_fed_to_command() {
        let dir = tempfile::tempdir().unwrap();
        let result = crate::exec_bash_ext(
            "cat",
            dir.path(),
            None,
            None,
            Some(b"hello from stdin".to_vec()),
        )
        .await;
        assert!(
            result.contains("hello from stdin"),
            "stdin must be forwarded to command, got: {result}"
        );
    }

    #[tokio::test]
    async fn bash_timeout_secs_short_timeout_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = crate::exec_bash_ext("sleep 10", dir.path(), Some(1), None, None).await;
        assert!(
            result.contains("timed out"),
            "short timeout must return timeout error, got: {result}"
        );
    }

    // ── WI-012: stderr block, partial output on timeout ───────────────────────

    #[tokio::test]
    async fn bash_stderr_appended_as_block_on_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        // Write something to stdout and something to stderr, then exit 1.
        let result = crate::exec_bash_ext(
            "echo out; echo err >&2; exit 1",
            dir.path(),
            None,
            None,
            None,
        )
        .await;
        assert!(
            result.starts_with("error:"),
            "non-zero exit must start with error: prefix; got: {result}"
        );
        assert!(
            result.contains("[stderr]"),
            "stderr must appear in a [stderr] block; got: {result}"
        );
        assert!(
            result.contains("err"),
            "stderr content must be included; got: {result}"
        );
    }

    #[tokio::test]
    async fn bash_partial_output_returned_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // Print one line immediately, then sleep to trigger timeout.
        let result =
            crate::exec_bash_ext("echo partial; sleep 10", dir.path(), Some(1), None, None).await;
        assert!(
            result.contains("partial"),
            "output emitted before timeout must be returned; got: {result}"
        );
        assert!(
            result.contains("timed out"),
            "timeout message must be present; got: {result}"
        );
    }
}
