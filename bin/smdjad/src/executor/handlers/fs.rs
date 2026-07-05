//! Filesystem and shell-exec tool bodies dispatched from `execute_tool`.
//!
//! Covers `bash`/`run_command`, `read_file`, `list_files`, `grep_files`,
//! `find_files`, `move_file`, `copy_file`, and `delete_file`. Guard failures
//! that the original arms exited via `return` are surfaced as `Err` so the
//! caller reproduces the exact (scan-bypassing) control flow.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::executor::fs_tools::{assert_within_workspace, role_allows_write_bash};
use crate::executor::{confined_root_for, filter_command_output};
use crate::sandbox::SandboxExecutor;

/// Minimal glob matcher supporting `*` (any sequence except `/`) and `?` (one char).
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let mut p = pattern.as_bytes();
    let mut s = name.as_bytes();
    loop {
        match (p.first(), s.first()) {
            (None, None) => return true,
            (Some(&b'*'), _) => {
                p = &p[1..];
                if p.is_empty() {
                    return true;
                }
                // Try matching `*` against 0..n chars.
                for i in 0..=s.len() {
                    if glob_match(
                        std::str::from_utf8(p).unwrap_or(""),
                        std::str::from_utf8(&s[i..]).unwrap_or(""),
                    ) {
                        return true;
                    }
                }
                return false;
            }
            (Some(&b'?'), Some(_)) => {
                p = &p[1..];
                s = &s[1..];
            }
            (Some(a), Some(b)) if a == b => {
                p = &p[1..];
                s = &s[1..];
            }
            _ => return false,
        }
    }
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct BashConfig {
    pub(crate) blocked_patterns: Option<Vec<String>>,
    pub(crate) timeout_secs: Option<u64>,
}

pub(crate) fn bash_config(workspace: &std::path::Path) -> BashConfig {
    #[derive(serde::Deserialize, Default)]
    struct WorkspaceToml {
        tools: Option<ToolsSection>,
    }
    #[derive(serde::Deserialize, Default)]
    struct ToolsSection {
        bash: Option<BashConfig>,
    }
    let path = workspace.join(".smedja").join("workspace.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<WorkspaceToml>(&s).ok())
        .and_then(|c| c.tools?.bash)
        .unwrap_or_default()
}

/// `bash` / `run_command` tool body.
pub(crate) async fn bash(
    tool_name: &str,
    input: &Value,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
) -> Result<String, String> {
    let cmd = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    let bash_cfg = bash_config(workspace);

    // Blocked patterns — checked before any spawn, all permission modes.
    for pat in bash_cfg.blocked_patterns.unwrap_or_default() {
        if cmd.contains(&*pat) {
            return Err(format!(
                "error: command blocked by policy (matched pattern: {pat})"
            ));
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
                return Err(format!("error: env key '{key}' is not allowed"));
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
            return Err(
                "permission denied: review mode sessions cannot execute write commands".to_owned(),
            );
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
    Ok(filter_command_output(cmd, raw, workspace, session, ingot, vault).await)
}

/// `read_file` tool body.
pub(crate) async fn read_file(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let path_str = input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let full = match assert_within_workspace(workspace, path_str) {
        Ok(p) => p,
        Err(err) => return Err(err),
    };
    let encoding = input
        .get("encoding")
        .and_then(Value::as_str)
        .unwrap_or("text");
    let start_line = input
        .get("start_line")
        .and_then(Value::as_u64)
        .map(|n| n.try_into().unwrap_or(usize::MAX));
    let end_line = input
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|n| n.try_into().unwrap_or(usize::MAX));
    let raw = match tokio::fs::read(&full).await {
        Ok(bytes) => bytes,
        Err(e) => return Err(format!("error reading {path_str}: {e}")),
    };
    Ok(if encoding == "base64" {
        base64::engine::general_purpose::STANDARD.encode(&raw)
    } else {
        let text = String::from_utf8_lossy(&raw).into_owned();
        if start_line.is_none() && end_line.is_none() {
            text
        } else {
            let start = start_line.unwrap_or(1).saturating_sub(1);
            let lines: Vec<&str> = text.lines().collect();
            let end = end_line.map_or(lines.len(), |e| e.min(lines.len()));
            lines[start.min(lines.len())..end].join("\n")
        }
    })
}

/// `list_files` tool body.
pub(crate) async fn list_files(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let dir_str = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let full = match assert_within_workspace(workspace, dir_str) {
        Ok(p) => p,
        Err(err) => return Err(err),
    };
    let depth: usize = input
        .get("depth")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .try_into()
        .unwrap_or(usize::MAX);
    let pattern = input
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let max_depth = if depth == 0 { usize::MAX } else { depth };
    Ok(
        match tokio::task::spawn_blocking(move || {
            let mut entries = Vec::new();
            let walker = walkdir::WalkDir::new(&full)
                .max_depth(max_depth)
                .into_iter()
                .filter_entry(|e| {
                    e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.')
                });
            for entry in walker.filter_map(std::result::Result::ok).skip(1) {
                let name = entry.path().strip_prefix(&full).unwrap_or(entry.path());
                let name_str = name.to_string_lossy().into_owned();
                if let Some(ref pat) = pattern {
                    // Simple glob: only match file name portion against the pattern.
                    let file_name = entry.file_name().to_string_lossy();
                    if !glob_match(pat, &file_name) {
                        continue;
                    }
                }
                entries.push(name_str);
            }
            entries.sort();
            entries.join("\n")
        })
        .await
        {
            Ok(result) => result,
            Err(e) => format!("error listing {dir_str}: {e}"),
        },
    )
}

/// `grep_files` tool body.
pub(crate) async fn grep_files(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let pattern = match input.get("pattern").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => return Err("error: pattern field required".into()),
    };
    let sub = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let max_results: usize = input
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .try_into()
        .unwrap_or(usize::MAX);
    let root = match assert_within_workspace(workspace, sub) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    Ok(tokio::task::spawn_blocking(move || {
        let mut hits: Vec<serde_json::Value> = Vec::new();
        let walker = walkdir::WalkDir::new(&root).into_iter();
        'files: for entry in walker.filter_map(std::result::Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            for (n, line) in text.lines().enumerate() {
                if line.contains(pattern.as_str()) {
                    hits.push(serde_json::json!({
                        "file": rel,
                        "line": n + 1,
                        "text": line.trim_end(),
                    }));
                    if hits.len() >= max_results {
                        break 'files;
                    }
                }
            }
        }
        let count = hits.len();
        serde_json::json!({"matches": hits, "count": count}).to_string()
    })
    .await
    .unwrap_or_else(|e| format!("error: grep_files failed: {e}")))
}

/// `find_files` tool body.
pub(crate) async fn find_files(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let pattern = input
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("*")
        .to_owned();
    let sub = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let max_results: usize = input
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .try_into()
        .unwrap_or(usize::MAX);
    let root = match assert_within_workspace(workspace, sub) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    Ok(tokio::task::spawn_blocking(move || {
        let mut files: Vec<String> = Vec::new();
        let walker = walkdir::WalkDir::new(&root).into_iter();
        for entry in walker.filter_map(std::result::Result::ok).skip(1) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if glob_match(&pattern, &name) {
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();
                files.push(rel);
                if files.len() >= max_results {
                    break;
                }
            }
        }
        files.sort();
        let count = files.len();
        serde_json::json!({"files": files, "count": count}).to_string()
    })
    .await
    .unwrap_or_else(|e| format!("error: find_files failed: {e}")))
}

/// `move_file` tool body.
pub(crate) async fn move_file(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let Some(src_str) = input.get("source").and_then(Value::as_str) else {
        return Err("error: source field required".into());
    };
    let Some(dst_str) = input.get("destination").and_then(Value::as_str) else {
        return Err("error: destination field required".into());
    };
    let src = match assert_within_workspace(workspace, src_str) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let dst = match assert_within_workspace(workspace, dst_str) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    Ok(match tokio::fs::rename(&src, &dst).await {
        Ok(()) => serde_json::json!({"moved": true}).to_string(),
        Err(e) => format!("error: move_file failed: {e}"),
    })
}

/// `copy_file` tool body.
pub(crate) async fn copy_file(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let Some(src_str) = input.get("source").and_then(Value::as_str) else {
        return Err("error: source field required".into());
    };
    let Some(dst_str) = input.get("destination").and_then(Value::as_str) else {
        return Err("error: destination field required".into());
    };
    let src = match assert_within_workspace(workspace, src_str) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let dst = match assert_within_workspace(workspace, dst_str) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    if let Some(parent) = dst.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    Ok(match tokio::fs::copy(&src, &dst).await {
        Ok(_) => serde_json::json!({"copied": true}).to_string(),
        Err(e) => format!("error: copy_file failed: {e}"),
    })
}

/// `delete_file` tool body.
pub(crate) async fn delete_file(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let Some(path_str) = input.get("path").and_then(Value::as_str) else {
        return Err("error: path field required".into());
    };
    let full = match assert_within_workspace(workspace, path_str) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    // ponytail: full async cowork gate is in roadmap; log destructive op and proceed
    tracing::info!(
        path = path_str,
        "delete_file: cowork gate (full approval gate is in roadmap)"
    );
    Ok(match tokio::fs::metadata(&full).await {
        Err(e) => format!("error: delete_file failed: {e}"),
        Ok(meta) if meta.is_dir() => match tokio::fs::remove_dir(&full).await {
            Ok(()) => serde_json::json!({"deleted": true}).to_string(),
            Err(e) => {
                format!("error: delete_file failed (use bash for non-empty directories): {e}")
            }
        },
        Ok(_) => match tokio::fs::remove_file(&full).await {
            Ok(()) => serde_json::json!({"deleted": true}).to_string(),
            Err(e) => format!("error: delete_file failed: {e}"),
        },
    })
}
