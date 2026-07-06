//! `statusbar`-area unit tests (moved verbatim from the former `tests.rs`).

use crate::statusbar::ModuleCtx;
use crate::test_support::{make_state, render_frame};
use crate::{
    format_capabilities_table, runner_is_subprocess, runner_supports_thinking, status_bar_line,
    status_hint_line,
};

#[test]
fn status_bar_line_segments_runner_tier_session() {
    let ctx = ModuleCtx {
        session_id: "abcd1234ef",
        mode: Some("impl"),
        tier: Some("deep"),
        runner: Some("claude-cli"),
        pending: false,
        input_mode: true,
        ctx_pct: None,
    };
    let text: String = status_bar_line(&ctx, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("INSERT"), "{text}");
    assert!(text.contains("CLAUDE"), "{text}"); // runner_label uppercases
    assert!(text.contains("deep"), "{text}");
    assert!(text.contains("abcd1234"), "{text}"); // 8-char session id
}

#[test]
fn status_bar_shows_ctx_pct_when_nonzero() {
    let ctx = ModuleCtx {
        session_id: "abc",
        mode: None,
        tier: None,
        runner: None,
        pending: false,
        input_mode: true,
        ctx_pct: Some(61),
    };
    let text: String = status_bar_line(&ctx, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("61%"), "ctx gauge must appear: {text}");
}

#[test]
fn status_bar_omits_ctx_gauge_when_none() {
    let ctx = ModuleCtx {
        session_id: "abc",
        mode: None,
        tier: None,
        runner: None,
        pending: false,
        input_mode: true,
        ctx_pct: None,
    };
    let text: String = status_bar_line(&ctx, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(!text.contains('%'), "no gauge when ctx_pct is None: {text}");
}

#[test]
fn runner_capability_flags_for_known_runners() {
    assert!(runner_supports_thinking("anthropic"));
    assert!(!runner_supports_thinking("claude-cli"));
    assert!(!runner_supports_thinking("openai"));
    assert!(runner_is_subprocess("claude-cli"));
    assert!(runner_is_subprocess("codex-cli"));
    assert!(!runner_is_subprocess("anthropic"));
}

#[test]
fn format_capabilities_table_lists_runners() {
    let runners = vec![
        serde_json::json!({ "runner": "anthropic", "tier": "fast", "model": "claude-haiku-4-5-20251001" }),
        serde_json::json!({ "runner": "claude-cli", "tier": "fast", "model": "claude-opus" }),
    ];
    let table = format_capabilities_table(&runners);
    assert!(table.contains("anthropic"), "{table}");
    assert!(table.contains("thinking"), "{table}");
    assert!(table.contains("subprocess"), "{table}");
}

#[test]
fn status_hint_advertises_real_entry_points() {
    let text: String = status_hint_line(true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("/help"), "{text}");
    assert!(text.contains("^W"), "{text}");
}

#[test]
fn status_bar_shows_tier_when_set() {
    let mut state = make_state("sess-xyz");
    state.tier = Some("fast".into());
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(content.contains("fast"), "status bar must render the tier");
}

#[test]
fn status_bar_shows_unknown_when_no_tier() {
    let mut state = make_state("sess-xyz");
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!content.trim().is_empty());
}

// --- thinking indicator tests ---

#[test]
fn status_bar_shows_runner_when_set() {
    let mut state = make_state("sess-runner");
    state.runner = "anthropic".to_owned();
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("ANTHROPIC"),
        "status bar must render the runner label; got: {content}"
    );
}

// ── tui-message-selection: T6 tests ─────────────────────────────────────

#[test]
fn status_bar_shows_input_mode_badge_when_not_scroll() {
    let mut state = make_state("sess-mode");
    state.scroll_focus = false;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("INSERT"),
        "status bar must show INSERT when scroll_focus=false; got: {content}"
    );
}

#[test]
fn status_bar_shows_normal_mode_badge_when_scroll() {
    let mut state = make_state("sess-mode");
    state.scroll_focus = true;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("SCROLL"),
        "status bar must show SCROLL when scroll_focus=true; got: {content}"
    );
}

// --- tui-prompt-history tests ---
