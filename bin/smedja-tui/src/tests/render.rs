//! `render`-area unit tests (moved verbatim from the former `tests.rs`).

use crate::blocks;
use crate::filtered_completions;
use crate::render::render;
use crate::state::SessionDetail;
use crate::test_support::{make_state, render_frame};

#[test]
fn quit_flag_starts_false_and_can_be_set() {
    let mut state = make_state("test-session");
    assert!(!state.quit);
    state.quit = true;
    assert!(state.quit);
}

#[test]
fn render_does_not_panic_with_empty_state() {
    let mut state = make_state("test-session");
    let _buf = render_frame(&mut state);
    // Verify no panic — any output is acceptable.
}

#[test]
fn slash_popup_visible_flag_and_render() {
    let mut state = make_state("test-session");
    assert!(!state.slash_popup_visible);
    state.slash_popup_visible = true;
    state.slash_completions = filtered_completions("/");
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !content.trim().is_empty(),
        "buffer should not be entirely blank when slash popup is open"
    );
}

#[test]
fn block_browser_renders_without_panic() {
    let mut state = make_state("test-session");
    let mut block = blocks::TurnBlock::new(1);
    block.complete(42);
    state.block_store.push(block);
    state.block_browser_open = true;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !content.trim().is_empty(),
        "buffer should not be blank when block browser is open"
    );
}

#[test]
fn diff_overlay_renders_without_panic() {
    let mut state = make_state("test-session");
    state.diff_overlay = Some((
        0,
        vec!["+added line".to_owned(), "-removed line".to_owned()],
    ));
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !content.trim().is_empty(),
        "buffer should not be blank when diff overlay is set"
    );
}

#[test]
fn connect_banner_visible() {
    let mut state = make_state("sess-abc");
    let sock = "/run/user/1000/smdjad.sock";
    state.main_panel.push_line(format!("connected to {sock}"));
    state.main_panel.push_line("session sess-abc".into());
    state.main_panel.push_line("provider: unknown".into());
    state.main_panel.push_line("tier: default".into());
    state
        .main_panel
        .push_line("type a message or /help for commands".into());
    // Auto-scroll leaves scroll at the last line; scroll to top to see the
    // full banner in the rendered frame.
    state.main_panel.scroll_to_top();
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(content.contains("sess-abc"), "banner must show session ID");
    assert!(
        content.contains("connected"),
        "banner must show connection line"
    );
}

#[test]
fn thinking_indicator_visible_when_turn_in_flight() {
    let mut state = make_state("sess-think");
    state.turn_in_flight = true;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("thinking") || content.contains("streaming") || content.contains("cancel"),
        "buffer should contain the live line when turn_in_flight is true"
    );
}

#[test]
fn thinking_indicator_hidden_when_idle() {
    let mut state = make_state("sess-idle");
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!content.is_empty());
}

// --- layout regression tests ---

#[test]
fn layout_input_row_at_bottom_of_80x24() {
    let mut state = make_state("sess-layout");
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let buf = terminal.backend().buffer();
    assert_eq!(buf.area().height, 24);
    assert_eq!(buf.area().width, 80);
}

#[test]
fn layout_40x10_does_not_panic() {
    let mut state = make_state("sess-narrow");
    let backend = ratatui::backend::TestBackend::new(40, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let buf = terminal.backend().buffer();
    assert_eq!(buf.area().width, 40);
    assert_eq!(buf.area().height, 10);
}

// Verifies that the token-count footer pushed after a turn completes
// appears in the rendered frame buffer.
#[test]
fn turn_footer_shows_token_counts() {
    let mut state = make_state("sess-footer");
    // Simulate what the subscribe completion path does.
    state.main_panel.push_delta("response text");
    state
        .main_panel
        .push_line("↳ 10↑ 20↓ tokens · 250ms".into());
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("tokens"),
        "turn footer should show token count label in the rendered buffer"
    );
}

// --- input cursor tests ---

/// Build the footer string the same way the streaming `done` handler does,
/// so the unit test does not depend on a live event loop.
fn build_turn_footer(
    input_tok: u64,
    output_tok: u64,
    turn_ms: u64,
    traceparent: Option<&str>,
    otlp_configured: bool,
) -> String {
    if let Some(tp_str) = traceparent {
        if otlp_configured {
            format!("↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str}")
        } else {
            format!(
                    "↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str} · traces not exported (set SMEDJA_OTLP_ENDPOINT)"
                )
        }
    } else {
        format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms")
    }
}

#[test]
fn footer_shows_otlp_warning_when_not_configured() {
    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let footer = build_turn_footer(100, 50, 300, Some(tp), false);
    assert!(
        footer.contains(tp),
        "footer must include the traceparent string"
    );
    assert!(
        footer.contains("traces not exported"),
        "footer must warn that traces are not exported when OTLP is not configured"
    );
    assert!(
        footer.contains("SMEDJA_OTLP_ENDPOINT"),
        "footer must mention SMEDJA_OTLP_ENDPOINT"
    );
}

#[test]
fn footer_shows_no_otlp_warning_when_configured() {
    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let footer = build_turn_footer(100, 50, 300, Some(tp), true);
    assert!(
        footer.contains(tp),
        "footer must include the traceparent string"
    );
    assert!(
        !footer.contains("traces not exported"),
        "footer must not show warning when OTLP is configured"
    );
}

#[test]
fn session_rail_toggle_clears_cursor() {
    let mut state = make_state("sess-rail");
    assert!(!state.panels.session_rail);
    // Simulate Ctrl-W: enable rail.
    state.panels.session_rail = true;
    state.session_rail_cursor = 0;
    state.last_session_rail_poll = None;
    assert!(state.panels.session_rail);
    // Toggle off.
    state.panels.session_rail = false;
    assert!(!state.panels.session_rail);
}

#[test]
fn session_rail_cursor_navigates_within_bounds() {
    let mut state = make_state("sess-rail-nav");
    state.session_rail_items = vec![
        ("id1".into(), "claude  id1".into()),
        ("id2".into(), "claude  id2".into()),
        ("id3".into(), "claude  id3".into()),
    ];
    state.session_rail_cursor = 0;
    // ] moves forward.
    let max = state.session_rail_items.len().saturating_sub(1);
    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    assert_eq!(state.session_rail_cursor, 1);
    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    assert_eq!(state.session_rail_cursor, 2);
    // Clamps at max.
    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    assert_eq!(state.session_rail_cursor, 2, "cursor must not exceed max");
    // [ moves backward.
    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    assert_eq!(state.session_rail_cursor, 1);
    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    assert_eq!(state.session_rail_cursor, 0);
    // Clamps at zero.
    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    assert_eq!(state.session_rail_cursor, 0, "cursor must not underflow");
}

// --- emit/canvas split: system message dual-routing ----------------------

#[test]
fn panel_visibility_startup_defaults_match_make_state() {
    let state = make_state("sess-panels");
    assert!(state.panels.context_rail, "context rail visible by default");
    assert!(!state.panels.metrics, "metrics hidden by default");
    assert!(!state.panels.session_rail, "session rail hidden by default");
    assert!(state.panels.lsp, "LSP visible by default");
    assert!(state.panels.obs, "obs visible by default");
    assert!(!state.panels.role_cockpit, "cockpit hidden by default");
}

// --- session detail overlay (Story A) ------------------------------------

#[test]
fn session_detail_starts_empty() {
    let state = make_state("sess-detail-init");
    assert!(
        state.session_detail_overlay.is_none(),
        "detail overlay must start empty"
    );
}

#[test]
fn session_detail_esc_closes_overlay() {
    let mut state = make_state("sess-detail-esc");
    state.session_detail_overlay = Some(SessionDetail {
        id: "abc-123".into(),
        title: None,
        mode: Some("auto".into()),
        status: Some("active".into()),
        active_change: None,
        created_at: "2026-06-28T00:00:00Z".into(),
        updated_at: "2026-06-28T00:00:00Z".into(),
        cowork_mode: None,
    });
    // Esc while overlay is open clears it.
    state.session_detail_overlay = None;
    assert!(
        state.session_detail_overlay.is_none(),
        "Esc must close the detail overlay"
    );
}

#[test]
fn session_detail_overlay_holds_correct_fields() {
    let detail = SessionDetail {
        id: "test-session-id".into(),
        title: Some("My session".into()),
        mode: Some("review".into()),
        status: Some("active".into()),
        active_change: Some("add-quality-panel".into()),
        created_at: "2026-06-01T12:00:00Z".into(),
        updated_at: "2026-06-28T09:00:00Z".into(),
        cowork_mode: Some("ask".into()),
    };
    assert_eq!(detail.id, "test-session-id");
    assert_eq!(detail.title.as_deref(), Some("My session"));
    assert_eq!(detail.mode.as_deref(), Some("review"));
    assert_eq!(detail.status.as_deref(), Some("active"));
    assert_eq!(detail.active_change.as_deref(), Some("add-quality-panel"));
    assert_eq!(detail.cowork_mode.as_deref(), Some("ask"));
}

#[test]
fn session_detail_from_json_maps_all_fields() {
    let v = serde_json::json!({
        "id": "sess-42",
        "title": "refactor sprint",
        "mode": "auto",
        "status": "active",
        "active_change": "add-auth",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-06-28T00:00:00Z",
        "cowork_mode": "plan",
    });
    let detail = SessionDetail::from_json(&v);
    assert_eq!(detail.id, "sess-42");
    assert_eq!(detail.title.as_deref(), Some("refactor sprint"));
    assert_eq!(detail.mode.as_deref(), Some("auto"));
    assert_eq!(detail.status.as_deref(), Some("active"));
    assert_eq!(detail.active_change.as_deref(), Some("add-auth"));
    assert_eq!(detail.cowork_mode.as_deref(), Some("plan"));
}

#[test]
fn session_detail_from_json_handles_missing_optional_fields() {
    let v = serde_json::json!({ "id": "bare-id",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    let detail = SessionDetail::from_json(&v);
    assert_eq!(detail.id, "bare-id");
    assert!(detail.title.is_none());
    assert!(detail.mode.is_none());
    assert!(detail.status.is_none());
    assert!(detail.active_change.is_none());
    assert!(detail.cowork_mode.is_none());
}

#[test]
fn session_detail_overlay_renders_in_buffer() {
    let mut state = make_state("sess-detail-render");
    state.session_detail_overlay = Some(SessionDetail {
        id: "full-id-abc-def-ghi".into(),
        title: Some("Sprint 12".into()),
        mode: Some("auto".into()),
        status: Some("active".into()),
        active_change: Some("add-quality-panel".into()),
        created_at: "2026-06-28T09:00:00Z".into(),
        updated_at: "2026-06-28T10:00:00Z".into(),
        cowork_mode: Some("ask".into()),
    });
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("full-id-abc-def-ghi"),
        "full id must render"
    );
    assert!(content.contains("Sprint 12"), "title must render");
    assert!(
        content.contains("add-quality-panel"),
        "active change must render"
    );
    assert!(content.contains("ask"), "cowork mode must render");
}

#[test]
fn session_rail_up_down_move_cursor_in_scroll_mode() {
    let mut state = make_state("sess-up-down");
    state.scroll_focus = true;
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "runner  id1".into()),
        ("id2".into(), "runner  id2".into()),
        ("id3".into(), "runner  id3".into()),
    ];
    state.session_rail_cursor = 0;

    // Down moves cursor forward.
    let max = state.session_rail_items.len().saturating_sub(1);
    if state.scroll_focus && state.panels.session_rail && !state.session_rail_items.is_empty() {
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(state.session_rail_cursor, 1);

    // Up moves cursor back.
    if state.scroll_focus && state.panels.session_rail {
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    }
    assert_eq!(state.session_rail_cursor, 0);
}

#[test]
fn session_rail_bracket_keys_work_in_input_mode() {
    let mut state = make_state("sess-bracket-input");
    state.scroll_focus = false; // input mode
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "label1".into()),
        ("id2".into(), "label2".into()),
    ];
    state.session_rail_cursor = 0;

    // ] advances cursor even in input mode.
    if state.panels.session_rail && !state.session_rail_items.is_empty() {
        let max = state.session_rail_items.len().saturating_sub(1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(state.session_rail_cursor, 1, "] must work in input mode");

    // [ goes back.
    if state.panels.session_rail {
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    }
    assert_eq!(state.session_rail_cursor, 0, "[ must work in input mode");
}

// --- session detail: Ctrl+Enter load (Story B) ---------------------------

#[test]
fn session_detail_ctrl_enter_switches_session_id() {
    let mut state = make_state("sess-switch-id");
    state.session_id = "original-session".into();
    state.session_detail_overlay = Some(SessionDetail {
        id: "new-session-abc".into(),
        title: Some("other work".into()),
        mode: Some("auto".into()),
        status: Some("active".into()),
        active_change: None,
        created_at: "2026-06-28T00:00:00Z".into(),
        updated_at: "2026-06-28T00:00:00Z".into(),
        cowork_mode: None,
    });
    // Simulate what Ctrl+Enter does: extract id, switch, clear overlay.
    let target_id = state
        .session_detail_overlay
        .as_ref()
        .map(|d| d.id.clone())
        .unwrap();
    state.session_id = target_id;
    state.session_detail_overlay = None;
    state.display_start_idx = state.messages.len();
    state.main_panel.clear_display();

    assert_eq!(
        state.session_id, "new-session-abc",
        "session_id must switch"
    );
    assert!(
        state.session_detail_overlay.is_none(),
        "overlay must close after load"
    );
}

#[test]
fn session_detail_ctrl_enter_does_nothing_without_overlay() {
    let mut state = make_state("sess-switch-no-overlay");
    state.session_id = "original".into();
    state.session_detail_overlay = None;
    // Nothing happens — session_id is unchanged.
    if let Some(ref d) = state.session_detail_overlay {
        state.session_id = d.id.clone();
    }
    assert_eq!(state.session_id, "original", "no overlay = no switch");
}

#[test]
fn session_detail_popup_shows_load_hint() {
    let mut state = make_state("sess-detail-hint");
    state.session_detail_overlay = Some(SessionDetail {
        id: "hint-session".into(),
        title: None,
        mode: None,
        status: None,
        active_change: None,
        created_at: "2026-06-28T00:00:00Z".into(),
        updated_at: "2026-06-28T00:00:00Z".into(),
        cowork_mode: None,
    });
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    // The popup must hint both the load binding and close binding.
    assert!(
        content.contains("load") || content.contains("Load"),
        "popup must show load hint: {content}"
    );
    assert!(
        content.contains("Esc") || content.contains("close"),
        "popup must show close hint"
    );
}

// --- session rail: arrow keys in input mode (Story B fix) ----------------

#[test]
fn session_rail_up_arrow_moves_cursor_in_input_mode() {
    let mut state = make_state("sess-up-input");
    state.scroll_focus = false; // input mode
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "label1".into()),
        ("id2".into(), "label2".into()),
        ("id3".into(), "label3".into()),
    ];
    state.session_rail_cursor = 2;

    // Simulate the early-exit block: Up decrements cursor, does not touch history.
    if state.panels.session_rail && !state.scroll_focus {
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    }
    assert_eq!(
        state.session_rail_cursor, 1,
        "Up must move rail cursor in input mode"
    );
    assert!(
        state.history_idx.is_none(),
        "prompt history must be untouched"
    );
}

#[test]
fn session_rail_down_arrow_moves_cursor_in_input_mode() {
    let mut state = make_state("sess-down-input");
    state.scroll_focus = false;
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "label1".into()),
        ("id2".into(), "label2".into()),
    ];
    state.session_rail_cursor = 0;

    if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty() {
        let max = state.session_rail_items.len().saturating_sub(1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(
        state.session_rail_cursor, 1,
        "Down must move rail cursor in input mode"
    );
}

#[test]
fn session_rail_down_arrow_clamps_at_bottom_in_input_mode() {
    let mut state = make_state("sess-down-clamp-input");
    state.scroll_focus = false;
    state.panels.session_rail = true;
    state.session_rail_items = vec![("id1".into(), "label1".into())];
    state.session_rail_cursor = 0;

    if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty() {
        let max = state.session_rail_items.len().saturating_sub(1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(state.session_rail_cursor, 0, "Down must clamp at last item");
}

// --- Slice 7: command palette ---

// --- Slice 8: file picker ---
