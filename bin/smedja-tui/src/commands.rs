use serde_json::json;

/// Available slash-command completions shown in the popup.
/// Short descriptions shown in the command palette (Ctrl+K). Order matches `SLASH_COMPLETIONS`.
pub(crate) const SLASH_COMMAND_DESCRIPTIONS: &[(&str, &str)] = &[
    ("/agent", "run named agent"),
    ("/approve", "approve a cowork item"),
    ("/briefing", "show session briefing"),
    (
        "/capabilities",
        "list provider capabilities (thinking, subprocess, model)",
    ),
    ("/clear", "clear message display"),
    ("/cowork", "toggle cowork approval mode"),
    ("/drawio", "generate draw.io diagram"),
    ("/gov", "govctl artifacts"),
    ("/health", "check daemon connectivity"),
    ("/help", "show help"),
    ("/index", "build the code graph"),
    ("/login", "authenticate with runner"),
    ("/loop", "manage loop runs"),
    ("/lsp", "LSP status and diagnostics"),
    ("/memory", "list stored memory"),
    ("/metrics", "show token usage and cost"),
    ("/model", "show or set model"),
    ("/pptx", "generate PowerPoint"),
    ("/quit", "exit smedja-tui"),
    ("/quota", "show usage quota"),
    ("/resume", "resume a session"),
    ("/review", "send git diff for review"),
    ("/session", "manage sessions"),
    ("/skills", "list loaded skills"),
    ("/spec", "browse OpenSpec changes"),
    ("/switch", "switch active session"),
    ("/takeover", "take over agent output"),
    ("/test", "run test suite"),
    ("/tier", "show or set tier"),
    ("/tools", "list available tools"),
    ("/upgrade", "upgrade smedja"),
    ("/version", "show version"),
];

pub(crate) const SLASH_COMPLETIONS: &[&str] = &[
    "/agent",
    "/approve",
    "/briefing",
    "/capabilities",
    "/clear",
    "/cowork",
    "/drawio",
    "/gov",
    "/health",
    "/help",
    "/index",
    "/login",
    "/loop",
    "/lsp",
    "/memory",
    "/metrics",
    "/model",
    "/pptx",
    "/quit",
    "/quota",
    "/resume",
    "/review",
    "/session",
    "/skills",
    "/spec",
    "/switch",
    "/takeover",
    "/test",
    "/tier",
    "/tools",
    "/upgrade",
    "/version",
];

pub(crate) const HELP_TEXT: &str = "\
slash commands:
  /agent [id]        — run named agent (omit id to list available agents)
  /approve [id]      — approve a cowork item (omit id to list pending approvals)
  /briefing          — show session briefing
  /clear             — clear message display (keeps session data)
  /cowork on|off|status — toggle or query cowork approval mode
  /drawio <slug>     — generate draw.io diagram
  /gov [list|show <id>|create work-item|rfc|adr <title>|transition <id> <status>] — govctl artifacts
  /health            — check daemon connectivity
  /help              — show this message
  /login             — authenticate with runner
  /loop [status|list|create <goal>|cancel] — manage loop runs
  /index [path]      — build the code graph for the workspace (auto-injected into context)
  /lsp               — show LSP server status and diagnostic summary
  /memory [session]  — list stored memory (turn history); pass a session id to view another's
  /metrics           — show token usage and cost
  /model [name]      — show or set model (local runner: lists GPU fit / hot-swaps)
  /pptx <slug>       — generate PowerPoint
  /quit              — exit smedja-tui
  /quality           — trigger Tier-2 LLM quality review (Ctrl-Q hold for 500ms also fires this)
  /value             — print ROI report for the active openspec change
  /quota             — show usage quota
  /resume [id [turn]] — resume a session (omit id for interactive picker; turn rewinds)
  /review            — send git diff for review
  /spec              — browse OpenSpec changes
  /skills [add <dir>] — list skills (~/.claude/skills + .smedja/skills) or add a directory
  /switch [runner]   — switch AI runner (omit for interactive picker)
  /takeover <runner> — fork session to new runner
  /test              — run cargo test and show a pass/fail summary
  /tools             — list recent tool calls (right-click a tool card for full args)
  /tier <t>          — set tier (local|fast|deep)
  /version           — print current version and check for a newer release
  /upgrade           — download and install the latest release in-place

inline context fragments (expanded into your message before the turn runs):
  @file <path>       — inject a workspace file's contents (path stays inside the workspace)
  @git               — inject `git status --short` and `git diff HEAD`
  @branch            — inject the current branch and upstream
  @shell <cmd>       — inject a shell command's output (gated by cowork when enabled)

keybindings (input mode):
  Esc                — enter scroll/normal mode
  Enter              — submit the message
  Shift/Alt-Enter    — insert a newline (compose multi-line in place)
  Up / Ctrl-P        — browse history backwards
  Down / Ctrl-N      — browse history forwards
  Ctrl-R             — toggle reverse history search
  Ctrl-G             — open $EDITOR / $VISUAL to compose a multi-line message
  Ctrl-B             — move cursor left one character
  Ctrl-K             — kill from cursor to end of line (push to kill ring)
  Ctrl-U             — kill from start of line to cursor (push to kill ring)
  Ctrl-Y             — yank most recent kill at cursor

keybindings (scroll/normal mode):
  i / a              — return to input mode
  j / k              — scroll down / up
  G                  — scroll to bottom
  gg                 — scroll to top
  Ctrl-A             — toggle role cockpit panel (active role/tier/turn status)
  Ctrl-F             — toggle context rail
  Ctrl-L             — toggle LSP diagnostic panel
  Ctrl-O             — toggle observability panel
  Ctrl-Q             — toggle quality gate panel
  Ctrl-V             — toggle value / ROI panel (Ctrl-V in input mode pastes)
  Ctrl-W             — toggle session browser (left rail)
  Alt+↑ / Alt+↓     — move cursor up / down in session rail (input mode)
  [ / ]              — move cursor up / down in session rail (scroll mode)
  mouse drag         — mark lines in the messages panel; release copies them
  v                  — start line selection (visual mode)
  y                  — yank selection to clipboard
  t                  — copy traceparent
  T                  — expand / collapse thinking block (when model emits thinking tokens)
  /                  — search panel text (type to filter, Esc to clear)
  Esc                — exit selection / return to input

note: scroll wheel scrolls the main panel; drag the mouse over messages to mark
      and copy, or use v/y in scroll mode. Long messages wrap to the panel width.";

/// Returns completions from `SLASH_COMPLETIONS` whose prefix matches `input`.
pub(crate) fn filtered_completions(input: &str) -> Vec<String> {
    SLASH_COMPLETIONS
        .iter()
        .copied()
        .filter(|c| c.starts_with(input))
        .map(str::to_owned)
        .collect()
}

/// Returns all slash commands whose name contains `query` as a substring (case-insensitive).
/// An empty query returns every command.
pub(crate) fn command_palette_filtered(query: &str) -> Vec<String> {
    let q = query.to_ascii_lowercase();
    SLASH_COMPLETIONS
        .iter()
        .copied()
        .filter(|c| q.is_empty() || c.to_ascii_lowercase().contains(&q))
        .map(str::to_owned)
        .collect()
}

/// Parses `/review` argument flags into the `audit.run` RPC scope params.
///
/// No args → working-tree diff (`{}`); `<path>` → `{ "path": <path> }`;
/// `--branch <base>` → `{ "branch": <base> }`; `--pr <ref>` → `{ "pr": <ref> }`.
/// Unknown leading tokens are treated as a path argument.
pub(crate) fn parse_review_scope(args: &str) -> serde_json::Value {
    let args = args.trim();
    if args.is_empty() {
        return json!({});
    }
    let mut parts = args.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    match head {
        "--branch" => json!({ "branch": rest }),
        "--pr" => json!({ "pr": rest }),
        "--diff" => json!({ "diff": true }),
        path => json!({ "path": path }),
    }
}

/// Renders a per-severity findings summary plus the report location.
///
/// `counts` is the `audit.run` response's `counts` object; `report_path` is the
/// written path when present, otherwise the report was returned inline.
pub(crate) fn render_findings_summary(
    counts: &serde_json::Value,
    report_path: Option<&str>,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("audit complete — findings:");
    for severity in ["critical", "high", "medium", "low", "info"] {
        let n = counts
            .get(severity)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let _ = write!(out, " {severity}={n}");
    }
    match report_path {
        Some(path) => {
            let _ = write!(out, "\nreport: {path}");
        }
        None => out.push_str("\nreport: (inline)"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn review_no_args_is_diff_scope() {
        let params = parse_review_scope("");
        assert_eq!(params, json!({}), "no args → working-tree diff");
    }

    #[test]
    fn review_path_arg_is_path_scope() {
        assert_eq!(
            parse_review_scope("src/lib.rs"),
            json!({ "path": "src/lib.rs" })
        );
    }

    #[test]
    fn review_branch_flag_is_branch_scope() {
        assert_eq!(
            parse_review_scope("--branch main"),
            json!({ "branch": "main" })
        );
    }

    #[test]
    fn review_pr_flag_is_pr_scope() {
        assert_eq!(parse_review_scope("--pr 42"), json!({ "pr": "42" }));
    }

    #[test]
    fn findings_summary_lists_counts_and_report_path() {
        let counts = json!({ "critical": 1, "high": 0, "medium": 2, "low": 3, "info": 0 });
        let summary = render_findings_summary(&counts, Some("/tmp/report.md"));
        assert!(summary.contains("critical=1"), "got: {summary}");
        assert!(summary.contains("medium=2"), "got: {summary}");
        assert!(summary.contains("low=3"), "got: {summary}");
        assert!(summary.contains("report: /tmp/report.md"), "got: {summary}");
    }

    #[test]
    fn findings_summary_marks_inline_when_no_path() {
        let counts = json!({ "critical": 0, "high": 0, "medium": 0, "low": 0, "info": 0 });
        let summary = render_findings_summary(&counts, None);
        assert!(summary.contains("report: (inline)"), "got: {summary}");
    }

    #[test]
    fn slash_completions_filter_by_prefix() {
        let completions = filtered_completions("/bri");
        assert_eq!(completions, vec!["/briefing".to_owned()]);
    }

    #[test]
    fn slash_completions_all_on_bare_slash() {
        let completions = filtered_completions("/");
        assert_eq!(completions.len(), SLASH_COMPLETIONS.len());
    }

    #[test]
    fn slash_completions_empty_for_no_match() {
        let completions = filtered_completions("/zzz");
        assert!(completions.is_empty());
    }

    #[test]
    fn slash_completions_include_new_commands() {
        let required = [
            "/agent",
            "/approve",
            "/briefing",
            "/login",
            "/metrics",
            "/model",
            "/quota",
            "/review",
            "/switch",
            "/takeover",
        ];
        for cmd in required {
            assert!(
                SLASH_COMPLETIONS.contains(&cmd),
                "{cmd} must be in SLASH_COMPLETIONS"
            );
        }
    }

    #[test]
    fn slash_completions_switch_matches_sw_prefix() {
        let completions = filtered_completions("/sw");
        assert!(
            completions.contains(&"/switch".to_owned()),
            "/switch must match '/sw' prefix; got: {completions:?}"
        );
    }

    #[test]
    fn slash_completions_takeover_matches_tak_prefix() {
        let completions = filtered_completions("/tak");
        assert!(
            completions.contains(&"/takeover".to_owned()),
            "/takeover must match '/tak' prefix; got: {completions:?}"
        );
    }

    #[test]
    fn resume_in_slash_completions() {
        assert!(
            SLASH_COMPLETIONS.contains(&"/resume"),
            "/resume must be in SLASH_COMPLETIONS"
        );
        let completions = filtered_completions("/res");
        assert!(
            completions.contains(&"/resume".to_owned()),
            "/resume must match '/res' prefix; got: {completions:?}"
        );
    }

    #[test]
    fn help_text_mentions_resume() {
        assert!(HELP_TEXT.contains("/resume"), "help must document /resume");
    }

    #[test]
    fn slash_health_in_completions() {
        // /health must appear in completions when user types "/h"
        let completions = filtered_completions("/h");
        assert!(
            completions.contains(&"/health".to_owned()),
            "/health must be in SLASH_COMPLETIONS and match '/h' prefix"
        );
    }

    #[test]
    fn slash_completions_includes_drawio_and_pptx() {
        assert!(
            SLASH_COMPLETIONS.contains(&"/drawio"),
            "/drawio must be in SLASH_COMPLETIONS"
        );
        assert!(
            SLASH_COMPLETIONS.contains(&"/pptx"),
            "/pptx must be in SLASH_COMPLETIONS"
        );
    }

    #[test]
    fn slash_completions_sorted_alphabetically() {
        let mut sorted = SLASH_COMPLETIONS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            SLASH_COMPLETIONS.to_vec(),
            sorted,
            "SLASH_COMPLETIONS must be in alphabetical order"
        );
    }

    #[test]
    fn help_text_covers_all_major_commands() {
        for cmd in [
            "/switch",
            "/health",
            "/tier",
            "/agent",
            "/briefing",
            "/clear",
        ] {
            assert!(HELP_TEXT.contains(cmd), "HELP_TEXT must mention {cmd}");
        }
    }

    #[test]
    fn slash_completions_include_help_and_clear() {
        assert!(
            SLASH_COMPLETIONS.contains(&"/help"),
            "/help must be in SLASH_COMPLETIONS"
        );
        assert!(
            SLASH_COMPLETIONS.contains(&"/clear"),
            "/clear must be in SLASH_COMPLETIONS"
        );
    }

    #[test]
    fn command_palette_empty_query_returns_all_commands() {
        let completions = command_palette_filtered("");
        assert_eq!(completions.len(), SLASH_COMPLETIONS.len());
    }

    #[test]
    fn command_palette_filters_by_substring() {
        // "model" matches "/model" and substring of other commands that contain "model"
        let completions = command_palette_filtered("mod");
        assert!(
            completions.contains(&"/model".to_owned()),
            "expected /model in results"
        );
    }

    #[test]
    fn command_palette_no_match_returns_empty() {
        let completions = command_palette_filtered("zzznomatch");
        assert!(completions.is_empty());
    }
}
