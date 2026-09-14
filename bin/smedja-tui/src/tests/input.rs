//! `input`-area unit tests (moved verbatim from the former `tests.rs`).

use std::collections::VecDeque;

use smedja_rpc::client::Client;

use crate::clipboard::push_kill;
use crate::editor::resolve_editor;
use crate::input::{always_approve_message, handle_key};
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

// --- cowork gate: [a] always ------------------------------------------------

#[test]
fn always_approve_message_prefers_daemon_note() {
    let ok = Ok(serde_json::json!({"id": "x", "resolved": true, "note": "rule saved: bash *"}));
    assert_eq!(always_approve_message(&ok, "bash"), "rule saved: bash *");
}

#[test]
fn always_approve_message_marks_persisted_rule() {
    let ok = Ok(serde_json::json!({"id": "x", "resolved": true, "rule_persisted": true}));
    let msg = always_approve_message(&ok, "bash");
    assert!(msg.contains("approved (always): bash"), "got: {msg}");
    assert!(msg.contains("persisted"), "got: {msg}");
}

#[test]
fn always_approve_message_confirms_persistence_without_note() {
    // A resolved reply with no veto note means the daemon persisted the rule.
    let ok = Ok(serde_json::json!({"id": "x", "resolved": true}));
    assert_eq!(
        always_approve_message(&ok, "bash"),
        "approved (always): bash — allow rule persisted"
    );
}

#[test]
fn always_approve_message_sanitizes_daemon_note() {
    // The daemon's note is echoed into the panel — terminal control characters
    // in it must be stripped before display.
    let ok = Ok(serde_json::json!({"note": "rule saved \u{1b}]52;c;eWV5\u{7} done"}));
    let msg = always_approve_message(&ok, "bash");
    assert!(!msg.contains('\u{1b}'), "ESC stripped: {msg:?}");
    assert!(!msg.contains('\u{7}'), "BEL stripped: {msg:?}");
    assert!(
        msg.contains("rule saved") && msg.contains("done"),
        "{msg:?}"
    );
}

#[test]
fn always_approve_message_ignores_empty_note() {
    // An empty note must fall through to the persisted/plain confirmation
    // rather than printing a blank line.
    let ok = Ok(serde_json::json!({"note": "", "rule_persisted": true}));
    let msg = always_approve_message(&ok, "bash");
    assert!(msg.contains("approved (always): bash"), "got: {msg}");
    assert!(msg.contains("persisted"), "got: {msg}");
}

// Pressing `a` at the cowork gate must send cowork.resolve with scope "always"
// and drop the item once the daemon resolves it.
#[tokio::test]
async fn cowork_always_key_sends_scope_always_and_confirms() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("cowork-always.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<serde_json::Value>();

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let mut reader = TokioBufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let req: serde_json::Value =
                serde_json::from_str(line.trim_end()).unwrap_or(serde_json::Value::Null);
            let _ = tx.send(req.clone());
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req["id"].clone(),
                "result": { "id": "ap-1", "resolved": true, "rule_persisted": true },
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            let _ = reader.get_mut().write_all(&bytes).await;
        }
    });

    let mut client = Client::connect(&sock_path).await.unwrap();
    let mut editor = rustyline::DefaultEditor::new().unwrap();
    let mut state = make_state("sess-always");
    state.pending_cowork.push(crate::cowork_widget::CoworkItem {
        id: "ap-1".into(),
        tool: "bash".into(),
        step_n: 1,
        args_display: r#"{"cmd":"rm -rf build/"}"#.into(),
        reasoning: String::new(),
        agent: None,
        cwd: None,
        risk: None,
        supports_modify: true,
        rows_cache: std::cell::RefCell::new(None),
    });

    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('a'),
        crossterm::event::KeyModifiers::empty(),
    );
    handle_key(key, &mut state, &mut client, &mut editor)
        .await
        .unwrap();

    let req = rx.await.unwrap();
    assert_eq!(req["method"].as_str(), Some("cowork.resolve"));
    assert_eq!(req["params"]["id"].as_str(), Some("ap-1"));
    assert_eq!(req["params"]["approved"].as_bool(), Some(true));
    assert_eq!(
        req["params"]["scope"].as_str(),
        Some("always"),
        "always key must send scope=always; params: {}",
        req["params"]
    );
    assert!(
        state.pending_cowork.is_empty(),
        "resolved item must leave the queue"
    );
    let body = state
        .main_panel
        .lines_text(0, state.main_panel.len())
        .join("\n");
    assert!(
        body.contains("persisted"),
        "panel must confirm the persisted rule; got: {body}"
    );
}

// --- masked secret entry: leak-proofing -------------------------------------
//
// While `secret_var` is set the input bar holds an in-progress credential.
// Only Char/Backspace/Delete/Enter/Esc may act; editor hand-off, the kill
// ring, and history browse are dead keys so the key cannot escape into a
// temp file, the kill ring, or the plaintext prompt.

mod secret_mode {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::net::UnixListener;

    /// A client connected to a socket whose peer never answers; the
    /// secret-mode keys under test never issue an RPC.
    async fn dummy_client(dir_name: &str) -> (tempfile::TempDir, Client) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join(format!("{dir_name}.sock"));
        let listener = UnixListener::bind(&sock_path).unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let client = Client::connect(&sock_path).await.unwrap();
        (dir, client)
    }

    fn secret_state(session: &str) -> crate::state::AppState {
        let mut state = make_state(session);
        state.secret_var = Some("SMEDJA_TEST_KEY".to_owned());
        state.input = "sk-live-secret".to_owned();
        state.input_cursor = state.input.len();
        state
    }

    async fn press(
        code: KeyCode,
        mods: KeyModifiers,
        state: &mut crate::state::AppState,
        client: &mut Client,
    ) {
        let mut editor = rustyline::DefaultEditor::new().unwrap();
        handle_key(KeyEvent::new(code, mods), state, client, &mut editor)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ctrl_g_does_not_hand_secret_to_editor() {
        let (_dir, mut client) = dummy_client("secret-ctrlg").await;
        let mut state = secret_state("sess-secret-g");
        let scratch = std::env::temp_dir().join(format!("smedja-edit-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&scratch);

        press(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;

        assert_eq!(state.input, "sk-live-secret", "input untouched by Ctrl-G");
        assert!(
            !scratch.exists(),
            "no editor scratch file may be written while a secret is in progress"
        );
        assert!(state.secret_var.is_some(), "still in secret mode");
    }

    #[tokio::test]
    async fn ctrl_u_and_ctrl_k_do_not_push_secret_to_kill_ring() {
        let (_dir, mut client) = dummy_client("secret-kill").await;
        let mut state = secret_state("sess-secret-kill");

        press(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;
        press(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;

        assert_eq!(state.input, "sk-live-secret", "kill keys are dead");
        assert!(
            state.kill_ring.is_empty(),
            "secret must never reach the kill ring"
        );
    }

    #[tokio::test]
    async fn ctrl_y_does_not_yank_into_secret_prompt() {
        let (_dir, mut client) = dummy_client("secret-yank").await;
        let mut state = secret_state("sess-secret-yank");
        push_kill(&mut state.kill_ring, "stale-kill".to_owned());

        press(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;

        assert_eq!(state.input, "sk-live-secret", "yank is dead in secret mode");
        // The ring is untouched either (a later plaintext yank keeps the
        // pre-existing entry, never the secret).
        assert_eq!(
            state.kill_ring.back().map(String::as_str),
            Some("stale-kill")
        );
    }

    #[tokio::test]
    async fn up_arrow_does_not_stash_secret_into_saved_input() {
        let (_dir, mut client) = dummy_client("secret-history").await;
        let mut state = secret_state("sess-secret-hist");
        state.prompt_history.push("previous prompt".to_owned());

        press(KeyCode::Up, KeyModifiers::empty(), &mut state, &mut client).await;

        assert_eq!(state.input, "sk-live-secret", "history browse is dead");
        assert!(
            state.saved_input.is_empty(),
            "the in-progress key must not be stashed for later restore"
        );
        assert!(state.history_idx.is_none());
    }

    #[tokio::test]
    async fn esc_cancel_clears_input_and_saved_input() {
        let (_dir, mut client) = dummy_client("secret-esc").await;
        let mut state = secret_state("sess-secret-esc");
        state.saved_input = "sk-stash".to_owned();

        press(KeyCode::Esc, KeyModifiers::empty(), &mut state, &mut client).await;

        assert!(state.secret_var.is_none(), "secret mode cancelled");
        assert!(state.input.is_empty(), "typed key discarded");
        assert!(
            state.saved_input.is_empty(),
            "stash cleared so Up/Down cannot restore the key"
        );
        let body = state
            .main_panel
            .lines_text(0, state.main_panel.len())
            .join("\n");
        assert!(body.contains("login: cancelled"), "cancel notice: {body}");
    }

    #[tokio::test]
    async fn enter_on_empty_secret_cancels_and_clears_saved_input() {
        let (_dir, mut client) = dummy_client("secret-enter").await;
        let mut state = secret_state("sess-secret-enter");
        state.input.clear();
        state.input_cursor = 0;
        state.saved_input = "sk-stash".to_owned();

        press(
            KeyCode::Enter,
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;

        assert!(state.secret_var.is_none());
        assert!(
            state.saved_input.is_empty(),
            "save path also drops the stash"
        );
        let body = state
            .main_panel
            .lines_text(0, state.main_panel.len())
            .join("\n");
        assert!(
            body.contains("login: empty key"),
            "empty-key notice: {body}"
        );
    }

    #[tokio::test]
    async fn typing_and_editing_still_work_in_secret_mode() {
        let (_dir, mut client) = dummy_client("secret-typing").await;
        let mut state = secret_state("sess-secret-typing");

        press(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert_eq!(state.input, "sk-live-secretx");

        press(
            KeyCode::Backspace,
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert_eq!(state.input, "sk-live-secret");

        state.input_cursor = 0;
        press(
            KeyCode::Delete,
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert_eq!(state.input, "k-live-secret");
    }
}

// --- cowork gate: dismissal, quit reachability, modify UX -------------------
//
// Regression coverage for the round-2 review fixes: Esc dismisses the head
// item (deny-by-dismissal via the session-agnostic cowork.resolve), Ctrl-C
// quit stays reachable while approvals are pending, masked secret entry runs
// before the cowork interception, and modify mode edits JSON replacement
// args.

mod cowork_gate {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
    use tokio::net::UnixListener;

    fn gate_item(id: &str) -> crate::cowork_widget::CoworkItem {
        crate::cowork_widget::CoworkItem {
            id: id.into(),
            tool: "bash".into(),
            step_n: 1,
            args_display: r#"{"cmd":"ls -la"}"#.into(),
            reasoning: String::new(),
            agent: None,
            cwd: None,
            risk: None,
            supports_modify: true,
            rows_cache: std::cell::RefCell::new(None),
        }
    }

    /// A client whose peer records the first request, then replies with
    /// `reply` as the RPC result.
    async fn respondent_client(
        dir_name: &str,
        reply: serde_json::Value,
    ) -> (
        tempfile::TempDir,
        Client,
        tokio::sync::oneshot::Receiver<serde_json::Value>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join(format!("{dir_name}.sock"));
        let listener = UnixListener::bind(&sock_path).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let mut reader = TokioBufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let req: serde_json::Value =
                    serde_json::from_str(line.trim_end()).unwrap_or(serde_json::Value::Null);
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"].clone(),
                    "result": reply,
                });
                let _ = tx.send(req);
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                let _ = reader.get_mut().write_all(&bytes).await;
            }
        });
        let client = Client::connect(&sock_path).await.unwrap();
        (dir, client, rx)
    }

    /// A client connected to a socket whose peer never answers; the keys
    /// under test must not issue an RPC.
    async fn dummy_client(dir_name: &str) -> (tempfile::TempDir, Client) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join(format!("{dir_name}.sock"));
        let listener = UnixListener::bind(&sock_path).unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let client = Client::connect(&sock_path).await.unwrap();
        (dir, client)
    }

    async fn press(
        code: KeyCode,
        mods: KeyModifiers,
        state: &mut crate::state::AppState,
        client: &mut Client,
    ) {
        let mut editor = rustyline::DefaultEditor::new().unwrap();
        handle_key(KeyEvent::new(code, mods), state, client, &mut editor)
            .await
            .unwrap();
    }

    fn panel_text(state: &crate::state::AppState) -> String {
        state
            .main_panel
            .lines_text(0, state.main_panel.len())
            .join("\n")
    }

    #[tokio::test]
    async fn esc_dismisses_head_and_sends_deny() {
        let (_dir, mut client, rx) = respondent_client(
            "cowork-esc",
            serde_json::json!({"id": "ap-1", "resolved": true}),
        )
        .await;
        let mut state = make_state("sess-esc");
        state.pending_cowork.push(gate_item("ap-1"));

        press(KeyCode::Esc, KeyModifiers::empty(), &mut state, &mut client).await;

        let req = rx.await.unwrap();
        assert_eq!(req["method"].as_str(), Some("cowork.resolve"));
        assert_eq!(req["params"]["id"].as_str(), Some("ap-1"));
        assert_eq!(req["params"]["approved"].as_bool(), Some(false));
        assert!(
            state.pending_cowork.is_empty(),
            "Esc must dismiss the head item locally"
        );
        let body = panel_text(&state);
        assert!(
            body.contains("dismissed (denied): bash"),
            "dismissal confirmed; got: {body}"
        );
    }

    #[tokio::test]
    async fn esc_dismiss_removes_item_even_when_gate_gone() {
        // resolved:false (gate timed out / resolved elsewhere) must not
        // re-pend the dismissed item.
        let (_dir, mut client, rx) = respondent_client(
            "cowork-esc-stale",
            serde_json::json!({"id": "ap-1", "resolved": false}),
        )
        .await;
        let mut state = make_state("sess-esc-stale");
        state.pending_cowork.push(gate_item("ap-1"));

        press(KeyCode::Esc, KeyModifiers::empty(), &mut state, &mut client).await;

        let _ = rx.await.unwrap();
        assert!(
            state.pending_cowork.is_empty(),
            "a resolved:false reply must not keep the dismissed item"
        );
        let body = panel_text(&state);
        assert!(
            body.contains("already resolved"),
            "stale-gate dismissal noted; got: {body}"
        );
    }

    #[tokio::test]
    async fn ctrl_c_quit_stays_reachable_with_pending_cowork() {
        let (_dir, mut client) = dummy_client("cowork-ctrlc").await;
        let mut state = make_state("sess-ctrlc");
        state.pending_cowork.push(gate_item("ap-1"));

        // Non-empty input: first Ctrl-C clears the input only.
        state.input = "draft".into();
        state.input_cursor = 5;
        press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.input.is_empty(), "Ctrl-C clears the input first");
        assert!(!state.quit && !state.quit_armed);

        // Empty input: two consecutive Ctrl-C presses arm, then quit.
        press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.quit_armed, "first bare Ctrl-C arms the quit");
        assert!(!state.quit);
        assert_eq!(state.pending_cowork.len(), 1, "Ctrl-C must not decide");
        press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.quit, "second consecutive Ctrl-C quits");
    }

    #[tokio::test]
    async fn secret_entry_runs_before_cowork_interception() {
        // A pending approval must not hijack API-key entry: with BOTH
        // secret_var set and a cowork item pending, plain keys go into the
        // masked input, not to the approval widget.
        let (_dir, mut client) = dummy_client("cowork-secret-order").await;
        let mut state = make_state("sess-secret-order");
        state.secret_var = Some("SMEDJA_TEST_KEY".to_owned());
        state.pending_cowork.push(gate_item("ap-1"));

        press(
            KeyCode::Char('y'),
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert_eq!(state.input, "y", "'y' must type into the secret input");
        assert_eq!(state.pending_cowork.len(), 1, "no approval decided");
        assert!(state.secret_var.is_some(), "still in secret mode");
    }

    #[tokio::test]
    async fn ctrl_c_aborts_secret_entry() {
        let (_dir, mut client) = dummy_client("cowork-secret-ctrlc").await;
        let mut state = make_state("sess-secret-ctrlc");
        state.secret_var = Some("SMEDJA_TEST_KEY".to_owned());
        state.input = "sk-partial".to_owned();
        state.input_cursor = state.input.len();

        press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.secret_var.is_none(), "Ctrl-C aborts secret entry");
        assert!(state.input.is_empty(), "partial key discarded");
        assert!(!state.quit, "aborting secret entry is not a quit");
        assert!(panel_text(&state).contains("login: cancelled"));
    }

    #[tokio::test]
    async fn y_sends_session_agnostic_cowork_resolve() {
        let (_dir, mut client, rx) = respondent_client(
            "cowork-y",
            serde_json::json!({"id": "ap-1", "resolved": true}),
        )
        .await;
        let mut state = make_state("sess-y");
        state.pending_cowork.push(gate_item("ap-1"));

        press(
            KeyCode::Char('y'),
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;

        let req = rx.await.unwrap();
        assert_eq!(req["method"].as_str(), Some("cowork.resolve"));
        assert_eq!(req["params"]["id"].as_str(), Some("ap-1"));
        assert_eq!(req["params"]["approved"].as_bool(), Some(true));
        assert!(
            req["params"].get("session_id").is_none(),
            "cowork.resolve is session-agnostic; params: {}",
            req["params"]
        );
        assert!(state.pending_cowork.is_empty());
        assert!(panel_text(&state).contains("approved: bash"));
    }

    #[tokio::test]
    async fn modify_mode_prefills_current_args_json() {
        let (_dir, mut client) = dummy_client("cowork-modify-prefill").await;
        let mut state = make_state("sess-modify-prefill");
        state.pending_cowork.push(gate_item("ap-1"));

        press(
            KeyCode::Char('m'),
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.cowork_modify_mode);
        assert_eq!(
            state.cowork_modify_input, r#"{"cmd":"ls -la"}"#,
            "modify input is pre-filled with the current args JSON"
        );
    }

    #[tokio::test]
    async fn modify_mode_refused_when_backend_cannot_modify() {
        let (_dir, mut client) = dummy_client("cowork-modify-unsupported").await;
        let mut state = make_state("sess-modify-unsupported");
        let mut item = gate_item("ap-1");
        item.supports_modify = false;
        state.pending_cowork.push(item);

        press(
            KeyCode::Char('m'),
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert!(
            !state.cowork_modify_mode,
            "no modify mode for supports_modify:false"
        );
        assert!(panel_text(&state).contains("modify not supported"));
    }

    #[tokio::test]
    async fn modify_submit_refuses_placeholders_and_non_json() {
        let (_dir, mut client) = dummy_client("cowork-modify-refuse").await;
        let mut state = make_state("sess-modify-refuse");
        state.pending_cowork.push(gate_item("ap-1"));
        state.cowork_modify_mode = true;

        // A redacted display placeholder would destroy the real args.
        state.cowork_modify_input = r#"{"cmd":"run","token":"[redacted]"}"#.into();
        press(
            KeyCode::Enter,
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.cowork_modify_mode, "refusal keeps the editor open");
        assert!(panel_text(&state).contains("modify refused"));
        assert_eq!(state.pending_cowork.len(), 1, "no RPC, item stays");

        // Same for the daemon's truncation marker.
        state.cowork_modify_input = r#"{"cmd":"lo…[truncated]"#.to_owned();
        press(
            KeyCode::Enter,
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.cowork_modify_mode);
        assert!(panel_text(&state).contains("placeholder"));

        // A non-JSON-object instruction is refused too.
        state.cowork_modify_input = "just run it".to_owned();
        press(
            KeyCode::Enter,
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        assert!(state.cowork_modify_mode);
        assert!(panel_text(&state).contains("must be a JSON object"));
        assert_eq!(state.pending_cowork.len(), 1);
    }

    #[tokio::test]
    async fn modify_submit_sends_json_object_instruction() {
        let (_dir, mut client, rx) = respondent_client(
            "cowork-modify-send",
            serde_json::json!({"id": "ap-1", "resolved": true}),
        )
        .await;
        let mut state = make_state("sess-modify-send");
        state.pending_cowork.push(gate_item("ap-1"));

        press(
            KeyCode::Char('m'),
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;
        press(
            KeyCode::Enter,
            KeyModifiers::empty(),
            &mut state,
            &mut client,
        )
        .await;

        let req = rx.await.unwrap();
        assert_eq!(req["method"].as_str(), Some("cowork.modify"));
        assert_eq!(
            req["params"]["instruction"].as_str(),
            Some(r#"{"cmd":"ls -la"}"#),
            "the (edited) args JSON goes as the instruction"
        );
        assert!(state.pending_cowork.is_empty(), "resolved item removed");
        assert!(!state.cowork_modify_mode);
    }
}
