//! Top-level fragment expansion: parses a message, resolves each fragment, and
//! reassembles the output with surrounding prose preserved byte-for-byte.

use crate::cowork::CoworkGate;
use crate::fragments::cap::push_block;
use crate::fragments::parse::parse;
use crate::fragments::resolve::{
    resolve_branch, resolve_clippy, resolve_file, resolve_git, resolve_lsp, resolve_shell,
};
use crate::fragments::Caps;
use crate::fragments::Fragment;

/// Expands every recognised fragment in `content` in place, resolving against
/// `workspace`. When `gate` is `Some`, `@shell` commands are routed through the
/// cowork approval flow; when `None`, cowork is disabled and `@shell` runs
/// directly. Surrounding literal text is preserved byte-for-byte.
///
/// `lsp` is the daemon-side LSP manager; when `None`, `@lsp` expands to a
/// "no LSP servers running" marker instead of silently discarding the fragment.
///
/// Resolved content is capped per fragment and per message; over-cap content is
/// truncated with a visible `[smedja: truncated N bytes]` marker.
pub(crate) async fn expand(
    content: &str,
    workspace: &std::path::Path,
    gate: Option<&CoworkGate>,
    lsp: Option<&smedja_lsp::LspManager>,
) -> String {
    expand_with_caps(content, workspace, gate, lsp, Caps::from_env()).await
}

/// Core of [`expand`] with explicit `caps`, so callers (and tests) can supply
/// caps without mutating process-wide environment variables.
async fn expand_with_caps(
    content: &str,
    workspace: &std::path::Path,
    gate: Option<&CoworkGate>,
    lsp: Option<&smedja_lsp::LspManager>,
    caps: Caps,
) -> String {
    let fragments = parse(content);
    let mut out = String::with_capacity(content.len());
    let mut budget = caps.message_bytes;

    for fragment in fragments {
        match fragment {
            Fragment::Literal(text) => out.push_str(&text),
            Fragment::File(path) => {
                let block = resolve_file(workspace, &path).await;
                push_block(&mut out, "file", &path, block, caps, &mut budget);
            }
            Fragment::Git => {
                let block = resolve_git(workspace).await;
                push_block(&mut out, "git", "", block, caps, &mut budget);
            }
            Fragment::Branch => {
                let block = resolve_branch(workspace).await;
                push_block(&mut out, "branch", "", block, caps, &mut budget);
            }
            Fragment::Shell(cmd) => {
                let block = resolve_shell(workspace, &cmd, gate).await;
                push_block(&mut out, "shell", &cmd, block, caps, &mut budget);
            }
            Fragment::Clippy => {
                let block = resolve_clippy(workspace).await;
                push_block(&mut out, "clippy", "", block, caps, &mut budget);
            }
            Fragment::Lsp => {
                let block = resolve_lsp(lsp);
                push_block(&mut out, "lsp", "", block, caps, &mut budget);
            }
            Fragment::Paste(sha8) => {
                let path = std::env::temp_dir().join(format!("smedja-paste-{sha8}.txt"));
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .unwrap_or_else(|_| format!("[paste {sha8} not found]"));
                out.push_str(&content);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragments::test_caps as caps;
    use std::sync::Arc;

    // ── Paste ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn paste_fragment_injects_temp_file_content() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let sha8 = "deadbeef";
        let paste_path = std::env::temp_dir().join(format!("smedja-paste-{sha8}.txt"));
        tokio::fs::write(&paste_path, b"pasted content here")
            .await
            .unwrap();
        let out = expand_with_caps(
            &format!("before @paste:{sha8} after"),
            &ws,
            None,
            None,
            caps(1 << 20, 2_000, 1 << 20),
        )
        .await;
        tokio::fs::remove_file(&paste_path).await.ok();
        assert_eq!(out, "before pasted content here after");
    }

    #[tokio::test]
    async fn paste_fragment_missing_file_yields_marker() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let sha8 = "00000000";
        // Ensure the file does not exist.
        let paste_path = std::env::temp_dir().join(format!("smedja-paste-{sha8}.txt"));
        tokio::fs::remove_file(&paste_path).await.ok();
        let out = expand_with_caps(
            &format!("@paste:{sha8}"),
            &ws,
            None,
            None,
            caps(1 << 20, 2_000, 1 << 20),
        )
        .await;
        assert_eq!(out, format!("[paste {sha8} not found]"));
    }

    // ── @file resolution ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn file_fragment_injects_contents() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        tokio::fs::write(ws.join("hello.txt"), b"file body here")
            .await
            .unwrap();
        let out = expand_with_caps(
            "before @file hello.txt after",
            &ws,
            None,
            None,
            caps(1 << 20, 2_000, 1 << 20),
        )
        .await;
        assert!(out.contains("```file hello.txt\n"), "fenced header: {out}");
        assert!(out.contains("file body here"), "contents injected: {out}");
        assert!(out.starts_with("before "));
        assert!(out.ends_with(" after"));
    }

    #[tokio::test]
    async fn file_fragment_path_traversal_denied() {
        let parent = tempfile::tempdir().unwrap();
        let parent = parent.path().canonicalize().unwrap();
        tokio::fs::write(parent.join("secret.txt"), b"TOPSECRET")
            .await
            .unwrap();
        let ws = parent.join("ws");
        tokio::fs::create_dir(&ws).await.unwrap();

        let big = caps(1 << 20, 2_000, 1 << 20);
        let out = expand_with_caps("@file ../secret.txt", &ws, None, None, big).await;
        assert_eq!(out, "[smedja: @file rejected: path outside workspace]");
        assert!(!out.contains("TOPSECRET"), "no file content leaked: {out}");

        // Absolute path outside the workspace is rejected too.
        let abs = parent.join("secret.txt");
        let out = expand_with_caps(&format!("@file {}", abs.display()), &ws, None, None, big).await;
        assert_eq!(out, "[smedja: @file rejected: path outside workspace]");
        assert!(!out.contains("TOPSECRET"));
    }

    #[tokio::test]
    async fn file_fragment_non_file_errors() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        tokio::fs::create_dir(ws.join("adir")).await.unwrap();

        let big = caps(1 << 20, 2_000, 1 << 20);
        let out = expand_with_caps("@file adir", &ws, None, None, big).await;
        assert_eq!(out, "[smedja: @file not a regular file]");

        let out = expand_with_caps("@file missing.txt", &ws, None, None, big).await;
        assert!(
            out.starts_with("[smedja: @file unreadable:"),
            "marker: {out}"
        );
        assert!(!out.contains("```"), "no fenced block on error: {out}");
    }

    // ── @git / @branch ───────────────────────────────────────────────────────

    async fn init_git_workspace() -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        // Keep the tempdir alive for the test's duration by leaking it; the OS
        // reclaims the path when the process exits (test-only).
        std::mem::forget(dir);
        crate::exec_bash(
            "git init -q && git config user.email t@t && git config user.name t",
            &ws,
        )
        .await;
        crate::exec_bash("git checkout -q -b work", &ws).await;
        ws
    }

    #[tokio::test]
    async fn git_fragment_injects_status_and_diff() {
        let ws = init_git_workspace().await;
        tokio::fs::write(ws.join("a.txt"), b"one\n").await.unwrap();
        crate::exec_bash("git add a.txt && git commit -q -m init", &ws).await;
        tokio::fs::write(ws.join("a.txt"), b"two\n").await.unwrap();
        tokio::fs::write(ws.join("b.txt"), b"new\n").await.unwrap();

        let out = expand_with_caps("@git", &ws, None, None, caps(1 << 20, 2_000, 1 << 20)).await;
        assert!(out.contains("```git\n"), "fenced header: {out}");
        assert!(out.contains("$ git status --short"), "status label: {out}");
        assert!(out.contains("b.txt"), "untracked file in status: {out}");
        assert!(out.contains("$ git diff HEAD"), "diff label: {out}");
        assert!(
            out.contains("-one") && out.contains("+two"),
            "diff body: {out}"
        );
    }

    #[tokio::test]
    async fn branch_fragment_injects_current_branch() {
        let ws = init_git_workspace().await;
        tokio::fs::write(ws.join("a.txt"), b"one\n").await.unwrap();
        crate::exec_bash("git add a.txt && git commit -q -m init", &ws).await;

        let out = expand_with_caps("@branch", &ws, None, None, caps(1 << 20, 2_000, 1 << 20)).await;
        assert!(out.contains("```branch\n"), "fenced header: {out}");
        assert!(out.contains("branch: work"), "current branch: {out}");
    }

    // ── @shell with cowork gating ────────────────────────────────────────────

    #[tokio::test]
    async fn shell_fragment_injects_output() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let out = expand_with_caps(
            "@shell echo hi",
            &ws,
            None,
            None,
            caps(1 << 20, 2_000, 1 << 20),
        )
        .await;
        assert!(out.contains("```shell echo hi\n"), "fenced header: {out}");
        assert!(out.contains("hi"), "command output injected: {out}");
    }

    #[tokio::test]
    async fn shell_fragment_respects_cowork_decision() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        // Approved: output is injected.
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let ws2 = ws.clone();
        let handle = tokio::spawn(async move {
            expand_with_caps(
                "@shell echo hi",
                &ws2,
                Some(&gate2),
                None,
                caps(1 << 20, 2_000, 1 << 20),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let pending = gate.list_pending().await;
        assert_eq!(pending.len(), 1, "@shell must request approval");
        assert_eq!(pending[0].1.tool, "shell");
        gate.approve(&pending[0].0).await;
        let out = handle.await.unwrap();
        assert!(out.contains("hi"), "approved output injected: {out}");

        // Denied: a denial marker, no command output.
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let ws2 = ws.clone();
        let handle = tokio::spawn(async move {
            expand_with_caps(
                "@shell echo nope",
                &ws2,
                Some(&gate2),
                None,
                caps(1 << 20, 2_000, 1 << 20),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let pending = gate.list_pending().await;
        gate.deny(&pending[0].0, "no".to_owned()).await;
        let out = handle.await.unwrap();
        assert_eq!(out, "[smedja: @shell denied]");
        assert!(!out.contains("nope"));
    }

    // ── Caps ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn aggregate_fragment_cap_enforced() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        tokio::fs::write(ws.join("a.txt"), "aaaaaaaaaaaa")
            .await
            .unwrap();
        tokio::fs::write(ws.join("b.txt"), "bbbbbbbbbbbb")
            .await
            .unwrap();

        // Per-message budget of 8 bytes, with a generous per-fragment cap: the
        // first fragment consumes the whole budget and the second is fully dropped.
        let out = expand_with_caps(
            "@file a.txt @file b.txt",
            &ws,
            None,
            None,
            caps(1_000_000, 2_000, 8),
        )
        .await;
        // The first fragment's payload is capped to the 8-byte message budget.
        let first_payload = out.split(|c| c != 'a').map(str::len).max().unwrap_or(0);
        assert!(first_payload <= 8, "first fragment capped to budget: {out}");
        // The second fragment's payload is fully dropped once the budget is spent
        // (no run of payload `b` bytes survives; the lone `b` in `b.txt` is the header).
        let second_payload = out.split(|c| c != 'b').map(str::len).max().unwrap_or(0);
        assert!(
            second_payload <= 1,
            "second fragment dropped once budget exhausted: {out}"
        );
        assert!(
            out.contains("[smedja: truncated"),
            "truncation visible: {out}"
        );
    }
}
