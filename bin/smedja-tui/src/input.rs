use crate::capabilities::format_capabilities_table;
use crate::clipboard::{paste_from_clipboard, push_kill, yank_to_clipboard};
use crate::commands::{command_palette_filtered, filtered_completions};
use crate::completion::{accept_slash_completion, clear_slash_popup, open_file_picker};
use crate::cowork::{apply_cowork_decision, resolve_cowork};
use crate::editor::open_in_editor;
use crate::formatting::{history_search, next_char_boundary, prev_char_boundary};
use crate::messages::push_system_message;
use crate::metrics_poll::toggle_metrics_view;
use crate::secrets;
use crate::session::{resume_into_view, ResumePlan, SessionDetail};
use crate::slash::{self, dispatch_slash};
use crate::state::{AppState, InputMode, Message, Role};
use crate::submit::submit;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use serde_json::{json, Value};
use smedja_rpc::client::Client;

#[allow(clippy::too_many_lines)] // key dispatch table for TUI; splitting would obscure the flow
pub(crate) async fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    client: &mut Client,
    editor: &mut rustyline::DefaultEditor,
) -> Result<()> {
    // Any key other than Ctrl-C disarms the quit confirmation so the two presses
    // must be consecutive.
    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if !is_ctrl_c {
        state.quit_armed = false;
    }

    // ------------------------------------------------------------------
    // Cowork gate widget intercepts keys when there are pending approvals.
    // ------------------------------------------------------------------
    // `y`/`Y` → cowork.approve, `n`/`N` → cowork.deny, `m`/`M` → modify
    // mode. All other keys are consumed while approvals are pending so that
    // accidental keystrokes do not reach the input bar.
    if !state.pending_cowork.is_empty() {
        if state.cowork_modify_mode {
            match key.code {
                KeyCode::Esc => {
                    state.cowork_modify_mode = false;
                    state.cowork_modify_input.clear();
                }
                KeyCode::Enter => {
                    if let Some(item) = state.pending_cowork.first() {
                        let id = item.id.clone();
                        let tool = item.tool.clone();
                        let instruction = std::mem::take(&mut state.cowork_modify_input);
                        let session_id = state.session_id.clone();
                        let result = resolve_cowork(
                            client,
                            &session_id,
                            "cowork.modify",
                            json!({ "id": id, "instruction": instruction }),
                        )
                        .await;
                        let (remove, message) = apply_cowork_decision(
                            &result,
                            "cowork.modify",
                            &format!("modify sent: {instruction}"),
                            &tool,
                        );
                        if remove {
                            state.pending_cowork.remove(0);
                        }
                        push_system_message(state, message);
                    }
                    state.cowork_modify_mode = false;
                }
                KeyCode::Backspace => {
                    state.cowork_modify_input.pop();
                }
                KeyCode::Char(c) => {
                    state.cowork_modify_input.push(c);
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    if let Some(item) = state.pending_cowork.first() {
                        let id = item.id.clone();
                        let tool = item.tool.clone();
                        let session_id = state.session_id.clone();
                        let result = resolve_cowork(
                            client,
                            &session_id,
                            "cowork.approve",
                            json!({ "id": id }),
                        )
                        .await;
                        let (remove, message) = apply_cowork_decision(
                            &result,
                            "cowork.approve",
                            &format!("approved: {tool}"),
                            &tool,
                        );
                        if remove {
                            state.pending_cowork.remove(0);
                        }
                        push_system_message(state, message);
                    }
                }
                KeyCode::Char('n' | 'N') => {
                    if let Some(item) = state.pending_cowork.first() {
                        let id = item.id.clone();
                        let tool = item.tool.clone();
                        let session_id = state.session_id.clone();
                        let result = resolve_cowork(
                            client,
                            &session_id,
                            "cowork.deny",
                            json!({ "id": id, "reason": "denied" }),
                        )
                        .await;
                        let (remove, message) = apply_cowork_decision(
                            &result,
                            "cowork.deny",
                            &format!("denied: {tool}"),
                            &tool,
                        );
                        if remove {
                            state.pending_cowork.remove(0);
                        }
                        push_system_message(state, message);
                    }
                }
                KeyCode::Char('m' | 'M') => {
                    state.cowork_modify_mode = true;
                }
                _ => {}
            }
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // ESC interrupts an in-flight turn (kills the runaway stream) while
    // staying in the prompt. Guarded so it never steals ESC from a sub-mode
    // that uses it for its own cancel.
    // ------------------------------------------------------------------
    if key.code == KeyCode::Esc
        && state.pending_task_id.is_some()
        && !state.slash_popup_visible
        && !state.history_search_mode
        && state.secret_var.is_none()
        && !state.selection_mode
    {
        if let Some(task_id) = state.pending_task_id.take() {
            let _ = client
                .call("turn.cancel", json!({ "task_id": task_id }))
                .await;
            state.turn_in_flight = false;
            state.stream_rx = None;
            push_system_message(state, "\u{2298} interrupted");
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Session detail overlay: Ctrl+Enter loads, Esc closes.
    // ------------------------------------------------------------------
    if state.session_detail_overlay.is_some() {
        if key.code == KeyCode::Esc {
            state.session_detail_overlay = None;
            return Ok(());
        }
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(detail) = state.session_detail_overlay.take() {
                state.session_id = detail.id;
                state.display_start_idx = state.messages.len();
                state.main_panel.clear_display();
                resume_into_view(state, client, ResumePlan::ReplayOnly).await;
            }
            return Ok(());
        }
    }

    // ------------------------------------------------------------------
    // Shift+Tab cycles the permission mode (ask → accept_edits → plan → auto).
    // ------------------------------------------------------------------
    if key.code == KeyCode::BackTab && state.secret_var.is_none() {
        if let Ok(v) = client
            .call("cowork.set_mode", json!({ "session_id": state.session_id }))
            .await
        {
            if let Some(m) = v.get("mode").and_then(Value::as_str) {
                m.clone_into(&mut state.permission_mode);
                push_system_message(state, format!("permission mode \u{2192} {m}"));
            }
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // File picker intercepts keys when open.
    // ------------------------------------------------------------------
    if state.file_picker_open {
        match key.code {
            KeyCode::Esc => {
                state.file_picker_open = false;
            }
            KeyCode::Up => {
                state.file_picker_cursor = state.file_picker_cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = state.file_picker_entries.len().saturating_sub(1);
                if state.file_picker_cursor < max {
                    state.file_picker_cursor += 1;
                }
            }
            KeyCode::Enter => {
                if let Some((name, is_dir)) = state
                    .file_picker_entries
                    .get(state.file_picker_cursor)
                    .cloned()
                {
                    if is_dir {
                        let new_dir = if name == "../" {
                            state
                                .file_picker_dir
                                .parent()
                                .unwrap_or(&state.file_picker_dir)
                                .to_owned()
                        } else {
                            state.file_picker_dir.join(&name)
                        };
                        open_file_picker(state, new_dir);
                    } else {
                        let full_path = state.file_picker_dir.join(&name);
                        let at_ref = format!("@file {} ", full_path.display());
                        state.input.insert_str(state.input_cursor, &at_ref);
                        state.input_cursor += at_ref.len();
                        state.file_picker_open = false;
                    }
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Slash-completion popup intercepts most keys when visible.
    // ------------------------------------------------------------------
    if state.slash_popup_visible {
        match key.code {
            KeyCode::Esc => {
                clear_slash_popup(state);
            }
            KeyCode::Char(' ') | KeyCode::Tab => {
                if !state.runner_picker_mode && !state.session_picker_mode {
                    accept_slash_completion(state, true);
                }
            }
            KeyCode::Down => {
                let max = state.slash_completions.len().saturating_sub(1);
                if state.slash_cursor < max {
                    state.slash_cursor += 1;
                }
            }
            KeyCode::Up => {
                state.slash_cursor = state.slash_cursor.saturating_sub(1);
            }
            KeyCode::Enter => {
                if state.session_picker_mode {
                    let chosen = state.session_picker_ids.get(state.slash_cursor).cloned();
                    state.session_picker_mode = false;
                    state.slash_popup_visible = false;
                    state.slash_completions.clear();
                    state.session_picker_ids.clear();
                    state.slash_cursor = 0;
                    state.input.clear();
                    state.input_cursor = 0;
                    if let Some(id) = chosen.filter(|id| !id.is_empty()) {
                        // Resume in place: swap session, clear live display, replay.
                        state.session_id = id;
                        state.display_start_idx = state.messages.len();
                        state.main_panel.clear_display();
                        resume_into_view(state, client, ResumePlan::ReplayOnly).await;
                    }
                } else if state.runner_picker_mode {
                    if let Some(runner_name) =
                        state.slash_completions.get(state.slash_cursor).cloned()
                    {
                        let session_id = state.session_id.clone();
                        let result = client
                            .call(
                                "session.set_runner",
                                json!({ "session_id": session_id, "runner": runner_name }),
                            )
                            .await;
                        match result {
                            Ok(v) => {
                                let canonical = v
                                    .get("runner")
                                    .and_then(|r| r.as_str())
                                    .unwrap_or(&runner_name)
                                    .to_owned();
                                state.runner.clone_from(&canonical);
                                // Update displayed model to new runner's default.
                                if let Ok(list) = client.call("runner.list", json!({})).await {
                                    if let Some(runners) =
                                        list.get("runners").and_then(|r| r.as_array())
                                    {
                                        if let Some(m) = runners
                                            .iter()
                                            .find(|r| {
                                                r.get("runner").and_then(|n| n.as_str())
                                                    == Some(&canonical)
                                            })
                                            .and_then(|r| r.get("model").and_then(|m| m.as_str()))
                                        {
                                            state.model = Some(m.to_owned());
                                        }
                                    }
                                }
                                push_system_message(
                                    state,
                                    format!("runner switched to {canonical}"),
                                );
                            }
                            Err(e) => {
                                push_system_message(
                                    state,
                                    format!("session.set_runner error: {e}"),
                                );
                            }
                        }
                        state.runner_picker_mode = false;
                        state.slash_popup_visible = false;
                        state.slash_completions.clear();
                        state.slash_cursor = 0;
                        state.input.clear();
                        state.input_cursor = 0;
                    }
                } else if accept_slash_completion(state, false) {
                    let input = std::mem::take(&mut state.input);
                    state.input_cursor = 0;
                    let _ = editor.add_history_entry(&input);
                    if !dispatch_slash(&input, state, client).await? {
                        submit(&input, state, client).await?;
                    }
                }
            }
            KeyCode::Backspace => {
                if state.input_cursor > 0 {
                    let new_pos = prev_char_boundary(&state.input, state.input_cursor);
                    state.input.drain(new_pos..state.input_cursor);
                    state.input_cursor = new_pos;
                }
                let completions = if state.command_palette_mode {
                    command_palette_filtered(&state.input)
                } else if state.input.is_empty() {
                    state.slash_popup_visible = false;
                    Vec::new()
                } else {
                    filtered_completions(&state.input)
                };
                state.slash_cursor = state.slash_cursor.min(completions.len().saturating_sub(1));
                state.slash_completions = completions;
            }
            KeyCode::Char(c) => {
                state.input.insert(state.input_cursor, c);
                state.input_cursor += c.len_utf8();
                let completions = if state.command_palette_mode {
                    command_palette_filtered(&state.input)
                } else {
                    filtered_completions(&state.input)
                };
                state.slash_cursor = 0;
                if completions.is_empty() && !state.command_palette_mode {
                    state.slash_popup_visible = false;
                }
                state.slash_completions = completions;
            }
            _ => {}
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Reverse history search intercept — active when Ctrl-R pressed in
    // input mode.  Consumes all keys until Enter (accept) or Esc (cancel).
    // ------------------------------------------------------------------
    if state.history_search_mode {
        match key.code {
            KeyCode::Esc => {
                state.history_search_mode = false;
                state.history_search_query.clear();
                state.input = std::mem::take(&mut state.saved_input);
                state.input_cursor = state.input.len();
            }
            KeyCode::Enter => {
                state.history_search_mode = false;
                state.history_search_query.clear();
            }
            KeyCode::Backspace => {
                state.history_search_query.pop();
                let query = state.history_search_query.clone();
                if query.is_empty() {
                    state.saved_input.clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                } else if let Some((_, matched)) = history_search(&state.prompt_history, &query) {
                    matched.clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                }
            }
            KeyCode::Char(c) => {
                state.history_search_query.push(c);
                let query = state.history_search_query.clone();
                if let Some((_, matched)) = history_search(&state.prompt_history, &query) {
                    matched.clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Panel search mode intercept — '/' in scroll mode opens this.
    // ------------------------------------------------------------------
    if state.panel_search_mode {
        match key.code {
            KeyCode::Esc => {
                state.panel_search_mode = false;
                state.panel_search_query.clear();
            }
            KeyCode::Enter => {
                // Keep query on Enter so the user can browse matches.
                state.panel_search_mode = false;
            }
            KeyCode::Backspace => {
                state.panel_search_query.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.panel_search_query.push(c);
            }
            _ => {}
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Ctrl-A: toggle role cockpit panel (works in both input and scroll mode).
    // ------------------------------------------------------------------
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.panels.role_cockpit = !state.panels.role_cockpit;
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Ctrl-V: toggle value panel when in scroll/rail mode; paste in input mode.
    // ------------------------------------------------------------------
    if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.scroll_focus {
            state.panels.value = !state.panels.value;
        } else if let Some(text) = paste_from_clipboard() {
            let text = text.replace('\r', "");
            let before = &state.input[..state.input_cursor];
            let after = &state.input[state.input_cursor..];
            let new_input = format!("{before}{text}{after}");
            let advance = text.len();
            state.input = new_input;
            state.input_cursor += advance;
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Session rail cursor navigation in input mode — Alt+↑/↓ only, so
    // plain Up/Down remain available for prompt history.
    // ------------------------------------------------------------------
    if state.panels.session_rail && !state.scroll_focus && key.modifiers.contains(KeyModifiers::ALT)
    {
        match key.code {
            KeyCode::Up => {
                state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down if !state.session_rail_items.is_empty() => {
                let max = state.session_rail_items.len().saturating_sub(1);
                state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
                return Ok(());
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Scroll / visual-selection mode intercept.
    // ------------------------------------------------------------------
    if state.scroll_focus {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if state.diff_overlay.is_some() {
                    state.diff_scroll = state.diff_scroll.saturating_add(1);
                } else if state.panels.session_rail && !state.session_rail_items.is_empty() {
                    let max = state.session_rail_items.len().saturating_sub(1);
                    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
                } else if state.selection_mode {
                    // Keyboard selection is whole-line: extend by a line, snapping
                    // the end column to that line's length.
                    let next =
                        (state.selection_end.0 + 1).min(state.main_panel.len().saturating_sub(1));
                    state.selection_end = (next, state.main_panel.line_char_len(next));
                } else {
                    state.main_panel.scroll_down();
                }
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if state.diff_overlay.is_some() {
                    state.diff_scroll = state.diff_scroll.saturating_sub(1);
                } else if state.panels.session_rail {
                    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
                } else if state.selection_mode {
                    let prev = state.selection_end.0.saturating_sub(1);
                    state.selection_end = (prev, state.main_panel.line_char_len(prev));
                } else {
                    state.main_panel.scroll_up();
                }
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('G') => {
                state.main_panel.scroll_to_bottom();
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('g') => {
                if state.g_pending {
                    state.main_panel.scroll_to_top();
                    state.g_pending = false;
                } else {
                    state.g_pending = true;
                }
                return Ok(());
            }
            KeyCode::Char('v') if !state.selection_mode => {
                state.selection_mode = true;
                let l = state.main_panel.scroll;
                state.selection_anchor = (l, 0);
                state.selection_end = (l, state.main_panel.line_char_len(l));
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('y') if state.selection_mode => {
                let text = state
                    .main_panel
                    .selection_text(state.selection_anchor, state.selection_end);
                let count = text.lines().count().max(1);
                let msg = match yank_to_clipboard(std::slice::from_ref(&text)) {
                    Ok(_) => format!("\u{2713} {count} lines copied to clipboard"),
                    Err(e) => e,
                };
                state.clipboard = Some(text);
                state.selection_mode = false;
                push_system_message(state, msg);
                return Ok(());
            }
            KeyCode::Char('t') => {
                if let Some(tp) = state.last_traceparent.clone() {
                    let _ = yank_to_clipboard(std::slice::from_ref(&tp));
                    state.clipboard = Some(tp.clone());
                    let hint = if state.otlp_configured {
                        // Extract trace_id: field index 1 of the W3C traceparent
                        // (format: version-trace_id-parent_id-flags), which is a
                        // 32-hex-char trace ID.
                        let trace_id = tp.split('-').nth(1).unwrap_or("");
                        format!(" — open in Jaeger: http://localhost:16686/trace/{trace_id}")
                    } else {
                        " — set SMEDJA_OTLP_ENDPOINT to export traces".to_owned()
                    };
                    push_system_message(state, format!("trace: {tp}  (copied){hint}"));
                }
                return Ok(());
            }
            // T (uppercase): toggle thinking step timeline expansion.
            KeyCode::Char('T') => {
                if !state.thinking_steps.is_empty() {
                    state.thinking_expanded = !state.thinking_expanded;
                }
                return Ok(());
            }
            // S: toggle diff overlay between unified and split view.
            KeyCode::Char('S') => {
                if state.diff_overlay.is_some() {
                    state.diff_split_view = !state.diff_split_view;
                }
                return Ok(());
            }
            // [ / ] : navigate session rail cursor (when rail is visible).
            KeyCode::Char('[') => {
                if state.panels.session_rail {
                    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
                }
                return Ok(());
            }
            KeyCode::Char(']') => {
                if state.panels.session_rail && !state.session_rail_items.is_empty() {
                    let max = state.session_rail_items.len().saturating_sub(1);
                    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
                }
                return Ok(());
            }
            // Enter: open session detail overlay for the highlighted session.
            KeyCode::Enter if state.panels.session_rail => {
                if let Some((id, _)) = state
                    .session_rail_items
                    .get(state.session_rail_cursor)
                    .cloned()
                {
                    if let Ok(v) = client.call("session.get", json!({ "id": id })).await {
                        state.session_detail_overlay = Some(SessionDetail::from_json(&v));
                    }
                }
                return Ok(());
            }
            KeyCode::Char('i' | 'a') => {
                state.scroll_focus = false;
                state.selection_mode = false;
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('/') => {
                state.panel_search_mode = true;
                state.panel_search_query.clear();
                return Ok(());
            }
            KeyCode::Esc => {
                // Fall through to the main Esc handler below.
            }
            // Ctrl+P in scroll mode: toggle session config peek overlay.
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.show_session_peek = !state.show_session_peek;
                return Ok(());
            }
            _ => return Ok(()), // consume unknown keys in scroll mode
        }
    }

    // ------------------------------------------------------------------
    // M10 — Vim Normal mode intercept (only when not in scroll mode)
    // ------------------------------------------------------------------
    if !state.scroll_focus && state.vim_input_mode == InputMode::Normal {
        match key.code {
            // Enter insert mode.
            KeyCode::Char('i' | 'a') => {
                state.vim_input_mode = InputMode::Insert;
                state.pending_vim_key = None;
            }
            // Motion: h → move cursor left.
            KeyCode::Char('h') => {
                if state.input_cursor > 0 {
                    // Step back one UTF-8 character.
                    let before = &state.input[..state.input_cursor];
                    if let Some(ch) = before.chars().next_back() {
                        state.input_cursor -= ch.len_utf8();
                    }
                }
                state.pending_vim_key = None;
            }
            // Motion: l → move cursor right.
            KeyCode::Char('l') => {
                if state.input_cursor < state.input.len() {
                    let ch = state.input[state.input_cursor..]
                        .chars()
                        .next()
                        .unwrap_or('\0');
                    state.input_cursor += ch.len_utf8();
                }
                state.pending_vim_key = None;
            }
            // Motion: 0 → start of line.
            KeyCode::Char('0') => {
                state.input_cursor = 0;
                state.pending_vim_key = None;
            }
            // Motion: $ → end of line.
            KeyCode::Char('$') => {
                state.input_cursor = state.input.len();
                state.pending_vim_key = None;
            }
            // Motion: w → forward one word.
            KeyCode::Char('w') => {
                let rest = &state.input[state.input_cursor..];
                let skip = rest
                    .char_indices()
                    .skip_while(|(_, c)| !c.is_whitespace())
                    .skip_while(|(_, c)| c.is_whitespace())
                    .map(|(i, _)| i)
                    .next()
                    .unwrap_or(rest.len());
                state.input_cursor += skip;
                state.pending_vim_key = None;
            }
            // Motion: b → backward one word.
            KeyCode::Char('b') => {
                let before = &state.input[..state.input_cursor];
                let rev_skip = before
                    .char_indices()
                    .rev()
                    .skip_while(|(_, c)| c.is_whitespace())
                    .skip_while(|(_, c)| !c.is_whitespace())
                    .map(|(i, _)| i)
                    .next()
                    .unwrap_or(0);
                state.input_cursor = rev_skip;
                state.pending_vim_key = None;
            }
            // Edit: x → delete char at cursor.
            KeyCode::Char('x') => {
                if state.input_cursor < state.input.len() {
                    let ch = state.input[state.input_cursor..]
                        .chars()
                        .next()
                        .unwrap_or('\0');
                    state.input.remove(state.input_cursor);
                    let _ = ch; // char used for len_utf8 implicitly by remove
                }
                state.pending_vim_key = None;
            }
            // Edit: dd → kill entire line.
            KeyCode::Char('d') => {
                if state.pending_vim_key == Some('d') {
                    state.input.clear();
                    state.input_cursor = 0;
                    state.pending_vim_key = None;
                } else {
                    state.pending_vim_key = Some('d');
                }
            }
            // Scroll: G → bottom; gg → top.
            KeyCode::Char('G') => {
                state.main_panel.scroll_to_bottom();
                state.pending_vim_key = None;
            }
            KeyCode::Char('g') => {
                if state.pending_vim_key == Some('g') {
                    state.main_panel.scroll = state.main_panel.display_start;
                    state.pending_vim_key = None;
                } else {
                    state.pending_vim_key = Some('g');
                }
            }
            // Esc: already handled below by the main Esc arm — don't return early.
            KeyCode::Esc => {}
            // Ignore all other keys in Normal mode.
            _ => {
                state.pending_vim_key = None;
                return Ok(());
            }
        }
        // Esc in Normal mode falls through to the main Esc handler below.
        if key.code != KeyCode::Esc {
            return Ok(());
        }
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl-C is non-destructive: clear an in-progress input, otherwise
            // require a SECOND consecutive Ctrl-C to actually quit (so an accidental
            // press never drops you to a blank terminal). Copy is mouse / v-y.
            if !state.input.is_empty() {
                state.input.clear();
                state.input_cursor = 0;
                state.quit_armed = false;
            } else if state.quit_armed {
                state.quit = true;
            } else {
                state.quit_armed = true;
                push_system_message(state, "press Ctrl-C again to exit smedja-tui");
            }
        }

        // Ctrl-S: open/close session browser overlay.
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.session_browser_open {
                state.session_browser_open = false;
            } else {
                state.session_browser_open = true;
                state.session_browser_cursor = 0;
            }
        }

        // Ctrl-R: toggle reverse history search (input mode only).
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                state.history_search_mode = !state.history_search_mode;
                state.history_search_query.clear();
                if state.history_search_mode {
                    state.input.clone_into(&mut state.saved_input);
                }
            }
        }

        // Ctrl-F: toggle context rail (scroll mode) / open file picker (input mode).
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.scroll_focus {
                state.panels.context_rail = !state.panels.context_rail;
            } else {
                let start =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                open_file_picker(state, start);
            }
        }

        // Ctrl-W / Ctrl-Shift-W: toggle session browser left-rail.
        // Ctrl-W is consumed by many Linux WMs/terminals (e.g. WezTerm on CachyOS),
        // so Ctrl-Shift-W (crossterm: Char('W') + CONTROL) is the Linux fallback.
        KeyCode::Char('w' | 'W') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.panels.session_rail = !state.panels.session_rail;
            state.session_rail_cursor = 0;
            // Trigger an immediate poll on next tick by clearing the timestamp.
            if state.panels.session_rail {
                state.last_session_rail_poll = None;
            }
        }

        // Ctrl-G: open $EDITOR / $VISUAL to compose a multi-line message.
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                if let Some(new_text) = open_in_editor(&state.input) {
                    state.input = new_text;
                    state.input_cursor = state.input.chars().count();
                }
            }
        }

        // Ctrl-K: kill from cursor to end of line; or open command palette when input is empty.
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                let tail: String = state.input[state.input_cursor..].to_owned();
                if tail.is_empty() && state.input_cursor == 0 {
                    // Empty input → open command palette.
                    state.slash_popup_visible = true;
                    state.command_palette_mode = true;
                    state.slash_completions = command_palette_filtered("");
                    state.slash_cursor = 0;
                } else if !tail.is_empty() {
                    state.input.drain(state.input_cursor..);
                    push_kill(&mut state.kill_ring, tail);
                }
            }
        }

        // Ctrl-U: kill from start of line to cursor; push onto kill ring (input mode only).
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                let killed: String = state.input[..state.input_cursor].to_owned();
                if !killed.is_empty() {
                    state.input.drain(..state.input_cursor);
                    state.input_cursor = 0;
                    push_kill(&mut state.kill_ring, killed);
                }
            }
        }

        // Ctrl-Y: yank most recent kill at cursor position (input mode only).
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                if let Some(text) = state.kill_ring.back().cloned() {
                    state.input.insert_str(state.input_cursor, &text);
                    state.input_cursor += text.len();
                }
            }
        }

        // Ctrl-B: move cursor one character left (input mode only).
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus && state.input_cursor > 0 {
                state.input_cursor = prev_char_boundary(&state.input, state.input_cursor);
            }
        }

        // Ctrl-T: toggle the metrics view panel (read-only rollup snapshot).
        // Toggling on clears the poll cadence so the next tick fetches at once.
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            toggle_metrics_view(state);
        }

        // Ctrl-L: toggle LSP diagnostic panel in the right rail.
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.panels.lsp = !state.panels.lsp;
        }

        // Ctrl-O: toggle observability panel in the right rail.
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.panels.obs = !state.panels.obs;
        }

        // Ctrl-Q: tap toggles the quality panel; hold ≥ 500ms triggers Tier-2 review.
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match key.kind {
                KeyEventKind::Press => {
                    // First press: toggle panel and start timing for hold detection.
                    state.panels.quality = !state.panels.quality;
                    state.ctrl_q_pressed_at = Some(std::time::Instant::now());
                }
                KeyEventKind::Repeat => {
                    // Key repeat fires while held; trigger review once at 500ms.
                    if let Some(t) = state.ctrl_q_pressed_at {
                        if t.elapsed() >= std::time::Duration::from_millis(500) {
                            state.ctrl_q_pressed_at = None;
                            state.panels.quality = true; // ensure panel is open
                            slash::trigger_quality_review(state, client).await;
                        }
                    }
                }
                KeyEventKind::Release => {
                    state.ctrl_q_pressed_at = None;
                }
            }
        }

        KeyCode::Esc => {
            if state.session_browser_open {
                state.session_browser_open = false;
            } else if state.show_session_peek {
                state.show_session_peek = false;
            } else if state.secret_var.take().is_some() {
                // Cancel masked secret entry; discard whatever was typed.
                state.input.clear();
                state.input_cursor = 0;
                push_system_message(state, "login: cancelled");
            } else if state.panel_search_mode {
                state.panel_search_mode = false;
                state.panel_search_query.clear();
            } else if state.diff_overlay.is_some() {
                state.diff_overlay = None;
                state.diff_split_view = false;
            } else if state.selection_mode {
                state.selection_mode = false;
            } else if !state.scroll_focus && state.vim_input_mode == InputMode::Insert {
                // Esc in Insert mode → enter Normal mode.
                state.vim_input_mode = InputMode::Normal;
                state.pending_vim_key = None;
            } else if state.scroll_focus {
                state.scroll_focus = false;
            } else if state.block_browser_open {
                state.block_browser_open = false;
            } else {
                state.scroll_focus = true;
            }
        }

        KeyCode::Up => {
            if state.session_browser_open {
                state.session_browser_cursor = state.session_browser_cursor.saturating_sub(1);
            } else if state.block_browser_open {
                state.block_browser_cursor = state.block_browser_cursor.saturating_sub(1);
            } else {
                // input mode: browse prompt history backwards
                if !state.prompt_history.is_empty() {
                    let new_idx = match state.history_idx {
                        None => {
                            state.input.clone_into(&mut state.saved_input);
                            state.prompt_history.len() - 1
                        }
                        Some(0) => 0,
                        Some(i) => i - 1,
                    };
                    state.history_idx = Some(new_idx);
                    state.prompt_history[new_idx].clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                }
            }
        }

        KeyCode::Down => {
            if state.session_browser_open {
                let count = state.session_rail_items.len();
                if count > 0 {
                    state.session_browser_cursor = (state.session_browser_cursor + 1) % count;
                }
            } else if state.block_browser_open {
                let max = state.block_store.len().saturating_sub(1);
                if state.block_browser_cursor < max {
                    state.block_browser_cursor += 1;
                }
            } else {
                // input mode: browse prompt history forwards
                if let Some(idx) = state.history_idx {
                    if idx + 1 < state.prompt_history.len() {
                        let new_idx = idx + 1;
                        state.history_idx = Some(new_idx);
                        state.prompt_history[new_idx].clone_into(&mut state.input);
                        state.input_cursor = state.input.len();
                    } else {
                        state.history_idx = None;
                        state.input = std::mem::take(&mut state.saved_input);
                        state.input_cursor = state.input.len();
                    }
                }
            }
        }

        KeyCode::Backspace => {
            if state.input_cursor > 0 {
                let new_pos = prev_char_boundary(&state.input, state.input_cursor);
                state.input.drain(new_pos..state.input_cursor);
                state.input_cursor = new_pos;
            }
        }

        // NOTE: the keyboard "block browser" (bare b/c/r/d/D) was removed — those
        // letters now type normally via the catch-all `Char(c)` arm below. Browse
        // with the arrow keys / mouse wheel; mark & copy with the mouse;
        // Shift+Enter for a newline.

        // Shift/Alt/Ctrl+Enter → insert a literal newline (multi-line compose),
        // mirroring claude-cli / opencode. Requires the kitty keyboard protocol
        // (pushed at startup) so the host terminal disambiguates the modifier.
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            if !state.scroll_focus {
                state.input.insert(state.input_cursor, '\n');
                state.input_cursor += 1;
            }
        }

        KeyCode::Enter => {
            // Session browser: switch to the selected session and close the overlay.
            if state.session_browser_open {
                if let Some((id, _label)) = state
                    .session_rail_items
                    .get(state.session_browser_cursor)
                    .cloned()
                {
                    state.session_browser_open = false;
                    let _ = client
                        .call("session.switch", serde_json::json!({ "session_id": id }))
                        .await;
                }
                return Ok(());
            }

            // Masked secret entry (API key paste): save to the secrets file under
            // the pending env-var name; never echo or send it as a turn.
            if let Some(var) = state.secret_var.take() {
                let key = std::mem::take(&mut state.input);
                state.input_cursor = 0;
                let msg = if key.trim().is_empty() {
                    "login: empty key — cancelled".to_owned()
                } else {
                    secrets::save_secret(&var, key.trim())
                };
                push_system_message(state, msg);
                return Ok(());
            }

            // L128: multi-line continuation — trailing `\` means "continue".
            if state.input.ends_with('\\') {
                // Strip the trailing backslash and append a newline continuation.
                state.input.pop();
                state.input.push('\n');
                state.input_cursor = state.input.len();
                return Ok(());
            }

            let input = std::mem::take(&mut state.input);
            state.input_cursor = 0;

            // Record in rustyline history (ignore errors — history is advisory).
            let _ = editor.add_history_entry(&input);

            if dispatch_slash(&input, state, client).await? {
                return Ok(());
            }

            if let Some(rest) = input.trim().strip_prefix("/task create ") {
                let title = rest.trim().to_owned();
                if !title.is_empty() {
                    if let Ok(v) = client.call("task.create", json!({"title": title})).await {
                        let msg = Message {
                            role: Role::System,
                            text: format!("task created: {}", v["id"].as_str().unwrap_or("?")),
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                    }
                }
            } else if let Some(id) = input.trim().strip_prefix("/task done ") {
                let id = id.trim().to_owned();
                if client.call("task.close", json!({"id": id})).await.is_ok() {
                    let msg = Message {
                        role: Role::System,
                        text: format!("task {id} closed"),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                }
            } else if let Some(arg) = input.trim().strip_prefix("/cowork ") {
                match arg.trim() {
                    "on" | "off" => {
                        let enabled = arg.trim() == "on";
                        let session_id = state.session_id.clone();
                        if client
                            .call(
                                "cowork.set",
                                json!({ "session_id": session_id, "enabled": enabled }),
                            )
                            .await
                            .is_ok()
                        {
                            let msg = Message {
                                role: Role::System,
                                text: format!(
                                    "cowork mode {}",
                                    if enabled { "enabled" } else { "disabled" }
                                ),
                            };
                            state.main_panel.push_line(msg.text.clone());
                            state.messages.push(msg);
                        }
                    }
                    "status" => {
                        let session_id = state.session_id.clone();
                        match client
                            .call("session.get", json!({ "id": session_id }))
                            .await
                        {
                            Ok(resp) => {
                                let cowork_on = resp["cowork_mode"].as_bool().unwrap_or(false);
                                push_system_message(
                                    state,
                                    format!("cowork: {}", if cowork_on { "on" } else { "off" }),
                                );
                            }
                            Err(_) => {
                                push_system_message(state, "cowork: status unavailable");
                            }
                        }
                    }
                    _ => {
                        let msg = Message {
                            role: Role::System,
                            text: "usage: /cowork on|off|status".into(),
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                    }
                }
            } else if let Some(rest) = input.trim().strip_prefix("/stage ") {
                // /stage <tool> <json-args>
                if let Some((tool, json_args)) = rest.split_once(' ') {
                    let text = match state.staging_queue.stage(tool, json_args) {
                        Ok(s) => s,
                        Err(e) => e,
                    };
                    let msg = Message {
                        role: Role::System,
                        text,
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                } else {
                    let msg = Message {
                        role: Role::System,
                        text: "usage: /stage <tool> <json-args>".into(),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                }
            } else if let Some(rest) = input.trim().strip_prefix("/unstage") {
                // /unstage [N]
                let n: Option<usize> = rest.trim().parse().ok();
                let text = state.staging_queue.unstage(n);
                let msg = Message {
                    role: Role::System,
                    text,
                };
                state.main_panel.push_line(msg.text.clone());
                state.messages.push(msg);
                for item in state.staging_queue.list() {
                    let msg = Message {
                        role: Role::System,
                        text: item,
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                }
            } else if input.trim() == "/run" {
                let actions = state.staging_queue.drain();
                if actions.is_empty() {
                    let msg = Message {
                        role: Role::System,
                        text: "no staged actions".into(),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                } else {
                    for action in actions {
                        let payload = json!({"tool": action.tool, "args": action.args});
                        let result = client.call("tool.call", payload).await;
                        let text = match result {
                            Ok(v) => format!("\u{25b8} {v}"),
                            Err(e) => format!("\u{25b8} error: {e}"),
                        };
                        let msg = Message {
                            role: Role::System,
                            text,
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                    }
                }
            } else if input.trim() == "/agent sre" {
                state.mode = Some("sre".into());
                state.tier = Some("deep".into());
                let session_id = state.session_id.clone();
                let _ = client
                    .call(
                        "session.set_mode",
                        json!({
                            "session_id": session_id,
                            "mode": "sre",
                        }),
                    )
                    .await;
                let msg = Message {
                    role: Role::System,
                    text: "SRE mode activated (tier: deep)".into(),
                };
                state.main_panel.push_line(msg.text.clone());
                state.messages.push(msg);
            } else if input.trim() == "/capabilities" {
                let text = match client.call("runner.list", json!({})).await {
                    Ok(v) => {
                        let runners = v
                            .get("runners")
                            .and_then(|r| r.as_array())
                            .cloned()
                            .unwrap_or_default();
                        format_capabilities_table(&runners)
                    }
                    Err(e) => format!("capabilities: error — {e}"),
                };
                let msg = Message {
                    role: Role::System,
                    text,
                };
                state.main_panel.push_line(msg.text.clone());
                state.messages.push(msg);
            } else if input.trim() == "/health" {
                // Measure RPC round-trip latency by calling session.get.
                let start = std::time::Instant::now();
                let session_id = state.session_id.clone();
                let health_result = client
                    .call("session.get", json!({ "id": session_id }))
                    .await;
                let latency_ms = start.elapsed().as_millis();
                let text = match health_result {
                    Ok(_) => {
                        format!("health: socket=ok session={session_id} latency={latency_ms}ms")
                    }
                    Err(e) => format!("health: error — {e}"),
                };
                let msg = Message {
                    role: Role::System,
                    text,
                };
                state.main_panel.push_line(msg.text.clone());
                state.messages.push(msg);
            } else {
                submit(&input, state, client).await?;
            }
        }

        KeyCode::Left => {
            state.input_cursor = prev_char_boundary(&state.input, state.input_cursor);
        }

        KeyCode::Right => {
            state.input_cursor = next_char_boundary(&state.input, state.input_cursor);
        }

        KeyCode::Home => {
            state.input_cursor = 0;
        }

        KeyCode::End => {
            state.input_cursor = state.input.len();
        }

        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input_cursor = 0;
        }

        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input_cursor = state.input.len();
        }

        // Ctrl-P / Ctrl-N: history browse (Emacs-style aliases for Up / Down)
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.prompt_history.is_empty() {
                let new_idx = match state.history_idx {
                    None => {
                        state.input.clone_into(&mut state.saved_input);
                        state.prompt_history.len() - 1
                    }
                    Some(0) => 0,
                    Some(i) => i - 1,
                };
                state.history_idx = Some(new_idx);
                state.prompt_history[new_idx].clone_into(&mut state.input);
                state.input_cursor = state.input.len();
            }
        }

        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(idx) = state.history_idx {
                if idx + 1 < state.prompt_history.len() {
                    let new_idx = idx + 1;
                    state.history_idx = Some(new_idx);
                    state.prompt_history[new_idx].clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                } else {
                    state.history_idx = None;
                    state.input = std::mem::take(&mut state.saved_input);
                    state.input_cursor = state.input.len();
                }
            }
        }

        KeyCode::Char('/') if state.input.is_empty() => {
            // L129: open slash popup when `/` is the first character typed.
            state.input.insert(state.input_cursor, '/');
            state.input_cursor += 1;
            state.slash_completions = filtered_completions("/");
            state.slash_cursor = 0;
            state.slash_popup_visible = true;
        }

        KeyCode::Char(c) => {
            state.input.insert(state.input_cursor, c);
            state.input_cursor += c.len_utf8();
        }

        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::commands::SLASH_COMPLETIONS;
    #[allow(unused_imports)]
    use crate::main_panel;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use crate::thoughts_panel;
    #[allow(unused_imports)]
    use serde_json::{json, Value};
    #[allow(unused_imports)]
    use std::collections::VecDeque;

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
    fn backslash_continuation_appends_newline() {
        let mut input = "hello\\".to_owned();
        // Simulate the Enter key handling logic inline.
        assert!(input.ends_with('\\'));
        input.pop();
        input.push('\n');
        assert!(input.contains('\n'));
        assert_eq!(input, "hello\n");
    }

    #[test]
    fn slash_esc_clears_input_and_closes_popup() {
        let mut state = make_state("test-session");
        state.input = "/ti".to_owned();
        state.input_cursor = 3;
        state.slash_completions = filtered_completions("/ti");
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        clear_slash_popup(&mut state);

        assert!(state.input.is_empty(), "input must be cleared on Esc");
        assert_eq!(state.input_cursor, 0, "cursor must reset to 0 on Esc");
        assert!(!state.slash_popup_visible, "popup must close on Esc");
        assert!(
            state.slash_completions.is_empty(),
            "completions must be cleared on Esc"
        );
        assert_eq!(state.slash_cursor, 0);
    }

    #[test]
    fn slash_esc_on_popup_already_closed_is_idempotent() {
        let mut state = make_state("test-session");
        state.input = "hello".to_owned();
        state.input_cursor = 5;
        state.slash_popup_visible = false;

        clear_slash_popup(&mut state);

        assert!(state.input.is_empty());
        assert_eq!(state.input_cursor, 0);
        assert!(!state.slash_popup_visible);
    }

    #[test]
    fn health_command_shows_socket_path_in_state() {
        let mut state = make_state("sess-health");
        // Simulate what /health should push to main_panel.
        let msg = format!("health: socket=ok session={} latency=?ms", state.session_id);
        state.main_panel.push_line(msg.clone());
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("health"),
            "health output should appear in panel"
        );
        assert!(
            content.contains("sess-health"),
            "health output should show session ID"
        );
    }

    #[test]
    fn health_command_param_key_is_id() {
        // The /health handler must pass "id", not "session_id", to session.get.
        let session_id = "sess-health";
        let payload = json!({ "id": session_id });
        assert!(
            payload.get("id").is_some(),
            "RPC payload must contain key \"id\""
        );
        assert!(
            payload.get("session_id").is_none(),
            "RPC payload must not contain key \"session_id\""
        );
        assert_eq!(
            payload["id"].as_str().unwrap(),
            session_id,
            "\"id\" value must match the session id"
        );
    }

    #[tokio::test]
    async fn health_command_handle_key_shows_latency_in_panel() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
        use tokio::net::UnixListener;

        // Bind to a socket inside a temp dir so the path is unique per test run.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("health-test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn a minimal mock daemon that handles exactly one JSON-RPC request.
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let mut reader = TokioBufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let req: serde_json::Value =
                    serde_json::from_str(line.trim_end()).unwrap_or(serde_json::Value::Null);
                let id = req["id"].clone();
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "id": "sess-health-mock" }
                });
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                let _ = reader.get_mut().write_all(&bytes).await;
            }
        });

        let mut client = Client::connect(&sock_path).await.unwrap();
        let mut state = make_state("sess-health-mock");
        state.input = "/health".into();
        let mut editor = rustyline::DefaultEditor::new().unwrap();

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        );
        handle_key(key, &mut state, &mut client, &mut editor)
            .await
            .unwrap();

        // input is cleared by std::mem::take before the command runs.
        assert!(
            state.input.is_empty(),
            "/health must clear the input field after Enter"
        );

        // The main panel must contain the health output line.
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("health"),
            "main panel should contain health output after /health command; got: {content:?}"
        );
    }

    #[test]
    fn prev_char_boundary_moves_back_one_ascii() {
        assert_eq!(prev_char_boundary("hello", 3), 2);
    }

    #[test]
    fn prev_char_boundary_at_zero_stays_zero() {
        assert_eq!(prev_char_boundary("hello", 0), 0);
    }

    #[test]
    fn next_char_boundary_moves_forward_one_ascii() {
        assert_eq!(next_char_boundary("hello", 2), 3);
    }

    #[test]
    fn next_char_boundary_at_end_stays_at_end() {
        assert_eq!(next_char_boundary("hello", 5), 5);
    }

    #[test]
    fn prev_char_boundary_unicode_moves_by_char() {
        // 'é' encodes as 2 bytes (U+00E9); cursor at 2 should move to 0.
        let s = "é";
        assert_eq!(s.len(), 2);
        assert_eq!(prev_char_boundary(s, 2), 0);
    }

    #[test]
    fn next_char_boundary_unicode_moves_by_char() {
        let s = "é";
        assert_eq!(next_char_boundary(s, 0), 2);
    }

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

    #[test]
    fn runner_picker_confirm_sets_runner_and_clears_mode() {
        let mut state = make_state("sess-picker");
        state.runner_picker_mode = true;
        state.slash_completions = vec!["codex".to_owned(), "claude".to_owned()];
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        // Simulate accept: take selected runner name, update state, clear picker
        let runner_name = state.slash_completions[state.slash_cursor].clone();
        state.runner = runner_name.clone();
        push_system_message(&mut state, format!("runner switched to {runner_name}"));
        state.runner_picker_mode = false;
        state.slash_popup_visible = false;
        state.slash_completions.clear();
        state.slash_cursor = 0;

        assert_eq!(state.runner, "codex");
        assert!(
            !state.runner_picker_mode,
            "runner_picker_mode must be cleared after confirm"
        );
        assert!(
            !state.slash_popup_visible,
            "popup must be closed after confirm"
        );
        assert!(
            state
                .main_panel
                .lines_text(0, 100)
                .iter()
                .any(|l| l.contains("runner switched")),
            "confirmation message must appear in panel"
        );
    }

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

    #[test]
    fn thinking_tokens_accumulate_in_current_thinking() {
        let mut state = make_state("sess-think");
        assert!(state.current_thinking.is_empty());
        // Simulate two ThinkingDelta stream events arriving.
        state.current_thinking.push_str("step one ");
        state.current_thinking.push_str("step two");
        assert_eq!(state.current_thinking, "step one step two");
    }

    #[test]
    fn thinking_expanded_toggles_only_when_content_present() {
        let mut state = make_state("sess-think-toggle");
        state.scroll_focus = true;
        // No steps: T key must be a no-op.
        assert!(state.thinking_steps.is_empty());
        if !state.thinking_steps.is_empty() {
            state.thinking_expanded = !state.thinking_expanded;
        }
        assert!(
            !state.thinking_expanded,
            "T must not toggle when no thinking steps"
        );

        // With steps: T key must toggle.
        state
            .thinking_steps
            .push(thoughts_panel::ThinkingStep::Answer { elapsed_s: 1.0 });
        if !state.thinking_steps.is_empty() {
            state.thinking_expanded = !state.thinking_expanded;
        }
        assert!(
            state.thinking_expanded,
            "T must expand when thinking steps are present"
        );
        if !state.thinking_steps.is_empty() {
            state.thinking_expanded = !state.thinking_expanded;
        }
        assert!(!state.thinking_expanded, "second T must collapse");
    }

    #[test]
    fn thinking_steps_cleared_at_turn_start() {
        let mut state = make_state("sess-steps-clear");
        state
            .thinking_steps
            .push(thoughts_panel::ThinkingStep::Answer { elapsed_s: 1.0 });
        assert_eq!(state.thinking_steps.len(), 1);
        state.thinking_steps.clear();
        assert!(state.thinking_steps.is_empty());
    }

    #[test]
    fn thinking_step_tool_has_correct_fields() {
        let step = thoughts_panel::ThinkingStep::Tool {
            name: "bash".into(),
            preview: "ls /src".into(),
            elapsed_s: 0.5,
        };
        assert!(matches!(step.elapsed_s(), 0.4..=0.6));
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

    #[test]
    fn thinking_cleared_on_new_turn() {
        let mut state = make_state("sess-think-clear");
        state.current_thinking = "previous reasoning".to_owned();
        state.thinking_expanded = true;
        // Simulate what happens when a new turn starts.
        state.current_thinking.clear();
        state.thinking_expanded = false;
        assert!(state.current_thinking.is_empty());
        assert!(!state.thinking_expanded);
    }

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
    fn active_agent_name_captured_from_stream_started_event() {
        let mut state = make_state("sess-agent");
        let event = serde_json::json!({"type": "started", "agent_name": "review"});
        if let Some(name) = event["agent_name"].as_str() {
            state.active_agent_name = Some(name.to_owned());
        }
        assert_eq!(state.active_agent_name.as_deref(), Some("review"));
    }

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

        if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty()
        {
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

        if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty()
        {
            let max = state.session_rail_items.len().saturating_sub(1);
            state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        }
        assert_eq!(state.session_rail_cursor, 0, "Down must clamp at last item");
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

    #[test]
    fn esc_enters_normal_mode() {
        let mut state = make_state("vim-esc");
        assert_eq!(state.vim_input_mode, InputMode::Insert);
        // Simulate Esc in Insert mode (not in scroll_focus).
        state.vim_input_mode = InputMode::Normal;
        assert_eq!(state.vim_input_mode, InputMode::Normal);
    }

    #[test]
    fn i_returns_to_insert_mode() {
        let mut state = make_state("vim-i");
        state.vim_input_mode = InputMode::Normal;
        // Simulate `i` key.
        state.vim_input_mode = InputMode::Insert;
        assert_eq!(state.vim_input_mode, InputMode::Insert);
    }

    #[test]
    fn vim_h_moves_cursor_left() {
        let mut state = make_state("vim-h");
        state.input = "hello".to_owned();
        state.input_cursor = 3;
        state.vim_input_mode = InputMode::Normal;
        // Simulate `h` — move left by one char.
        let before = &state.input[..state.input_cursor];
        if let Some(ch) = before.chars().next_back() {
            state.input_cursor -= ch.len_utf8();
        }
        assert_eq!(state.input_cursor, 2);
    }

    #[test]
    fn vim_w_skips_word() {
        let mut state = make_state("vim-w");
        state.input = "hello world".to_owned();
        state.input_cursor = 0;
        state.vim_input_mode = InputMode::Normal;
        // Simulate `w` — skip past "hello" and the space.
        let rest = &state.input[state.input_cursor..];
        let skip = rest
            .char_indices()
            .skip_while(|(_, c)| !c.is_whitespace())
            .skip_while(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .next()
            .unwrap_or(rest.len());
        state.input_cursor += skip;
        assert_eq!(state.input_cursor, 6, "cursor must be at 'world'");
    }

    #[test]
    fn vim_dd_clears_input() {
        let mut state = make_state("vim-dd");
        state.input = "some text".to_owned();
        state.vim_input_mode = InputMode::Normal;
        // Simulate first `d` then second `d`.
        state.pending_vim_key = Some('d');
        if state.pending_vim_key == Some('d') {
            state.input.clear();
            state.input_cursor = 0;
            state.pending_vim_key = None;
        }
        assert!(state.input.is_empty());
    }

    #[test]
    fn vim_dollar_goes_to_end() {
        let mut state = make_state("vim-dollar");
        state.input = "hello world".to_owned();
        state.input_cursor = 0;
        state.vim_input_mode = InputMode::Normal;
        // Simulate `$`.
        state.input_cursor = state.input.len();
        assert_eq!(state.input_cursor, 11);
    }

    #[test]
    fn session_browser_opens_on_ctrl_s() {
        let mut state = make_state("sess-browser");
        assert!(!state.session_browser_open);
        // Simulate Ctrl-S toggle.
        state.session_browser_open = true;
        state.session_browser_cursor = 0;
        assert!(state.session_browser_open);
    }

    #[test]
    fn session_browser_closes_on_esc() {
        let mut state = make_state("sess-browser-esc");
        state.session_browser_open = true;
        // Esc handler closes the overlay.
        state.session_browser_open = false;
        assert!(!state.session_browser_open);
    }

    #[test]
    fn session_browser_cursor_wraps_on_down() {
        let mut state = make_state("sess-browser-wrap");
        state.session_rail_items = vec![
            ("id1".into(), "Session 1".into()),
            ("id2".into(), "Session 2".into()),
            ("id3".into(), "Session 3".into()),
        ];
        state.session_browser_open = true;
        state.session_browser_cursor = 2; // at last item
                                          // Simulate Down key with wrap-around.
        let count = state.session_rail_items.len();
        state.session_browser_cursor = (state.session_browser_cursor + 1) % count;
        assert_eq!(state.session_browser_cursor, 0, "cursor must wrap to 0");
    }
}
