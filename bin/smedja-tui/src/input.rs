//! Key input handling: `handle_key` and its slash-popup / cowork helpers.
//!
//! Split out of `main.rs` verbatim; behaviour is unchanged.

use super::*;

pub(crate) fn accept_slash_completion(state: &mut AppState, append_space: bool) -> bool {
    let Some(completion) = state.slash_completions.get(state.slash_cursor).cloned() else {
        state.slash_popup_visible = false;
        return false;
    };
    completion.clone_into(&mut state.input);
    if append_space {
        state.input.push(' ');
    }
    state.input_cursor = state.input.len();
    state.slash_popup_visible = false;
    state.slash_completions.clear();
    state.slash_cursor = 0;
    true
}

pub(crate) fn clear_slash_popup(state: &mut AppState) {
    state.slash_popup_visible = false;
    state.slash_completions.clear();
    state.slash_cursor = 0;
    state.input.clear();
    state.input_cursor = 0;
    state.runner_picker_mode = false;
    state.session_picker_mode = false;
    state.command_palette_mode = false;
    state.session_picker_ids.clear();
}

// dispatch_slash, apply_tier, apply_agent, and their exclusive format helpers
// (format_model_list, format_local_model_list, format_agents_table,
// format_metrics, format_approvals_list) have been extracted to src/slash.rs.
// They are re-exported at the top of this file via `pub(crate) use slash::...`
// so callers and the test module (which uses `use super::*`) see them unchanged.

// ---------------------------------------------------------------------------
// Cowork resolver
// ---------------------------------------------------------------------------

/// Reads the daemon's `resolved` flag from a `cowork.*` RPC result.
///
/// Returns `true` only when the response is `Ok` and carries `"resolved": true`.
/// A `resolved: false`, a missing field, or any transport error all yield `false`
/// so the caller keeps the pending item rather than dropping it silently.
pub(crate) fn cowork_resolved(result: &Result<serde_json::Value, smedja_rpc::RpcError>) -> bool {
    match result {
        Ok(v) => v
            .get("resolved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Decides whether a cowork item should be removed and what transcript line to emit.
///
/// `success` is the confirmation text used when the daemon resolved the decision
/// (`approved: <tool>`, `denied: <tool>`, or `modify sent: <instruction>`). On
/// `resolved: false` the item is retained with an `item not found: <tool>` line;
/// on a transport error it is retained with a `<method> error: <e>` line. Returns
/// `(remove, message)`.
pub(crate) fn apply_cowork_decision(
    result: &Result<serde_json::Value, smedja_rpc::RpcError>,
    method: &str,
    success: &str,
    tool: &str,
) -> (bool, String) {
    match result {
        Ok(_) if cowork_resolved(result) => (true, success.to_owned()),
        Ok(_) => (false, format!("item not found: {tool}")),
        Err(e) => (false, format!("{method} error: {e}")),
    }
}

/// Sends a `cowork.*` decision RPC, injecting `session_id` into `params`.
///
/// Returns the raw RPC result so the caller can both check the `resolved` flag
/// (via [`cowork_resolved`]) and surface the appropriate transcript line. The
/// `session_id` is merged into `params` so call sites pass only the decision
/// fields (`id`, optional `reason`/`instruction`).
async fn resolve_cowork(
    client: &mut Client,
    session_id: &str,
    method: &str,
    mut params: serde_json::Value,
) -> Result<serde_json::Value, smedja_rpc::RpcError> {
    if let Some(obj) = params.as_object_mut() {
        obj.insert("session_id".to_owned(), json!(session_id));
    }
    client.call(method, params).await
}

// ---------------------------------------------------------------------------
// Key handler
// ---------------------------------------------------------------------------

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
    // Trace-waterfall span inspector: `x` steps through the current turn's
    // spans and expands the selected span's detail. The trace panel is drawn
    // whenever obs is on and the turn recorded spans — including in input mode
    // — so this is handled here rather than only inside the scroll block. To
    // avoid stealing a typed 'x', it only fires when the trace panel is visible
    // and the user is not composing (scroll mode, or an empty input line).
    // ------------------------------------------------------------------
    if key.code == KeyCode::Char('x')
        && key.modifiers.is_empty()
        && state.panels.obs
        && !state.current_trace.is_empty()
        && (state.scroll_focus || state.input.is_empty())
    {
        let n = state.current_trace.spans.len();
        if n > 0 {
            if state.trace_expanded {
                // Step to the next span; wrapping past the last collapses.
                state.trace_selected = (state.trace_selected + 1) % n;
                if state.trace_selected == 0 {
                    state.trace_expanded = false;
                }
            } else {
                state.trace_expanded = true;
                state.trace_selected = 0;
            }
        }
        return Ok(());
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
            // (`x` — the trace-waterfall span inspector — is handled by a
            // mode-independent intercept above so it also works in input mode.)
            // A (Shift-A): collapse / expand the action log. Uppercase so it does
            // not collide with lowercase 'a' (exit scroll mode) or Ctrl-A (role
            // cockpit).
            KeyCode::Char('A') => {
                state.action_log.visible = !state.action_log.visible;
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

        // Ctrl-G: toggle the multi-agent fleet roster in the right rail.
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.panels.fleet = !state.panels.fleet;
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
            if state.show_session_peek {
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
            } else if state.scroll_focus {
                state.scroll_focus = false;
            } else if state.block_browser_open {
                state.block_browser_open = false;
            } else {
                state.scroll_focus = true;
            }
        }

        KeyCode::Up => {
            if state.block_browser_open {
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
            if state.block_browser_open {
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
                        state.push_message(msg);
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
                    state.push_message(msg);
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
                            state.push_message(msg);
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
                        state.push_message(msg);
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
                    state.push_message(msg);
                } else {
                    let msg = Message {
                        role: Role::System,
                        text: "usage: /stage <tool> <json-args>".into(),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.push_message(msg);
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
                state.push_message(msg);
                for item in state.staging_queue.list() {
                    let msg = Message {
                        role: Role::System,
                        text: item,
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.push_message(msg);
                }
            } else if input.trim() == "/run" {
                let actions = state.staging_queue.drain();
                if actions.is_empty() {
                    let msg = Message {
                        role: Role::System,
                        text: "no staged actions".into(),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.push_message(msg);
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
                        state.push_message(msg);
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
                state.push_message(msg);
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
                state.push_message(msg);
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
                state.push_message(msg);
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
