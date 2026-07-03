//! Per-fragment resolution: turns a recognised [`Fragment`](crate::fragments::Fragment)
//! into either fenced-block content or a verbatim error/denial marker.

use crate::cowork::{ApprovalPrompt, CoworkGate, Decision};
use crate::executor::fs_tools::assert_within_workspace;

/// Cowork approval timeout (seconds) for an `@shell` fragment, mirroring the
/// tool-execution gate timeout used by the orchestrator.
const SHELL_APPROVAL_TIMEOUT_SECS: u64 = 300;

/// Resolution outcome for a single fragment: either fenced-block content to be
/// capped, or a verbatim error/denial marker that bypasses capping.
pub(crate) enum Resolved {
    /// Content destined for a fenced block, subject to size caps.
    Content(String),
    /// A short marker (rejection / denial) injected verbatim, not capped.
    Marker(String),
}

/// Resolves an `@file` fragment, routing the path through the workspace boundary
/// check. On rejection or a non-file/unreadable path, yields an error marker and
/// reads nothing.
pub(crate) async fn resolve_file(workspace: &std::path::Path, path: &str) -> Resolved {
    let Ok(full) = assert_within_workspace(workspace, path) else {
        return Resolved::Marker("[smedja: @file rejected: path outside workspace]".to_owned());
    };
    match tokio::fs::metadata(&full).await {
        Ok(meta) if meta.is_file() => match tokio::fs::read_to_string(&full).await {
            Ok(contents) => Resolved::Content(contents),
            Err(e) => Resolved::Marker(format!("[smedja: @file unreadable: {e}]")),
        },
        Ok(_) => Resolved::Marker("[smedja: @file not a regular file]".to_owned()),
        Err(e) => Resolved::Marker(format!("[smedja: @file unreadable: {e}]")),
    }
}

/// Resolves an `@git` fragment to `git status --short` plus `git diff HEAD`,
/// run in the session workspace.
pub(crate) async fn resolve_git(workspace: &std::path::Path) -> Resolved {
    let status = crate::exec_bash("git status --short", workspace).await;
    let diff = crate::exec_bash("git diff HEAD", workspace).await;
    Resolved::Content(format!(
        "$ git status --short\n{status}\n$ git diff HEAD\n{diff}"
    ))
}

/// Resolves an `@branch` fragment to the current branch name and its upstream (when set).
pub(crate) async fn resolve_branch(workspace: &std::path::Path) -> Resolved {
    let branch = crate::exec_bash("git rev-parse --abbrev-ref HEAD", workspace)
        .await
        .trim()
        .to_owned();
    let upstream = crate::exec_bash(
        "git rev-parse --abbrev-ref --symbolic-full-name @{u}",
        workspace,
    )
    .await
    .trim()
    .to_owned();
    let body = if upstream.is_empty() || upstream.starts_with("error:") {
        format!("branch: {branch}")
    } else {
        format!("branch: {branch}\nupstream: {upstream}")
    };
    Resolved::Content(body)
}

/// Resolves an `@clippy` fragment by running `cargo clippy --message-format=short`
/// in `workspace`. No cowork gate is applied because clippy is read-only static
/// analysis — but note that `cargo` may run `build.rs` scripts, which can execute
/// arbitrary code. Only use `@clippy` in workspaces you trust.
pub(crate) async fn resolve_clippy(workspace: &std::path::Path) -> Resolved {
    let out = crate::exec_bash("cargo clippy --message-format=short 2>&1", workspace).await;
    Resolved::Content(out)
}

/// Resolves an `@lsp` fragment from the daemon's shared `LspManager` snapshot.
pub(crate) fn resolve_lsp(lsp: Option<&smedja_lsp::LspManager>) -> Resolved {
    let Some(mgr) = lsp else {
        return Resolved::Marker("[smedja: @lsp — no LSP manager available]".to_owned());
    };
    let snap = mgr.snapshot();
    if snap.servers.is_empty() {
        return Resolved::Marker(
            "[smedja: @lsp — no language servers running (install rust-analyzer, pyright, gopls, etc.)]"
                .to_owned(),
        );
    }
    let mut lines = vec!["LSP diagnostics:".to_owned()];
    if snap.diagnostics.is_empty() {
        lines.push("  (clean — no diagnostics)".to_owned());
    } else {
        for d in &snap.diagnostics {
            let label = d.severity.label();
            let code = d
                .code
                .as_deref()
                .map_or_else(String::new, |c| format!(" {c}"));
            lines.push(format!(
                "  {label}{code}  {}:{}  {}",
                d.file.display(),
                d.line,
                d.message
            ));
        }
    }
    Resolved::Content(lines.join("\n"))
}

/// Resolves an `@shell` fragment, gating execution through cowork when enabled.
pub(crate) async fn resolve_shell(
    workspace: &std::path::Path,
    cmd: &str,
    gate: Option<&CoworkGate>,
) -> Resolved {
    if gate.is_none() {
        tracing::warn!(cmd = %cmd, "executing @shell fragment without cowork gate — enable cowork mode to require human approval");
    }
    if let Some(gate) = gate {
        let prompt = ApprovalPrompt {
            step_n: 0,
            tool: "shell".to_owned(),
            args_scrubbed: serde_json::json!({ "cmd": cmd }),
            reasoning: "inline @shell fragment".to_owned(),
            plan_summary: String::new(),
        };
        match gate
            .intercept(prompt, SHELL_APPROVAL_TIMEOUT_SECS, None)
            .await
        {
            Decision::Approve => {}
            // A denial or a modify request both mean "do not run this command as
            // submitted"; an inline fragment has no place to apply a modification.
            Decision::Deny(_) | Decision::Modify(_) => {
                return Resolved::Marker("[smedja: @shell denied]".to_owned());
            }
        }
    }
    let output = crate::exec_bash(cmd, workspace).await;
    Resolved::Content(output)
}
