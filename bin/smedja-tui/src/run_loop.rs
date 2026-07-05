//! The TUI event / poll / draw loop.
//!
//! Drains crossterm input, renders one frame per batch inside a synchronized
//! update, drains streamed turn events (or falls back to a blocking
//! `turn.subscribe` poll), and runs the periodic background polls (graph,
//! session rail, metrics, obs, LSP). Runs until quit or SIGTERM, then persists
//! rustyline history. Moved verbatim from `main.rs`; behavior is unchanged.

use super::*;

/// Drives the TUI until the user quits or SIGTERM arrives.
#[allow(clippy::too_many_lines)] // event loop + render + poll in a single binary entry point
pub(crate) async fn run(session: bootstrap::Session) -> Result<()> {
    let bootstrap::Session {
        mut state,
        mut client,
        mut editor,
        history_path,
        sock,
        mut terminal,
        sigterm_rx,
        guard: _guard,
    } = session;

    // Periodically force a full repaint so any cell that desynced between
    // ratatui's diff and the host terminal grid (observed as stale content
    // lingering in the top rows) is rewritten. The repaint is atomic thanks to
    // the synchronized-output bracket below, so it does not flicker.
    let mut last_full_repaint = std::time::Instant::now();
    loop {
        // Collect all ready crossterm events before drawing — one render per batch.
        let event_available =
            tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(100)))
                .await
                .context("poll task panicked")??;

        if event_available {
            // Drain every immediately-available event in the same tick.
            loop {
                let ev = tokio::task::spawn_blocking(event::read)
                    .await
                    .context("read task panicked")??;
                match ev {
                    Event::Key(key) => {
                        handle_key(key, &mut state, &mut client, &mut editor).await?;
                    }
                    Event::Mouse(mouse_ev) => match mouse_ev.kind {
                        MouseEventKind::ScrollDown => {
                            state.main_panel.scroll_down();
                        }
                        MouseEventKind::ScrollUp => {
                            state.main_panel.scroll_up();
                        }
                        // Left press inside the messages panel starts a
                        // character-precise drag selection at the clicked column.
                        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                            if let Some(pos) =
                                state.main_panel.pos_at(mouse_ev.column, mouse_ev.row)
                            {
                                // Selection renders from `selection_mode` alone —
                                // do NOT switch into scroll/keyboard mode, so the
                                // user can keep typing while/after marking.
                                state.selection_mode = true;
                                state.selection_anchor = pos;
                                state.selection_end = pos;
                            }
                        }
                        // Right-click on a tool card → expand its full args in an
                        // overlay (left-drag still selects; no conflict).
                        MouseEventKind::Down(crossterm::event::MouseButton::Right) => {
                            if let Some((line, _)) =
                                state.main_panel.pos_at(mouse_ev.column, mouse_ev.row)
                            {
                                if let Some((_, name, full)) = state
                                    .tool_details
                                    .iter()
                                    .find(|(l, _, _)| *l == line)
                                    .cloned()
                                {
                                    state.diff_overlay =
                                        Some((0, format_tool_detail(&name, &full)));
                                    state.diff_scroll = 0;
                                }
                            }
                        }
                        // Dragging extends the selection to the cell under cursor.
                        // Dragging past the top/bottom edge auto-scrolls so a
                        // selection can run beyond the visible region.
                        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                            if state.selection_mode {
                                if state.main_panel.row_above(mouse_ev.row) {
                                    state.main_panel.scroll_up();
                                } else if state.main_panel.row_below(mouse_ev.row) {
                                    state.main_panel.scroll_down();
                                }
                                if let Some(pos) = state
                                    .main_panel
                                    .pos_at_clamped(mouse_ev.column, mouse_ev.row)
                                {
                                    state.selection_end = pos;
                                }
                            }
                        }
                        // Release copies the marked text (only if an actual range
                        // was dragged — a bare click just places the anchor and is
                        // dismissed without clobbering the clipboard).
                        MouseEventKind::Up(crossterm::event::MouseButton::Left)
                            if state.selection_mode =>
                        {
                            if state.selection_anchor == state.selection_end {
                                state.selection_mode = false;
                            } else {
                                let text = state
                                    .main_panel
                                    .selection_text(state.selection_anchor, state.selection_end);
                                let count = text.lines().count().max(1);
                                let msg = match yank_to_clipboard(std::slice::from_ref(&text)) {
                                    Ok(_) => {
                                        format!("\u{2713} {count} lines copied to clipboard")
                                    }
                                    Err(e) => e,
                                };
                                state.clipboard = Some(text);
                                state.selection_mode = false;
                                push_system_message(&mut state, msg);
                            }
                        }
                        _ => {}
                    },
                    Event::Paste(text) => {
                        // Insert the whole paste as a single edit at the cursor.
                        // Because we don't process it key-by-key, embedded
                        // newlines stay literal (no accidental submit) — pasting
                        // a multi-line URL/snippet just lands in the input.
                        let cur = state.input_cursor.min(state.input.len());
                        state.input.insert_str(cur, &text);
                        state.input_cursor = cur + text.len();
                        let completions = filtered_completions(&state.input);
                        state.slash_completions = completions;
                    }
                    Event::Resize(_, _) => {
                        // Clamp scroll after resize so we don't end up past the
                        // last available line.
                        state.main_panel.clamp_scroll();
                    }
                    _ => {}
                }
                // Check if more events are ready without blocking.
                let more = tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(0)))
                    .await
                    .context("poll task panicked")??;
                if !more {
                    break;
                }
            }
        }

        // Bracket the frame in a synchronized update (`?2026h`/`?2026l`) so the
        // host terminal only presents complete frames. Without this, rapid
        // streaming redraws are parsed and rendered half-applied, tearing the
        // message area into overlapping garbage.
        let _ = execute!(stdout(), crossterm::terminal::BeginSynchronizedUpdate);
        // Force a full redraw a few times a second to self-heal any diff/grid
        // desync (clear() erases the screen + discards ratatui's prev-buffer so
        // the next draw emits every cell). Kept inside the synchronized-update
        // bracket so the erase+redraw is presented atomically — no flicker.
        if last_full_repaint.elapsed() >= Duration::from_millis(500) {
            let _ = terminal.clear();
            last_full_repaint = std::time::Instant::now();
        }
        terminal.draw(|f| render(f, &mut state))?;
        let _ = execute!(stdout(), crossterm::terminal::EndSynchronizedUpdate);

        // Drain NDJSON stream events from the background reader task.
        // When streaming is active (stream_rx is Some), render deltas in real
        // time and finalise the turn on the terminal event.  When streaming is
        // not available, fall back to the turn.subscribe blocking poll.
        let mut pending_output_save: Option<(OutputType, String)> = None;
        if state.stream_rx.is_some() {
            let mut turn_done = false;
            let mut stream_disconnected = false;
            loop {
                let event = match state
                    .stream_rx
                    .as_mut()
                    .expect("stream_rx present")
                    .try_recv()
                {
                    Ok(ev) => ev,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        if !turn_done {
                            stream_disconnected = true;
                        }
                        break;
                    }
                };
                if apply_stream_event(&mut state, event, &mut pending_output_save) {
                    turn_done = true;
                }
            }

            if stream_disconnected {
                // Sender dropped without a terminal event — daemon socket closed
                // unexpectedly. Surface a recoverable error rather than spinning forever.
                state.main_panel.push_line(
                    "[STREAM] daemon closed unexpectedly — ↑ to recall and retry".to_owned(),
                );
                if let Some(mut block) = state.current_block.take() {
                    block.fail();
                    state.block_store.push(block);
                }
                turn_done = true;
            }

            if turn_done {
                state.pending_task_id = None;
                state.stream_rx = None;
                state.turn_in_flight = false;
                state.poll_retry_count = 0;
                state.last_poll = None;

                // Refresh context rail after turn completes.
                if let Ok(ctx) = client
                    .call("session.context", json!({ "session_id": state.session_id }))
                    .await
                {
                    if let Some(used) = ctx["used_tok"].as_i64() {
                        state.context_used = u64::try_from(used.max(0)).unwrap_or(0);
                    }
                    if let Some(window) = ctx["window_tok"].as_u64() {
                        if window > 0 {
                            state.context_window = window;
                        }
                    }
                    state.obs_snapshot.context_used = state.context_used;
                    state.obs_snapshot.context_window = state.context_window;
                }
                // Surface model/tier/compaction transitions as transcript dividers.
                check_transitions(&mut state);
            }
        } else if let Some(task_id) = state.pending_task_id.clone() {
            // Fallback: blocking poll via turn.subscribe (no stream socket available).
            let should_call = state
                .last_poll
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(50));
            if should_call {
                state.last_poll = Some(std::time::Instant::now());
                match client
                    .call("turn.subscribe", json!({ "task_id": task_id }))
                    .await
                {
                    Ok(v) if v["done"].as_bool() == Some(true) => {
                        if let Some(error) = v["error"].as_str() {
                            let (label, hint) = classify_turn_error(error);
                            let header = format_turn_error(&state.runner, label, error);
                            let display = if hint.is_empty() {
                                header
                            } else {
                                format!("{header}\n  \u{2192} {hint}")
                            };
                            state.main_panel.push_line(display.clone());
                            push_system_message(&mut state, display);
                            if let Some(mut block) = state.current_block.take() {
                                block.fail();
                                state.block_store.push(block);
                            }
                        } else {
                            let response = v["response"].as_str().unwrap_or("").to_owned();
                            let input_tok = v["input_tok"].as_i64().unwrap_or(0);
                            let output_tok = v["output_tok"].as_i64().unwrap_or(0);
                            let turn_ms = state.turn_submitted_at.map_or(0, |t| {
                                u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                            });
                            state.turn_submitted_at = None;

                            if let Some(mut block) = state.current_block.take() {
                                block.push_text(&response);
                                block.complete(turn_ms);
                                for line in block.render_lines(80) {
                                    state.main_panel.push_line(line.clone());
                                    state.push_message(Message {
                                        role: Role::System,
                                        text: line,
                                    });
                                }
                                state.block_store.push(block);
                            } else {
                                if !response.is_empty() {
                                    state.main_panel.push_delta(&response);
                                }
                                state.push_message(Message {
                                    role: Role::System,
                                    text: response,
                                });
                            }

                            let footer =
                                format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms");
                            state.main_panel.push_line(footer);
                            state.last_traceparent = None;
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                        state.turn_in_flight = false;
                        state.poll_retry_count = 0;

                        if let Ok(ctx) = client
                            .call("session.context", json!({ "session_id": state.session_id }))
                            .await
                        {
                            if let Some(used) = ctx["used_tok"].as_i64() {
                                state.context_used = u64::try_from(used.max(0)).unwrap_or(0);
                            }
                            if let Some(window) = ctx["window_tok"].as_u64() {
                                if window > 0 {
                                    state.context_window = window;
                                }
                            }
                            state.obs_snapshot.context_used = state.context_used;
                            state.obs_snapshot.context_window = state.context_window;
                        }
                    }
                    Ok(_) => {
                        state.poll_retry_count += 1;
                        // Exponential backoff on non-terminal returns: 100 ms → 200 ms → … → 1 s.
                        let shift = state.poll_retry_count.saturating_sub(1).min(10);
                        let backoff_ms = (100u64 << shift).min(1_000);
                        #[allow(clippy::unchecked_time_subtraction)]
                        let poll_base =
                            std::time::Instant::now() - std::time::Duration::from_millis(50);
                        state.last_poll =
                            Some(poll_base + std::time::Duration::from_millis(backoff_ms));
                        if state.poll_retry_count % 5 == 1 {
                            let attempt = state.poll_retry_count;
                            push_action_log(
                                &mut state,
                                format!("waiting for turn… (poll attempt {attempt})"),
                            );
                        }
                        if state.poll_retry_count >= 60 {
                            state.main_panel.push_line(
                                "turn appears stuck — no response after 60 polls; giving up"
                                    .to_owned(),
                            );
                            state.pending_task_id = None;
                            state.last_poll = None;
                            state.turn_in_flight = false;
                            state.poll_retry_count = 0;
                        }
                    }
                    Err(e) => {
                        let text = if e.code == smedja_rpc::codes::TIMEOUT {
                            "turn timed out (>60 s) — daemon is still running the turn".to_owned()
                        } else {
                            format!("turn error: {e}")
                        };
                        // On transport-level disconnects attempt a reconnect before
                        // giving up, so a daemon restart does not require a TUI restart.
                        if e.code == smedja_rpc::codes::SERVER_DISCONNECTED {
                            state.main_panel.push_line(
                                "daemon disconnected — attempting reconnect…".to_owned(),
                            );
                            match try_reconnect(&sock).await {
                                Some(new_client) => {
                                    client = new_client;
                                    state
                                        .main_panel
                                        .push_line("reconnected to daemon".to_owned());
                                }
                                None => {
                                    state.main_panel.push_line(
                                        "reconnect failed — restart smedja-tui when the daemon is back".to_owned(),
                                    );
                                }
                            }
                        } else {
                            state.main_panel.push_line(text.clone());
                            state.push_message(Message {
                                role: Role::System,
                                text,
                            });
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                        state.turn_in_flight = false;
                        state.poll_retry_count = 0;
                    }
                }
            }
        }

        // Process any deferred generator output (drawio/pptx) now that the
        // stream_rx borrow has ended.
        if let Some((output_type, content)) = pending_output_save {
            save_generator_output(&output_type, &content, &mut state);
        }

        // Poll background upgrade result.
        let upgrade_done: Option<String> = if let Some(ref mut rx) = state.upgrade_rx {
            rx.try_recv().ok()
        } else {
            None
        };
        if let Some(msg) = upgrade_done {
            push_system_message(&mut state, msg);
            state.upgrade_rx = None;
        }

        // Graph status poll: reflect the real indexed symbol count for the
        // workspace every 5 s, so the right-bar shows "N symbols" after an index
        // built outside this session (e.g. `smj workspace index`) instead of
        // always "graph: /index to build".
        let should_poll_graph = state
            .last_graph_poll
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(5));
        if should_poll_graph {
            state.last_graph_poll = Some(std::time::Instant::now());
            let ws = state.graph_workspace.clone().or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
            });
            if let Some(ws) = ws {
                if let Ok(v) = client
                    .call("graph.status", json!({ "workspace": ws }))
                    .await
                {
                    if v.get("exists").and_then(Value::as_bool).unwrap_or(false) {
                        if let Some(n) = v.get("indexed").and_then(Value::as_u64) {
                            state.graph_symbols = usize::try_from(n).ok();
                        }
                    }
                }
            }
        }

        // Session rail poll: refresh the session list every 5 s when visible.
        let should_poll_sessions = state.panels.session_rail
            && state
                .last_session_rail_poll
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(5));
        if should_poll_sessions {
            state.last_session_rail_poll = Some(std::time::Instant::now());
            if let Ok(Value::Array(sessions)) = client.call("session.list", json!({})).await {
                state.session_rail_items = sessions
                    .iter()
                    .filter_map(|v| {
                        let id = v["id"].as_str()?.to_owned();
                        let runner = v["runner"].as_str().unwrap_or("?");
                        let label = format!("{runner}  {}", &id[..id.len().min(12)]);
                        Some((id, label))
                    })
                    .collect();
                // On first load (cursor still at 0) point at the current session.
                // On subsequent polls clamp to the new list length.
                let current_idx = state
                    .session_rail_items
                    .iter()
                    .position(|(id, _)| id == &state.session_id);
                if let Some(idx) = current_idx {
                    state.session_rail_cursor = idx;
                } else if !state.session_rail_items.is_empty() {
                    state.session_rail_cursor = state
                        .session_rail_cursor
                        .min(state.session_rail_items.len().saturating_sub(1));
                }
            }
        }

        // Metrics panel poll: a single slow (~3 s) cadence drives BOTH the
        // per-runner `metrics.summary` fetch (cost/usage rows) and the
        // token-economy `savings.summary` fetch (savings/efficiency section).
        // Gated on the panel being visible; never fetched while hidden. The
        // fetch only mutates the cached snapshots — it never blocks the render,
        // mirroring the cowork poll's tolerant `if let Ok(...)` handling.
        if metrics_poll_due(
            state.panels.metrics,
            state.last_metrics_poll,
            std::time::Instant::now(),
        ) {
            state.last_metrics_poll = Some(std::time::Instant::now());
            let now_micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0i64, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));

            // Per-runner rollups: hourly tier over the last 24h.
            let metrics_since = now_micros.saturating_sub(METRICS_SINCE_WINDOW_MICROS);
            if let Ok(resp) = client
                .call(
                    "metrics.summary",
                    json!({ "tier": "hourly", "since": metrics_since }),
                )
                .await
            {
                state.metrics_snapshot = metrics_rows_from_summary(&resp);
                // Extract 24h token total for the daily quota bar (buckets array).
                if let Some(buckets) = resp["buckets"].as_array() {
                    let total_24h: u64 = buckets
                        .iter()
                        .map(|b| {
                            let i = b["input_tok"].as_u64().unwrap_or(0);
                            let o = b["output_tok"].as_u64().unwrap_or(0);
                            i.saturating_add(o)
                        })
                        .sum();
                    if total_24h > 0 {
                        state.obs_snapshot.daily_tokens_used = Some(total_24h);
                    }
                }
            }

            // Token-economy savings: daily tier over the last 7 days, matching
            // `smj savings` defaults.
            let savings_since = now_micros.saturating_sub(7 * 86_400 * 1_000_000);
            if let Ok(resp) = client
                .call(
                    "savings.summary",
                    json!({ "tier": "daily", "since": savings_since }),
                )
                .await
            {
                state.savings_snapshot = metrics_view::savings_snapshot_from_json(&resp);
                // Mirror savings data into obs_snapshot.
                state.obs_snapshot.efficiency_ratio = state.savings_snapshot.efficiency_ratio;
                state.obs_snapshot.cache_saved = state.savings_snapshot.cache_saved;
            }
        }

        // Independent obs-panel poll (session.cost) — runs even when the metrics
        // overlay is closed so the obs rail always shows current cost.
        #[allow(clippy::items_after_statements)]
        const OBS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
        let obs_due = state
            .last_obs_poll
            .is_none_or(|t| t.elapsed() >= OBS_POLL_INTERVAL);
        if obs_due {
            state.last_obs_poll = Some(std::time::Instant::now());
            if let Ok(cost_resp) = client
                .call("session.cost", json!({ "session_id": &state.session_id }))
                .await
            {
                if let Some(usd) = cost_resp["cost_usd"].as_f64() {
                    state.obs_snapshot.session_cost_usd = usd;
                }
            }
            // Fetch daily token limit from daemon (reads SMEDJA_DAILY_TOKEN_LIMIT).
            if let Ok(quota_resp) = client.call("quota.limit", serde_json::Value::Null).await {
                state.obs_snapshot.daily_tokens_limit = quota_resp["daily_tokens"].as_u64();
            }
            // Refresh value panel with active-change token cost.
            if let Ok(vc) = client
                .call("cost.active_change", serde_json::Value::Null)
                .await
            {
                state.value_snapshot.change_name = vc["change_name"].as_str().map(str::to_owned);
                let token_cost = vc["token_cost"].as_u64().unwrap_or(0);
                state.value_snapshot.token_cost = token_cost;
                // Session blended $/token rate applied to this change's tokens.
                let total_tok = state
                    .session_tokens_in
                    .saturating_add(state.session_tokens_out);
                state.value_snapshot.cost_usd_micros = value_panel::blended_cost_micros(
                    state.obs_snapshot.session_cost_usd,
                    total_tok,
                    token_cost,
                );
                // Real running average of observed Tier-1 quality scores.
                let quality_avg = state
                    .quality_score_sum
                    .checked_div(state.quality_score_count)
                    .map_or(0u8, |avg| {
                        #[allow(clippy::cast_possible_truncation)] // avg of 0–100 fits u8
                        let a = avg as u8;
                        a
                    });
                state.value_snapshot.quality_avg = quality_avg;
                state.value_snapshot.estimated_value = value_panel::estimate_value(quality_avg);
            }
        }

        // Poll smdjad for LSP state every 5 s (single canonical source).
        #[allow(clippy::items_after_statements)]
        const LSP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
        let lsp_due = state
            .lsp_last_poll
            .is_none_or(|t| t.elapsed() >= LSP_POLL_INTERVAL);
        if lsp_due {
            state.lsp_last_poll = Some(std::time::Instant::now());
            if let (Ok(status_resp), Ok(diag_resp)) = (
                client.call("lsp.status", serde_json::Value::Null).await,
                client
                    .call("lsp.diagnostics", serde_json::Value::Null)
                    .await,
            ) {
                state.lsp_snapshot = lsp_snapshot_from_rpc(&status_resp, &diag_resp);
            }
        }

        if state.quit || *sigterm_rx.borrow() {
            break;
        }
    }

    // L127: persist history on clean shutdown.
    let _ = editor.save_history(&history_path);

    Ok(())
}
