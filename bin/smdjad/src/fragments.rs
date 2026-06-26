//! Inline context-fragment expansion for `turn.submit`.
//!
//! Recognises `@file <path>`, `@git`, `@branch`, and `@shell <cmd>` tokens inside
//! a submitted message and expands each in place into a fenced content block,
//! preserving surrounding prose. Expansion runs daemon-side so it can reuse the
//! workspace-boundary check ([`assert_within_workspace`]), the sandboxed shell
//! runner ([`exec_bash`](crate::exec_bash)), and the cowork approval gate
//! ([`CoworkGate`]).
//!
//! A fragment is recognised only when `@` begins a token (start-of-string or
//! immediately after whitespace) and `<kind>` is one of the four known kinds, so
//! email addresses and `foo@bar` inside prose are left verbatim. Each fragment's
//! resolved content is size-capped per fragment and per message; over-cap content
//! is truncated with a visible marker.

use crate::cowork::{ApprovalPrompt, CoworkGate, Decision};
use crate::executor::fs_tools::assert_within_workspace;

/// Default per-fragment byte cap (64 KiB), overridable via `SMEDJA_FRAGMENT_MAX_BYTES`.
const DEFAULT_FRAGMENT_MAX_BYTES: usize = 64 * 1024;
/// Default per-fragment line cap (2 000 lines).
const DEFAULT_FRAGMENT_MAX_LINES: usize = 2_000;
/// Default per-message aggregate byte cap (256 KiB), overridable via
/// `SMEDJA_FRAGMENT_MAX_TOTAL_BYTES`.
const DEFAULT_MESSAGE_MAX_BYTES: usize = 256 * 1024;

/// Cowork approval timeout (seconds) for an `@shell` fragment, mirroring the
/// tool-execution gate timeout used by the orchestrator.
const SHELL_APPROVAL_TIMEOUT_SECS: u64 = 300;

/// A parsed span of the submitted message: either verbatim literal text or one
/// recognised context fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fragment {
    /// Text passed through unchanged (including unrecognised `@word` tokens).
    Literal(String),
    /// `@file <path>` — inject the file's contents.
    File(String),
    /// `@git` — inject `git status --short` and `git diff HEAD`.
    Git,
    /// `@branch` — inject the current branch and upstream.
    Branch,
    /// `@shell <cmd>` — inject the command's captured output.
    Shell(String),
    /// `@clippy` — inject `cargo clippy --message-format=short` output (no
    /// cowork gate; read-only static analysis, never modifies files).
    Clippy,
    /// `@lsp` — inject the current LSP diagnostic snapshot from the daemon's
    /// `LspManager`. Empty when no language servers are running.
    Lsp,
}

/// Per-fragment and per-message size caps, with environment overrides resolved at
/// construction time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Caps {
    per_fragment_bytes: usize,
    per_fragment_lines: usize,
    message_bytes: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            per_fragment_bytes: DEFAULT_FRAGMENT_MAX_BYTES,
            per_fragment_lines: DEFAULT_FRAGMENT_MAX_LINES,
            message_bytes: DEFAULT_MESSAGE_MAX_BYTES,
        }
    }
}

impl Caps {
    /// Builds the caps, honouring `SMEDJA_FRAGMENT_MAX_BYTES` and
    /// `SMEDJA_FRAGMENT_MAX_TOTAL_BYTES`. A non-numeric or empty value keeps the
    /// default.
    #[must_use]
    pub(crate) fn from_env() -> Self {
        let mut caps = Self::default();
        if let Some(n) = env_usize("SMEDJA_FRAGMENT_MAX_BYTES") {
            caps.per_fragment_bytes = n;
        }
        if let Some(n) = env_usize("SMEDJA_FRAGMENT_MAX_TOTAL_BYTES") {
            caps.message_bytes = n;
        }
        caps
    }
}

/// Reads a `usize` environment override, returning `None` when unset, empty, or
/// not a valid number.
fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Returns `true` when `kind` names a recognised fragment.
fn is_known_kind(kind: &str) -> bool {
    matches!(kind, "file" | "git" | "branch" | "shell" | "clippy" | "lsp")
}

/// Parses `content` into a sequence of literal-text and recognised-fragment
/// spans.
///
/// A fragment is recognised only when `@` begins a token (preceded by
/// start-of-string or whitespace). `@file` consumes the next whitespace-delimited
/// token as its path; `@shell` consumes the remainder of the line as its command;
/// `@git` / `@branch` take no argument. An `@<kind>` with an unknown kind — or
/// `@file` with no following path token — is left verbatim.
#[must_use]
pub(crate) fn parse(content: &str) -> Vec<Fragment> {
    let bytes = content.as_bytes();
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut literal_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'@' || !at_token_boundary(bytes, i) {
            i += 1;
            continue;
        }

        let kind_start = i + 1;
        let kind_end = scan_word(bytes, kind_start);
        let kind = &content[kind_start..kind_end];
        if !is_known_kind(kind) {
            i = kind_end.max(i + 1);
            continue;
        }

        let Some((fragment, consumed_end)) = take_fragment(content, bytes, kind, kind_end) else {
            // Recognised kind but malformed (e.g. `@file` with no path): leave
            // verbatim by skipping past the kind word.
            i = kind_end.max(i + 1);
            continue;
        };

        // Flush any pending literal text before this fragment.
        if literal_start < i {
            fragments.push(Fragment::Literal(content[literal_start..i].to_owned()));
        }
        fragments.push(fragment);
        i = consumed_end;
        literal_start = consumed_end;
    }

    if literal_start < content.len() {
        fragments.push(Fragment::Literal(content[literal_start..].to_owned()));
    }
    fragments
}

/// Returns `true` when the `@` at `at` begins a token (start-of-string or
/// preceded by an ASCII-whitespace byte).
fn at_token_boundary(bytes: &[u8], at: usize) -> bool {
    at == 0 || bytes[at - 1].is_ascii_whitespace()
}

/// Returns the index one past the run of word bytes (ASCII alphanumeric,
/// underscore, or hyphen) starting at `start`.
fn scan_word(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
    {
        j += 1;
    }
    j
}

/// Builds the [`Fragment`] for a recognised `kind` whose word ends at `kind_end`,
/// returning the fragment and the byte offset one past everything it consumes.
///
/// Returns `None` when the kind requires an argument that is absent (e.g. `@file`
/// with no following path token), so the caller can leave the token verbatim.
fn take_fragment(
    content: &str,
    bytes: &[u8],
    kind: &str,
    kind_end: usize,
) -> Option<(Fragment, usize)> {
    match kind {
        "git" => Some((Fragment::Git, kind_end)),
        "branch" => Some((Fragment::Branch, kind_end)),
        "clippy" => Some((Fragment::Clippy, kind_end)),
        "lsp" => Some((Fragment::Lsp, kind_end)),
        "file" => {
            // Skip inline spaces/tabs (not newlines) before the path token.
            let path_start = skip_inline_space(bytes, kind_end);
            let path_end = scan_path(bytes, path_start);
            if path_end == path_start {
                return None;
            }
            Some((
                Fragment::File(content[path_start..path_end].to_owned()),
                path_end,
            ))
        }
        "shell" => {
            let cmd_start = skip_inline_space(bytes, kind_end);
            let cmd_end = scan_to_eol(bytes, cmd_start);
            let cmd = content[cmd_start..cmd_end].trim_end();
            if cmd.is_empty() {
                return None;
            }
            Some((Fragment::Shell(cmd.to_owned()), cmd_end))
        }
        _ => None,
    }
}

/// Returns the index past any run of inline spaces/tabs starting at `start`
/// (newlines terminate the run).
fn skip_inline_space(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    j
}

/// Returns the index one past the path token starting at `start` (a path is a run
/// of non-whitespace bytes).
fn scan_path(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    j
}

/// Returns the index of the next newline at or after `start`, or the end of the
/// buffer when none remains.
fn scan_to_eol(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && bytes[j] != b'\n' {
        j += 1;
    }
    j
}

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
        }
    }
    out
}

/// Resolution outcome for a single fragment: either fenced-block content to be
/// capped, or a verbatim error/denial marker that bypasses capping.
enum Resolved {
    /// Content destined for a fenced block, subject to size caps.
    Content(String),
    /// A short marker (rejection / denial) injected verbatim, not capped.
    Marker(String),
}

/// Resolves an `@file` fragment, routing the path through the workspace boundary
/// check. On rejection or a non-file/unreadable path, yields an error marker and
/// reads nothing.
async fn resolve_file(workspace: &std::path::Path, path: &str) -> Resolved {
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
async fn resolve_git(workspace: &std::path::Path) -> Resolved {
    let status = crate::exec_bash("git status --short", workspace).await;
    let diff = crate::exec_bash("git diff HEAD", workspace).await;
    Resolved::Content(format!(
        "$ git status --short\n{status}\n$ git diff HEAD\n{diff}"
    ))
}

/// Resolves an `@branch` fragment to the current branch name and its upstream (when set).
async fn resolve_branch(workspace: &std::path::Path) -> Resolved {
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
async fn resolve_clippy(workspace: &std::path::Path) -> Resolved {
    let out = crate::exec_bash("cargo clippy --message-format=short 2>&1", workspace).await;
    Resolved::Content(out)
}

/// Resolves an `@lsp` fragment from the daemon's shared `LspManager` snapshot.
fn resolve_lsp(lsp: Option<&smedja_lsp::LspManager>) -> Resolved {
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
async fn resolve_shell(
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
        match gate.intercept(prompt, SHELL_APPROVAL_TIMEOUT_SECS).await {
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

/// Appends a resolved fragment to `out`. Content blocks are size-capped (per
/// fragment and against the remaining message `budget`) and wrapped in a fenced
/// block tagged with `lang` and an optional `arg`; markers are injected verbatim.
fn push_block(
    out: &mut String,
    lang: &str,
    arg: &str,
    resolved: Resolved,
    caps: Caps,
    budget: &mut usize,
) {
    let content = match resolved {
        Resolved::Marker(marker) => {
            out.push_str(&marker);
            return;
        }
        Resolved::Content(c) => c,
    };

    let per_fragment_cap = caps.per_fragment_bytes.min(*budget);
    let capped = cap_content(&content, per_fragment_cap, caps.per_fragment_lines);
    *budget = budget.saturating_sub(capped.len());

    let header = if arg.is_empty() {
        format!("```{lang}\n")
    } else {
        format!("```{lang} {arg}\n")
    };
    out.push_str(&header);
    out.push_str(&capped);
    if !capped.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```");
}

/// Truncates `content` to at most `max_bytes` bytes and `max_lines` lines,
/// appending a `[smedja: truncated N bytes]` marker when anything is dropped.
/// Byte truncation respects UTF-8 char boundaries.
fn cap_content(content: &str, max_bytes: usize, max_lines: usize) -> String {
    // Line cap first: keep at most `max_lines` lines.
    let mut kept = content;
    let mut line_truncated = false;
    if content.lines().count() > max_lines {
        let mut end = 0usize;
        for (n, line) in content.split_inclusive('\n').enumerate() {
            if n >= max_lines {
                break;
            }
            end += line.len();
        }
        kept = &content[..end];
        line_truncated = true;
    }

    // Byte cap on the (possibly line-capped) slice, respecting char boundaries.
    let mut byte_truncated = false;
    let mut byte_end = kept.len();
    if kept.len() > max_bytes {
        byte_end = max_bytes;
        while byte_end > 0 && !kept.is_char_boundary(byte_end) {
            byte_end -= 1;
        }
        byte_truncated = true;
    }
    let final_slice = &kept[..byte_end];

    if !line_truncated && !byte_truncated {
        return content.to_owned();
    }
    let dropped = content.len() - final_slice.len();
    format!("{final_slice}\n[smedja: truncated {dropped} bytes]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Builds explicit caps for tests, avoiding any process-wide env mutation.
    fn caps(per_fragment_bytes: usize, per_fragment_lines: usize, message_bytes: usize) -> Caps {
        Caps {
            per_fragment_bytes,
            per_fragment_lines,
            message_bytes,
        }
    }

    // ── 1. Parser ────────────────────────────────────────────────────────────

    #[test]
    fn recognises_known_fragments_only_at_token_boundary() {
        let frags = parse("see @file src/lib.rs and @git then @branch and @shell echo hi");
        assert_eq!(
            frags,
            vec![
                Fragment::Literal("see ".to_owned()),
                Fragment::File("src/lib.rs".to_owned()),
                Fragment::Literal(" and ".to_owned()),
                Fragment::Git,
                Fragment::Literal(" then ".to_owned()),
                Fragment::Branch,
                Fragment::Literal(" and ".to_owned()),
                Fragment::Shell("echo hi".to_owned()),
            ]
        );

        // `@` not at a token boundary is never a fragment.
        let frags = parse("email me at foo@bar.com or user@file.com");
        assert_eq!(
            frags,
            vec![Fragment::Literal(
                "email me at foo@bar.com or user@file.com".to_owned()
            )]
        );
    }

    #[test]
    fn unknown_fragment_left_verbatim() {
        let frags = parse("hello @world and @fileness stays");
        assert_eq!(
            frags,
            vec![Fragment::Literal(
                "hello @world and @fileness stays".to_owned()
            )]
        );
    }

    #[test]
    fn shell_consumes_to_end_of_line_only() {
        let frags = parse("@shell ls -la | grep foo\nnext line");
        assert_eq!(
            frags,
            vec![
                Fragment::Shell("ls -la | grep foo".to_owned()),
                Fragment::Literal("\nnext line".to_owned()),
            ]
        );
    }

    #[test]
    fn file_without_path_left_verbatim() {
        let frags = parse("@file\n");
        assert_eq!(frags, vec![Fragment::Literal("@file\n".to_owned())]);
    }

    // ── 2. @file resolution ──────────────────────────────────────────────────

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

    // ── 3. @git / @branch ────────────────────────────────────────────────────

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

    // ── 4. @shell with cowork gating ─────────────────────────────────────────

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

    // ── 5. Caps ──────────────────────────────────────────────────────────────

    #[test]
    fn fragment_content_truncated_at_cap() {
        let content = "x".repeat(100);
        let capped = cap_content(&content, 10, 2_000);
        assert!(capped.starts_with(&"x".repeat(10)));
        assert!(
            capped.contains("[smedja: truncated 90 bytes]"),
            "marker: {capped}"
        );

        // Line cap.
        let many = "a\n".repeat(50);
        let capped = cap_content(&many, 1_000_000, 5);
        assert_eq!(
            capped.matches('\n').count() - 1,
            5,
            "5 lines kept: {capped}"
        );
        assert!(
            capped.contains("[smedja: truncated"),
            "line marker: {capped}"
        );
    }

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

    #[test]
    fn env_overrides_fragment_cap() {
        // SAFETY: no other test reads `Caps::from_env`; the var is set and removed
        // within this single test, so there is no cross-test env race.
        unsafe { std::env::set_var("SMEDJA_FRAGMENT_MAX_BYTES", "42") };
        let caps = Caps::from_env();
        unsafe { std::env::remove_var("SMEDJA_FRAGMENT_MAX_BYTES") };
        assert_eq!(caps.per_fragment_bytes, 42);
    }
}
