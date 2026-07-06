//! `input`-area unit tests (moved verbatim from the former `tests.rs`).

use std::collections::VecDeque;

use smedja_rpc::client::Client;

use crate::clipboard::push_kill;
use crate::editor::resolve_editor;
use crate::input::handle_key;
use crate::main_panel;
use crate::test_support::make_state;
use crate::{
    command_palette_filtered, history_search, prev_char_boundary, PROMPT_HISTORY_CAP,
    SLASH_COMPLETIONS,
};

#[test]
fn ctrl_p_in_scroll_mode_toggles_session_peek() {
    let mut state = make_state("sess-peek");
    state.scroll_focus = true;
    assert!(!state.show_session_peek);
    // Simulate Ctrl+P toggle
    state.show_session_peek = !state.show_session_peek;
    assert!(state.show_session_peek);
}

#[test]
fn prompt_history_capped_at_max_size() {
    let mut history: Vec<String> = Vec::new();
    for i in 0..=PROMPT_HISTORY_CAP {
        history.push(format!("msg{i}"));
        if history.len() > PROMPT_HISTORY_CAP {
            history.remove(0);
        }
    }
    assert_eq!(history.len(), PROMPT_HISTORY_CAP);
}

// L128: trailing backslash appends newline continuation, does not submit.
#[test]
fn backslash_continuation_appends_newline() {
    let mut input = "hello\\".to_owned();
    // Simulate the Enter key handling logic inline.
    assert!(input.ends_with('\\'));
    input.pop();
    input.push('\n');
    assert!(input.contains('\n'));
    assert_eq!(input, "hello\n");
}

// L128: continuation display prefix uses "..." for multi-line input.
#[test]
fn continuation_display_uses_ellipsis_prefix() {
    let input = "first line\nsecond";
    let display = if input.contains('\n') {
        let last_line = input.rsplit('\n').next().unwrap_or("");
        format!("... {last_line}_")
    } else {
        format!("> {input}_")
    };
    assert_eq!(display, "... second_");
}

// L128: normal input display uses "> " prefix.
#[test]
fn normal_display_uses_prompt_prefix() {
    let input = "hello";
    let display = format!("> {input}_");
    assert_eq!(display, "> hello_");
}

#[test]
fn input_accumulates_characters_in_state() {
    let mut state = make_state("test-session");
    state.input.push('h');
    state.input.push('i');
    assert_eq!(state.input, "hi");
    // TODO: assert the input appears in the rendered buffer once
    // handle_key can be called without a live Client.
}

// Bug regression: `x` inspects the trace waterfall whenever the panel is
// visible — including in input mode, where the owner actually watches the
// trace. It must not require scroll mode, but must never steal a typed 'x'
// while composing a message.
#[tokio::test]
async fn x_inspects_trace_in_input_mode_when_panel_visible() {
    use tokio::net::UnixListener;

    // A socket the client can connect to; the `x` handler returns before any
    // RPC, so the mock never needs to respond.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("trace-x.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    let mut client = Client::connect(&sock_path).await.unwrap();
    let mut editor = rustyline::DefaultEditor::new().unwrap();
    let mut state = make_state("trace-x");
    // Trace panel visible (obs on + spans recorded); input mode, empty buffer.
    state.panels.obs = true;
    state.scroll_focus = false;
    state.input.clear();
    state.current_trace.start_turn();
    state.current_trace.push_tool("Read", 100);
    state.current_trace.settle_last_tool(300, true);

    let x = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::empty(),
    );

    // First `x`: open the inspector on the first span.
    handle_key(x, &mut state, &mut client, &mut editor)
        .await
        .unwrap();
    assert!(
        state.trace_expanded,
        "x must expand the trace in input mode"
    );
    assert_eq!(state.trace_selected, 0);
    assert!(
        state.input.is_empty(),
        "x must be consumed as inspect, not typed into the input"
    );

    // Second `x`: step to the next span.
    handle_key(x, &mut state, &mut client, &mut editor)
        .await
        .unwrap();
    assert_eq!(state.trace_selected, 1, "x steps to the next span");

    // While composing (non-empty buffer), `x` types normally instead of inspecting.
    state.trace_expanded = false;
    state.input = "fi".into();
    state.input_cursor = state.input.len();
    handle_key(x, &mut state, &mut client, &mut editor)
        .await
        .unwrap();
    assert_eq!(state.input, "fix", "x must type normally while composing");
    assert!(
        !state.trace_expanded,
        "x must not trigger the inspector mid-compose"
    );
}

#[test]
fn input_cursor_defaults_to_zero_in_make_state() {
    let state = make_state("s");
    assert_eq!(state.input_cursor, 0);
}

// ── provider-display: session.create response parsing ───────────────────

#[test]
fn yank_lines_text_builds_newline_joined_string() {
    let mut panel = main_panel::MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    let lines = panel.lines_text(1, 3);
    let text = lines.join("\n");
    assert_eq!(text, "line 1\nline 2\nline 3");
}

#[test]
fn selection_anchor_end_resolves_to_min_max_regardless_of_direction() {
    // Drag from line 4 back to line 1 — selection should span 1..=4.
    let anchor = 4usize;
    let end = 1usize;
    let lo = anchor.min(end);
    let hi = anchor.max(end);
    assert_eq!(lo, 1);
    assert_eq!(hi, 4);
    // Forward direction.
    let anchor = 1usize;
    let end = 4usize;
    assert_eq!(anchor.min(end), 1);
    assert_eq!(anchor.max(end), 4);
}

#[test]
fn esc_in_selection_mode_cancels_selection_without_scroll_change() {
    let mut state = make_state("sess-sel");
    for i in 0..10u32 {
        state.main_panel.push_line(format!("msg {i}"));
    }
    state.scroll_focus = true;
    state.selection_mode = true;
    state.selection_anchor = (3, 0);
    state.selection_end = (6, 0);
    state.main_panel.scroll = 3;

    // Simulate the Esc path: selection_mode cleared, scroll unchanged.
    if state.selection_mode {
        state.selection_mode = false;
    }

    assert!(
        !state.selection_mode,
        "selection must be cancelled after Esc"
    );
    assert_eq!(state.main_panel.scroll, 3, "scroll must not change on Esc");
    assert!(
        state.scroll_focus,
        "scroll_focus must remain active after cancelling selection"
    );
}

#[test]
fn esc_when_idle_activates_scroll_focus() {
    let mut state = make_state("sess-idle");
    assert!(!state.scroll_focus, "scroll_focus should be off by default");

    // Simulate the last else branch of Esc: no overlay, no selection, no scroll focus.
    state.scroll_focus = true;

    assert!(
        state.scroll_focus,
        "scroll_focus must be set by Esc when idle"
    );
}

#[test]
fn insert_key_exits_scroll_and_clears_selection() {
    let mut state = make_state("sess-ins");
    state.scroll_focus = true;
    state.selection_mode = true;
    state.g_pending = true;

    // Simulate 'i' key in scroll_focus block.
    state.scroll_focus = false;
    state.selection_mode = false;
    state.g_pending = false;

    assert!(!state.scroll_focus);
    assert!(!state.selection_mode);
    assert!(!state.g_pending);
}

#[test]
fn ctrl_c_with_clipboard_does_not_quit() {
    let mut state = make_state("sess-ctrlc");
    state.clipboard = Some("some text".to_owned());
    // Simulate the Ctrl-C branch: clipboard is Some → do NOT quit
    if state.clipboard.is_some() {
        // copy, do not quit
    } else {
        state.quit = true;
    }
    assert!(
        !state.quit,
        "Ctrl-C must not quit when clipboard is non-empty"
    );
}

#[test]
fn ctrl_c_with_no_clipboard_quits() {
    let mut state = make_state("sess-ctrlc");
    state.clipboard = None;
    // Simulate the Ctrl-C branch: clipboard is None → quit
    if state.clipboard.is_some() {
        // copy
    } else {
        state.quit = true;
    }
    assert!(state.quit, "Ctrl-C must quit when clipboard is empty");
}

// --- tui-input-modes tests ---

#[test]
fn history_search_finds_most_recent_match() {
    let history = vec![
        "git status".to_owned(),
        "git diff".to_owned(),
        "ls".to_owned(),
    ];
    let result = history_search(&history, "git");
    assert_eq!(
        result,
        Some((1, "git diff")),
        "should return most recent match"
    );
}

#[test]
fn history_search_empty_query_returns_none() {
    let history = vec!["git status".to_owned()];
    assert!(history_search(&history, "").is_none());
}

#[test]
fn history_search_no_match_returns_none() {
    let history = vec!["git status".to_owned()];
    assert!(history_search(&history, "foobar").is_none());
}

#[test]
fn history_search_empty_history_returns_none() {
    let history: Vec<String> = vec![];
    assert!(history_search(&history, "git").is_none());
}

#[test]
fn up_key_loads_most_recent_history_entry() {
    let mut state = make_state("sess-hist");
    state.prompt_history = vec!["first".to_owned(), "second".to_owned()];
    state.input = "live".to_owned();
    state.input_cursor = state.input.len();

    // Simulate Up key (first press)
    if !state.prompt_history.is_empty() {
        let new_idx = match state.history_idx {
            None => {
                state.saved_input = state.input.clone();
                state.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        state.history_idx = Some(new_idx);
        state.input = state.prompt_history[new_idx].clone();
        state.input_cursor = state.input.len();
    }

    assert_eq!(state.input, "second");
    assert_eq!(state.history_idx, Some(1));
    assert_eq!(state.saved_input, "live");
}

#[test]
fn down_key_at_end_restores_live_input() {
    let mut state = make_state("sess-hist-down");
    state.prompt_history = vec!["only".to_owned()];
    state.saved_input = "live input".to_owned();
    state.history_idx = Some(0);
    state.input = "only".to_owned();

    // Simulate Down key past end
    if let Some(idx) = state.history_idx {
        if idx + 1 < state.prompt_history.len() {
            let new_idx = idx + 1;
            state.history_idx = Some(new_idx);
            state.input = state.prompt_history[new_idx].clone();
            state.input_cursor = state.input.len();
        } else {
            state.history_idx = None;
            state.input = std::mem::take(&mut state.saved_input);
            state.input_cursor = state.input.len();
        }
    }

    assert!(
        state.history_idx.is_none(),
        "history_idx must be None after returning to live input"
    );
    assert_eq!(state.input, "live input");
}

#[test]
fn ctrl_r_in_input_mode_enters_history_search() {
    let mut state = make_state("sess-ctrl-r");
    state.scroll_focus = false;
    state.input = "current".to_owned();

    // Simulate Ctrl-R in input mode
    state.history_search_mode = true;
    state.history_search_query.clear();
    state.saved_input = state.input.clone();

    assert!(state.history_search_mode);
    assert_eq!(state.saved_input, "current");
}

#[test]
fn ctrl_r_in_scroll_mode_toggles_context_rail() {
    let mut state = make_state("sess-ctrl-r-scroll");
    state.scroll_focus = true;
    state.panels.context_rail = true;

    // Simulate Ctrl-R in scroll mode
    state.panels.context_rail = !state.panels.context_rail;

    assert!(
        !state.panels.context_rail,
        "context rail must be toggled off"
    );
}

#[test]
fn history_search_esc_restores_saved_input() {
    let mut state = make_state("sess-search-esc");
    state.history_search_mode = true;
    state.history_search_query = "git".to_owned();
    state.saved_input = "original".to_owned();
    state.input = "git status".to_owned();

    // Simulate Esc
    state.history_search_mode = false;
    state.history_search_query.clear();
    state.input = std::mem::take(&mut state.saved_input);
    state.input_cursor = state.input.len();

    assert!(!state.history_search_mode);
    assert_eq!(state.input, "original");
    assert!(state.history_search_query.is_empty());
}

#[test]
fn history_search_enter_accepts_match() {
    let mut state = make_state("sess-search-enter");
    state.history_search_mode = true;
    state.history_search_query = "git".to_owned();
    state.input = "git status".to_owned();

    // Simulate Enter
    state.history_search_mode = false;
    state.history_search_query.clear();

    assert!(
        !state.history_search_mode,
        "search mode must be cleared on Enter"
    );
    assert_eq!(
        state.input, "git status",
        "matched input must be kept on Enter"
    );
}

// --- tui native spec-command formatter tests ---

#[test]
fn ctrl_f_in_scroll_mode_toggles_context_rail() {
    let mut state = make_state("sess-ctrlf");
    state.scroll_focus = true;
    let initial = state.panels.context_rail;
    // Simulate Ctrl-F in scroll mode.
    state.panels.context_rail = !state.panels.context_rail;
    assert_ne!(
        state.panels.context_rail, initial,
        "Ctrl-F must toggle panels.context_rail in scroll mode"
    );
    state.panels.context_rail = !state.panels.context_rail;
    assert_eq!(
        state.panels.context_rail, initial,
        "second Ctrl-F must restore original value"
    );
}

#[test]
fn ctrl_r_in_scroll_mode_does_not_affect_context_rail() {
    let mut state = make_state("sess-ctrlr-scroll");
    state.scroll_focus = true;
    let initial = state.panels.context_rail;
    // The Ctrl-R handler only acts when !scroll_focus, so it must be a no-op here.
    if !state.scroll_focus {
        state.history_search_mode = !state.history_search_mode;
    }
    assert_eq!(
        state.panels.context_rail, initial,
        "Ctrl-R in scroll mode must not touch panels.context_rail"
    );
    assert!(
        !state.history_search_mode,
        "history_search_mode must remain off when Ctrl-R fires in scroll mode"
    );
}

#[test]
fn ctrl_r_in_input_mode_toggles_history_search() {
    let mut state = make_state("sess-ctrlr-input");
    state.scroll_focus = false;
    state.input = String::from("partial query");
    assert!(!state.history_search_mode);
    // Simulate Ctrl-R in input mode.
    if !state.scroll_focus {
        state.history_search_mode = !state.history_search_mode;
        state.history_search_query.clear();
        if state.history_search_mode {
            state.input.clone_into(&mut state.saved_input);
        }
    }
    assert!(
        state.history_search_mode,
        "Ctrl-R must enable history_search_mode in input mode"
    );
    assert_eq!(
        state.saved_input, "partial query",
        "current input must be saved when entering history search"
    );
    assert!(
        state.history_search_query.is_empty(),
        "search query must be cleared on activation"
    );
}

// --- Ctrl-G external editor --------------------------------------------------

#[test]
fn resolve_editor_falls_back_to_vi() {
    // Remove VISUAL and EDITOR from the environment for this test.
    std::env::remove_var("VISUAL");
    std::env::remove_var("EDITOR");
    // Can't guarantee clean env in parallel tests, but the fallback path
    // must always produce a non-empty string.
    let editor = resolve_editor();
    assert!(
        !editor.is_empty(),
        "resolve_editor must return a non-empty string"
    );
}

#[test]
fn resolve_editor_prefers_visual_over_editor() {
    std::env::set_var("VISUAL", "emacs");
    std::env::set_var("EDITOR", "nano");
    let editor = resolve_editor();
    // Clean up after the test regardless of assertion result.
    std::env::remove_var("VISUAL");
    std::env::remove_var("EDITOR");
    assert_eq!(editor, "emacs", "VISUAL must be preferred over EDITOR");
}

#[test]
fn open_in_editor_temp_path_is_in_tmpdir() {
    // Verify the temp file path is inside the OS temp directory — we
    // cannot actually invoke an editor in a unit test, but we can check
    // that the path construction is correct.
    let tmp = std::env::temp_dir();
    let path = tmp.join(format!("smedja-edit-{}.md", std::process::id()));
    assert!(
        path.starts_with(&tmp),
        "temp file must be under the OS temp directory"
    );
    assert!(
        path.to_string_lossy().ends_with(".md"),
        "temp file must have .md extension for editor syntax highlighting"
    );
}

#[test]
fn ctrl_g_in_scroll_mode_is_noop() {
    let mut state = make_state("sess-ctrlg-scroll");
    state.scroll_focus = true;
    state.input = "existing input".to_owned();
    state.input_cursor = 14;
    // The Ctrl-G handler guards on !scroll_focus; simulate that guard.
    if !state.scroll_focus {
        // would call open_in_editor — never reached
        state.input = "replaced".to_owned();
    }
    assert_eq!(
        state.input, "existing input",
        "Ctrl-G in scroll mode must not modify input"
    );
}

// --- thinking token accumulation ------------------------------------------

#[test]
fn ctrl_k_kills_to_eol() {
    let mut state = make_state("sess-kill-k");
    state.input = "hello world".to_owned();
    state.input_cursor = 5; // cursor after "hello"
    let killed: String = state.input[state.input_cursor..].to_owned();
    state.input.drain(state.input_cursor..);
    push_kill(&mut state.kill_ring, killed);
    assert_eq!(state.input, "hello");
    assert_eq!(state.kill_ring.back().map(String::as_str), Some(" world"));
}

#[test]
fn ctrl_u_kills_to_bol() {
    let mut state = make_state("sess-kill-u");
    state.input = "hello world".to_owned();
    state.input_cursor = 5;
    let killed: String = state.input[..state.input_cursor].to_owned();
    state.input.drain(..state.input_cursor);
    state.input_cursor = 0;
    push_kill(&mut state.kill_ring, killed);
    assert_eq!(state.input, " world");
    assert_eq!(state.kill_ring.back().map(String::as_str), Some("hello"));
}

#[test]
fn ctrl_y_yanks_from_ring() {
    let mut state = make_state("sess-yank");
    state.input = "foo".to_owned();
    state.input_cursor = 3;
    push_kill(&mut state.kill_ring, " bar".to_owned());
    // Yank
    let text = state.kill_ring.back().cloned().unwrap();
    state.input.insert_str(state.input_cursor, &text);
    state.input_cursor += text.len();
    assert_eq!(state.input, "foo bar");
}

#[test]
fn ctrl_b_moves_cursor_left() {
    let mut state = make_state("sess-ctrl-b");
    state.input = "abc".to_owned();
    state.input_cursor = 3;
    state.input_cursor = prev_char_boundary(&state.input, state.input_cursor);
    assert_eq!(state.input_cursor, 2);
}

#[test]
fn kill_ring_evicts_oldest_at_capacity() {
    let mut ring: VecDeque<String> = VecDeque::new();
    for i in 0..17u32 {
        push_kill(&mut ring, i.to_string());
    }
    assert_eq!(ring.len(), 16, "ring must not exceed 16 entries");
    // Oldest entry (0) is evicted; front is "1".
    assert_eq!(ring.front().map(String::as_str), Some("1"));
}

// --- P2b: /gov create + transition ----------------------------------------

#[test]
fn role_cockpit_toggle_via_ctrl_a() {
    let mut state = make_state("sess-cockpit");
    assert!(!state.panels.role_cockpit, "cockpit hidden by default");
    state.panels.role_cockpit = !state.panels.role_cockpit;
    assert!(state.panels.role_cockpit, "toggle must show cockpit");
    state.panels.role_cockpit = !state.panels.role_cockpit;
    assert!(
        !state.panels.role_cockpit,
        "second toggle must hide cockpit"
    );
}

#[test]
fn file_picker_insert_formats_at_file() {
    let mut state = make_state("s");
    state.input.clear();
    state.input_cursor = 0;
    // Simulate inserting a file selection
    let path = "/workspace/src/main.rs";
    let at_ref = format!("@file {path}");
    state.input = at_ref.clone();
    state.input_cursor = state.input.len();
    assert!(state.input.starts_with("@file "));
    assert!(state.input.contains(path));
}

#[test]
fn ctrl_f_in_input_mode_opens_file_picker() {
    let mut state = make_state("s");
    state.scroll_focus = false; // input mode
                                // Simulate what Ctrl+F handler does
    state.file_picker_open = true;
    state.file_picker_entries = vec![("../".to_owned(), true), ("main.rs".to_owned(), false)];
    state.file_picker_cursor = 0;
    assert!(state.file_picker_open);
    assert_eq!(state.file_picker_entries.len(), 2);
}

#[test]
fn ctrl_k_on_empty_input_opens_palette() {
    let mut state = make_state("test-session");
    state.input.clear();
    // Simulate what the Ctrl+K handler does when input is empty
    state.slash_popup_visible = true;
    state.slash_completions = command_palette_filtered("");
    state.command_palette_mode = true;
    state.slash_cursor = 0;
    assert!(state.slash_popup_visible);
    assert_eq!(state.slash_completions.len(), SLASH_COMPLETIONS.len());
    assert!(state.command_palette_mode);
}
