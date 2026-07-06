//! `metrics`-area unit tests (moved verbatim from the former `tests.rs`).

use serde_json::json;

use crate::render::render;
use crate::test_support::make_state;
use crate::tool_call::tool_call_card;
use crate::{
    format_tool_detail, metrics_poll_due, metrics_rows_from_summary, metrics_view,
    toggle_metrics_view,
};

#[test]
fn format_tool_detail_pretty_prints_json_args() {
    let lines = format_tool_detail("Bash", r#"{"command":"ls -la","timeout":5}"#);
    let joined = lines.join("\n");
    assert!(joined.contains("tool: Bash"), "{joined}");
    assert!(joined.contains("\"command\""), "{joined}"); // pretty JSON
    assert!(joined.contains("ls -la"), "{joined}");
    assert!(joined.contains("Esc to close"), "{joined}");
    // Non-JSON falls back to raw.
    let raw = format_tool_detail("X", "not json");
    assert!(raw.join("\n").contains("not json"));
}

#[test]
fn tool_call_card_shows_glyph_label_and_summary() {
    let line = tool_call_card("Bash", "find . -type f", true, '\u{2713}');
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("execute"), "{text}"); // ACP kind label
    assert!(text.contains("find . -type f"), "{text}");
    assert!(text.contains('\u{2713}'), "{text}"); // status glyph present
                                                  // No raw JSON braces leak into the card.
    assert!(!text.contains('{'), "{text}");
}

// --- metrics-live-fetch: pure JSON→rows mapper ---

#[test]
fn metrics_rows_from_summary_folds_buckets_per_runner() {
    let resp = json!({
        "tier": "hourly",
        "buckets": [
            { "bucket_start": 0, "runner": "claude", "turns": 1,
              "input_tok": 100, "output_tok": 50, "cost_usd": 0.01, "error_count": 1 },
            { "bucket_start": 3_600_000_000_i64, "runner": "claude", "turns": 1,
              "input_tok": 200, "output_tok": 80, "cost_usd": 0.02, "error_count": 2 },
            { "bucket_start": 0, "runner": "local", "turns": 1,
              "input_tok": 480, "output_tok": 0, "cost_usd": 0.0, "error_count": 0 },
        ],
    });
    let rows = metrics_rows_from_summary(&resp);
    assert_eq!(rows.len(), 2, "one row per runner");
    // First-seen runner order: claude then local.
    assert_eq!(rows[0].runner, "claude");
    assert_eq!(
        rows[0].tokens,
        100 + 50 + 200 + 80,
        "tokens summed across buckets"
    );
    assert!((rows[0].cost_usd - 0.03).abs() < 1e-9, "cost accumulated");
    assert_eq!(rows[0].errors, 3, "errors accumulated");
    assert_eq!(rows[1].runner, "local");
    assert_eq!(rows[1].tokens, 480);
}

#[test]
fn metrics_rows_from_summary_empty_buckets_yields_no_rows() {
    let empty = json!({ "tier": "hourly", "buckets": [] });
    assert!(metrics_rows_from_summary(&empty).is_empty());
    let missing = json!({ "tier": "hourly" });
    assert!(metrics_rows_from_summary(&missing).is_empty());
}

#[test]
fn metrics_rows_from_summary_tolerates_missing_fields() {
    let resp = json!({
        "buckets": [
            { "runner": "claude" },
        ],
    });
    let rows = metrics_rows_from_summary(&resp);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].runner, "claude");
    assert_eq!(rows[0].tokens, 0);
    assert!((rows[0].cost_usd - 0.0).abs() < 1e-9);
    assert_eq!(rows[0].errors, 0);
}

// --- metrics-live-fetch: poll-due predicate ---

#[test]
fn metrics_poll_due_when_visible_and_unset_or_elapsed() {
    let now = std::time::Instant::now();
    // Visible and never polled → due.
    assert!(metrics_poll_due(true, None, now));
    // Visible and the interval has elapsed → due.
    let stale = now.checked_sub(std::time::Duration::from_secs(4)).unwrap();
    assert!(metrics_poll_due(true, Some(stale), now));
    // Visible but within the interval → not due.
    let fresh = now.checked_sub(std::time::Duration::from_secs(1)).unwrap();
    assert!(!metrics_poll_due(true, Some(fresh), now));
    // Hidden → never due, regardless of timing.
    assert!(!metrics_poll_due(false, None, now));
    assert!(!metrics_poll_due(false, Some(stale), now));
}

// --- metrics-live-fetch: toggle resets the poll cadence ---

#[test]
fn toggling_metrics_view_on_resets_last_metrics_poll() {
    let mut state = make_state("sess-metrics-toggle");
    state.last_metrics_poll = Some(std::time::Instant::now());
    assert!(!state.panels.metrics);
    // Toggle on → visible and the poll is cleared for an immediate fetch.
    toggle_metrics_view(&mut state);
    assert!(state.panels.metrics, "toggle makes the panel visible");
    assert!(
        state.last_metrics_poll.is_none(),
        "toggle-on clears last_metrics_poll for an immediate fetch"
    );
    // Toggle off → hidden again.
    toggle_metrics_view(&mut state);
    assert!(!state.panels.metrics, "second toggle hides the panel");
}

// --- metrics-live-fetch: live fetch populates/clears the snapshot ---

#[test]
fn live_metrics_response_populates_then_clears_snapshot() {
    let mut state = make_state("sess-metrics-populate");
    assert!(
        state.metrics_snapshot.is_empty(),
        "snapshot starts empty (the previously-blank panel)"
    );
    let resp = json!({
        "tier": "hourly",
        "buckets": [
            { "runner": "claude", "input_tok": 700, "output_tok": 80,
              "cost_usd": 0.06, "error_count": 1 },
        ],
    });
    state.metrics_snapshot = metrics_rows_from_summary(&resp);
    assert_eq!(
        state.metrics_snapshot.len(),
        1,
        "live fetch fills the snapshot"
    );
    assert_eq!(state.metrics_snapshot[0].runner, "claude");
    // An empty window replaces the snapshot wholesale — no stale rows.
    let empty = json!({ "tier": "hourly", "buckets": [] });
    state.metrics_snapshot = metrics_rows_from_summary(&empty);
    assert!(
        state.metrics_snapshot.is_empty(),
        "empty window clears the snapshot rather than leaving stale rows"
    );
}

// --- /review scope-flag parsing ---

#[test]
fn ctrl_t_toggles_metrics_view() {
    let mut state = make_state("sess-ctrl-t");
    assert!(!state.panels.metrics, "metrics view starts hidden");
    // Simulate Ctrl-T.
    state.panels.metrics = !state.panels.metrics;
    assert!(state.panels.metrics, "Ctrl-T must show metrics view");
    state.panels.metrics = !state.panels.metrics;
    assert!(!state.panels.metrics, "Ctrl-T again must hide it");
}

#[test]
fn metrics_view_panel_renders_per_runner_snapshot() {
    let mut state = make_state("sess-metrics-render");
    state.panels.metrics = true;
    state.metrics_snapshot = vec![
        metrics_view::MetricsRow {
            runner: "claude".into(),
            tokens: 780,
            cost_usd: 0.06,
            errors: 2,
        },
        metrics_view::MetricsRow {
            runner: "local".into(),
            tokens: 480,
            cost_usd: 0.0,
            errors: 0,
        },
    ];
    // MetricsView lives inside the context rail; rail needs width >= 100.
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(content.contains("claude"), "claude runner must render");
    assert!(content.contains("local"), "local runner must render");
    assert!(content.contains("$0.0600"), "claude cost must render");
    assert!(content.contains("780"), "claude tokens must render");
}
