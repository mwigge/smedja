//! Frame rendering: the top-level `render` and its per-panel/overlay helpers.
//!
//! Split out of `main.rs` verbatim; behaviour is unchanged.

use super::*;

mod cockpit;
mod overlays;
mod popups;

use cockpit::render_role_cockpit;
use overlays::{render_session_detail, render_session_peek};
use popups::{render_file_picker, render_slash_popup};

/// Compact count formatter for the live line's moving token counter
/// (`1.2k`, `18.4k`, `512`).
#[allow(clippy::cast_precision_loss)]
fn fmt_count(n: u32) -> String {
    if n >= 1_000 {
        format!("{:.1}k", f64::from(n) / 1000.0)
    } else {
        n.to_string()
    }
}

#[allow(clippy::too_many_lines)] // single-pass frame layout; splitting is out of scope here
pub(crate) fn render(frame: &mut ratatui::Frame, state: &mut AppState) {
    let area = frame.area();
    let p = palette();

    // Flood-fill the entire frame with the forge background so no terminal
    // default colour bleeds through panel gaps or empty areas.
    frame.render_widget(Block::default().style(Style::default().bg(p.bg)), area);

    // Build the input echo (prefix + visible cursor) and compute how many
    // visual rows it needs, so the input field grows and wraps instead of
    // running off the right edge ("typing blind"). The cursor's row drives an
    // internal scroll once the field hits its row cap.
    // Wrap at the main-content column width, not the full terminal width.
    // When rails are visible they take columns from the right/left of body_area;
    // subtracting their widths here keeps the height calculation and the visual
    // rendering in sync, so the input grows a row at the same point the text
    // visually wraps instead of running under the rail.
    // Decide rail visibility ONCE, up front: the session rail carves
    // SESSION_RAIL_W columns off the left of the body, and the context rail
    // needs 100 columns of what remains. The input-width calculation below
    // and the body split further down must agree on these, or the input
    // wraps at a width that has no relation to what is actually on screen
    // (previously the input reserved rail width even when the rail was not
    // drawn, because the two checks used different widths).
    const SESSION_RAIL_W: u16 = 28;
    let session_rail_on = state.panels.session_rail && area.width >= SESSION_RAIL_W + 40;
    let content_w = area
        .width
        .saturating_sub(if session_rail_on { SESSION_RAIL_W } else { 0 });
    let context_rail_on = state.panels.context_rail && content_w >= 100;
    let right_rail_w = if context_rail_on {
        context_rail::ContextRail::WIDTH
    } else {
        0
    };
    let input_w = area.width.saturating_sub(right_rail_w).max(1) as usize;
    let (input_display, input_cursor_row) = if let Some(ref var) = state.secret_var {
        // Masked secret entry — never echo the value (e.g. an API key).
        let dots = "\u{2022}".repeat(state.input.chars().count());
        (format!("{var} (hidden): {dots}\u{2588}"), 0usize)
    } else {
        let cur = state.input_cursor.min(state.input.len());
        let head = format!("> {}", &state.input[..cur]);
        let cursor_row = wrap_input_rows(&head, input_w).len().saturating_sub(1);
        (format!("{head}_{}", &state.input[cur..]), cursor_row)
    };
    // Pre-wrap the echo with the same char-level algorithm the render pass
    // uses below, so the height calculation and the drawn rows can never
    // disagree. (ratatui's WordWrapper wraps at whitespace and would push a
    // long unbroken token — a URL, a pasted blob — onto a visual row the
    // field never grew, rendering the prompt as "> " plus blanks.)
    let wrapped_input_rows = wrap_input_rows(&input_display, input_w);
    let input_rows: u16 = if state.history_search_mode {
        2
    } else if state.secret_var.is_some() {
        1
    } else {
        u16::try_from(wrapped_input_rows.len())
            .unwrap_or(INPUT_MAX_ROWS)
            .clamp(1, INPUT_MAX_ROWS)
    };
    // Scroll the field so the cursor's row stays visible once input overflows.
    let input_scroll = u16::try_from(input_cursor_row)
        .unwrap_or(0)
        .saturating_sub(input_rows.saturating_sub(1));

    // L122: outer vertical split:
    //   row 0 = status bar (1 row)
    //   row 1 = body (fill)
    //   row 2 = action log (5 rows)
    //   row 3 = input (grows to wrap, capped at INPUT_MAX_ROWS)
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(input_rows),
    ])
    .split(area);

    let status_area = outer[0];
    let body_area = outer[1];
    let action_log_area = outer[2];
    let (input_area, search_bar_area) = if state.history_search_mode && outer[3].height >= 2 {
        let parts =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(outer[3]);
        (parts[0], Some(parts[1]))
    } else {
        (outer[3], None)
    };

    // -- Status bar -----------------------------------------------------------
    let ctx_pct = (state.context_used * 100)
        .checked_div(state.context_window)
        .map(|p| u8::try_from(p.min(100)).unwrap_or(100));
    let ctx = ModuleCtx {
        session_id: &state.session_id,
        mode: state.mode.as_deref(),
        tier: state.tier.as_deref(),
        runner: Some(&state.runner),
        pending: state.pending_task_id.is_some(),
        input_mode: !state.scroll_focus,
        ctx_pct,
    };
    // Starship-style segmented status line (left), with a dim discoverability
    // hint right-aligned over the same row. Paint the panel background first so
    // both passes share it.
    let status_bg = if state.no_color {
        Style::default()
    } else {
        Style::default().bg(p.panel)
    };
    frame.render_widget(
        Paragraph::new(status_bar_line(&ctx, state.no_color)).style(status_bg),
        status_area,
    );
    frame.render_widget(
        Paragraph::new(status_hint_line(state.no_color))
            .alignment(ratatui::layout::Alignment::Right),
        status_area,
    );

    // -- Body: optional session rail | main panel | optional context rail ------
    // Rail visibility was decided at the top of render() next to the input
    // width calculation — reuse those decisions here so the two never drift.
    let (session_rail_area_opt, content_area) = if session_rail_on {
        let cols = Layout::horizontal([Constraint::Length(SESSION_RAIL_W), Constraint::Fill(1)])
            .split(body_area);
        (Some(cols[0]), cols[1])
    } else {
        (None, body_area)
    };

    // Then carve out the optional right context rail.
    let (main_area, rail_area) = if context_rail_on {
        let cols = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(context_rail::ContextRail::WIDTH),
        ])
        .split(content_area);
        (cols[0], Some(cols[1]))
    } else {
        (content_area, None)
    };

    // Render session rail when visible.
    if let Some(sr_area) = session_rail_area_opt {
        let cursor = state.session_rail_cursor;
        let lines: Vec<Line<'_>> = state
            .session_rail_items
            .iter()
            .enumerate()
            .map(|(i, (_, label))| {
                if i == cursor {
                    Line::from(Span::styled(
                        format!("▶ {label}"),
                        // Signature molten lava-orange for the active/selected row.
                        Style::default().fg(p.molten).add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(Span::raw(format!("  {label}")))
                }
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.border_dim))
            .title(" sessions [Ctrl-W] ");
        frame.render_widget(Paragraph::new(lines).block(block), sr_area);
    }

    // L122: render MainPanel from state.main_panel.
    let selection = if state.selection_mode {
        Some((state.selection_anchor, state.selection_end))
    } else {
        None
    };
    let search_q = if state.panel_search_query.is_empty() {
        None
    } else {
        Some(state.panel_search_query.as_str())
    };
    // Animate the in-flight tool card so a running tool shows liveness: the
    // running card is drawn full-width with a right-aligned RUNNING pill, its
    // molten star spinner cycling each render tick until the result settles the
    // card to the compact ✓/✗ collapse.
    if state.turn_in_flight {
        if let Some((idx, name, input)) = state.pending_tool.clone() {
            const TOOL_SPINNER: [char; 6] = ['·', '✻', '✽', '✶', '✳', '✢'];
            let frame_char = TOOL_SPINNER[state.spinner_tick as usize % TOOL_SPINNER.len()];
            let card_w = (main_area.width as usize).saturating_sub(1).max(12);
            let card = tool_call::card_header(
                &name,
                &input,
                card_w,
                state.no_color,
                tool_call::CardStatus::Running(frame_char),
            );
            state.main_panel.replace_styled_line(idx, card);
        }
    }

    state
        .main_panel
        .render(main_area, frame, selection, search_q, state.no_color);

    // -- Live line: the dedicated bottom row while a turn is active -----------
    if state.turn_in_flight {
        // Advance the shared spinner tick once per frame (drives the live line,
        // the running tool card, and the plan's current-step spinner).
        state.spinner_tick = state.spinner_tick.wrapping_add(1);
        let running = state.pending_tool.is_some();
        let live_state = if running {
            live_line::LiveState::RunningTool
        } else {
            live_line::LiveState::Thinking
        };
        let elapsed_s = state
            .turn_submitted_at
            .map_or(0.0, |t| t.elapsed().as_secs_f32());
        let stalled_secs = state
            .last_stream_activity
            .map_or(0, |t| t.elapsed().as_secs());
        let (verb, counter) = if running {
            let name = state
                .pending_tool
                .as_ref()
                .map_or("tool", |(_, n, _)| n.as_str());
            let kind = tool_call::tool_kind_of(name);
            let tool_s = state
                .tool_started_at
                .map_or(0.0, |t| t.elapsed().as_secs_f32());
            (
                format!("running {}", kind.label()),
                live_line::fmt_secs(tool_s),
            )
        } else {
            let verb = if state.current_thinking.is_empty() {
                "streaming".to_owned()
            } else {
                "thinking".to_owned()
            };
            (verb, format!("{} tok", fmt_count(state.live_tokens)))
        };
        live_line::render(
            main_area,
            true,
            live_state,
            &verb,
            elapsed_s,
            &counter,
            stalled_secs,
            state.spinner_tick,
            state.no_color,
            frame,
        );
    }
    thoughts_panel::render_step_overlay(
        main_area,
        state.thinking_expanded,
        &state.thinking_steps,
        state.no_color,
        frame,
    );

    // -- Action log -----------------------------------------------------------
    // L122: 5-row area using the existing ActionLog widget.
    state.action_log.render(action_log_area, frame);

    // -- Input area (auto-growing + wrapped; display/height computed above) ----
    // Prompt feedback: right-aligned char + estimated token count. Shown only
    // when the input is a single row, so it can never overlap wrapped text.
    let counter_text = if state.input.is_empty() {
        String::new()
    } else {
        let chars = state.input.chars().count();
        #[allow(clippy::integer_division)]
        let est_tok = chars / 4;
        format!("{chars}c ≈{est_tok}tok")
    };
    #[allow(clippy::cast_possible_truncation)]
    let counter_len = counter_text.chars().count() as u16;
    let counter_style = if state.no_color {
        Style::default()
    } else {
        Style::default().fg(p.text_dim).add_modifier(Modifier::DIM)
    };
    // Colour the leading "> " prompt indicator with the signature molten
    // lava-orange (primary accent); the typed text keeps the default fg.
    // The echo is pre-wrapped above (char-level, identical to the height
    // calculation), so the Paragraph renders it verbatim WITHOUT ratatui's
    // word wrapper — the two can never disagree about the row count.
    let mut input_lines: Vec<Line<'static>> = Vec::with_capacity(wrapped_input_rows.len());
    for (row_idx, row) in wrapped_input_rows.iter().enumerate() {
        if row_idx == 0 && !state.no_color && state.secret_var.is_none() {
            if let Some(rest) = row.strip_prefix("> ") {
                input_lines.push(Line::from(vec![
                    Span::styled(
                        "> ",
                        Style::default().fg(p.molten).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(rest.to_owned()),
                ]));
                continue;
            }
        }
        input_lines.push(Line::from(row.clone()));
    }
    let input_para = Paragraph::new(input_lines).scroll((input_scroll, 0));
    // Narrow the render rect to match input_w so the pre-wrapped rows agree
    // with the height calculation above.
    let effective_input_w = u16::try_from(input_w).unwrap_or(input_area.width);
    let effective_input_area = ratatui::layout::Rect::new(
        input_area.x,
        input_area.y,
        effective_input_w.min(input_area.width),
        input_area.height,
    );
    // The counter shares the row with the prompt; only show it when the
    // single visible row is short enough that the counter can't overlap it
    // (previously the gate ignored the content width, so a nearly-full row
    // wrapped under the counter and the tail scrolled out of view).
    let row0_w = u16::try_from(unicode_width::UnicodeWidthStr::width(
        wrapped_input_rows.first().map_or("", String::as_str),
    ))
    .unwrap_or(u16::MAX);
    if input_rows == 1 && counter_len > 0 && row0_w + 2 + counter_len < effective_input_w {
        let input_sub_w = effective_input_w - counter_len;
        let input_sub = ratatui::layout::Rect::new(
            effective_input_area.x,
            effective_input_area.y,
            input_sub_w,
            effective_input_area.height,
        );
        let counter_rect = ratatui::layout::Rect::new(
            effective_input_area.x + input_sub_w,
            effective_input_area.y,
            counter_len,
            effective_input_area.height,
        );
        frame.render_widget(input_para, input_sub);
        frame.render_widget(
            Paragraph::new(Span::styled(counter_text, counter_style)),
            counter_rect,
        );
    } else {
        frame.render_widget(input_para, effective_input_area);
    }

    if let Some(search_area) = search_bar_area {
        let matched = history_search(&state.prompt_history, &state.history_search_query)
            .map_or("", |(_, s)| s);
        let search_text = format!(
            "(reverse-i-search) `{}`: {}",
            state.history_search_query, matched
        );
        let search_widget = Paragraph::new(search_text)
            .style(Style::default().fg(p.text).add_modifier(Modifier::DIM));
        frame.render_widget(search_widget, search_area);
    }

    // -- Right rail: context | role cockpit | obs | trace | fleet | plan | quality | value | LSP
    // Context (1 row) is always present; the other panels are individually
    // toggled. Panels are STACKED MANUALLY top-to-bottom in priority order:
    // ratatui's cassowary Layout solver does not treat `Length` as exact —
    // it repeatedly starved the mid-rail obs panel to zero height while
    // inflating its neighbours. Manual stacking is deterministic: every
    // enabled panel gets its fixed height while room remains, and LSP (the
    // flexible panel) takes whatever is left at the bottom.
    if let Some(rail_rect) = rail_area {
        let show_cockpit = state.panels.role_cockpit;
        let show_lsp = state.panels.lsp;
        let show_obs = state.panels.obs;
        let show_quality = state.panels.quality;
        let show_value = state.panels.value;
        let show_plan = state.plan_steps.len() >= 2;
        let show_fleet = state.panels.fleet && !state.fleet.is_empty();
        // The trace waterfall rides with the obs panel (smedja's OTel moat) once
        // the current turn has recorded any spans.
        let show_trace = show_obs && !state.current_trace.is_empty();

        let rail_bottom = rail_rect.y + rail_rect.height;
        let mut rail_y = rail_rect.y;
        // Carve `h` rows off the top of the remaining rail; `None` when full.
        let mut take = |h: u16| -> Option<ratatui::layout::Rect> {
            let avail = rail_bottom.saturating_sub(rail_y);
            if avail == 0 {
                return None;
            }
            let hh = h.min(avail);
            let chunk = ratatui::layout::Rect::new(rail_rect.x, rail_y, rail_rect.width, hh);
            rail_y += hh;
            Some(chunk)
        };

        // ── Metrics / runner panel (top of rail) ─────────────────────────
        let show_metrics = state.panels.metrics;
        if show_metrics {
            let metrics_lines = metrics_view::MetricsView::with_savings_and_tiers(
                state.metrics_snapshot.clone(),
                state.savings_snapshot.clone(),
                state.tier_snapshot.clone(),
            )
            .with_hourly(state.metrics_hourly.clone())
            .lines()
            .len();
            // +2 for Block top and bottom border.
            let h = u16::try_from(metrics_lines + 2)
                .unwrap_or(11)
                .min(rail_rect.height / 2);
            if let Some(chunk) = take(h) {
                frame.render_widget(
                    metrics_view::MetricsView::with_savings_and_tiers(
                        state.metrics_snapshot.clone(),
                        state.savings_snapshot.clone(),
                        state.tier_snapshot.clone(),
                    )
                    .with_hourly(state.metrics_hourly.clone()),
                    chunk,
                );
            }
        }

        // ── Context slot (always, 1 row) ──────────────────────────────────
        // Clamp to usize::MAX — well within range on 64-bit targets.
        if let Some(chunk) = take(1) {
            let slots = vec![context_rail::ContextSlot {
                name: "context".into(),
                used: usize::try_from(state.context_used).unwrap_or(usize::MAX),
                total: usize::try_from(state.context_window).unwrap_or(usize::MAX),
            }];
            frame.render_widget(context_rail::ContextRail::new(slots), chunk);
        }

        // ── Role cockpit panel ────────────────────────────────────────────
        if show_cockpit {
            if let Some(chunk) = take(7) {
                render_role_cockpit(frame, chunk, state);
            }
        }

        // ── Observability panel ───────────────────────────────────────────
        if show_obs {
            if let Some(chunk) = take(6) {
                obs_panel::ObsPanel::new(&state.obs_snapshot).render(chunk, frame);
            }
        }

        // ── Turn trace waterfall (the in-terminal OTel viewer) ────────────
        if show_trace {
            // Border (2) + one row per span + up to 3 detail rows when expanded.
            #[allow(clippy::cast_possible_truncation)]
            let span_rows = state.current_trace.spans.len() as u16;
            let detail_h = if state.trace_expanded { 3 } else { 0 };
            let trace_h = (span_rows + 2 + detail_h).min(rail_rect.height / 3).max(3);
            if let Some(chunk) = take(trace_h) {
                let sel = Some(
                    state
                        .trace_selected
                        .min(state.current_trace.spans.len().saturating_sub(1)),
                );
                trace_waterfall::render(
                    chunk,
                    frame,
                    &state.current_trace,
                    sel,
                    state.trace_expanded,
                    state.no_color,
                );
                if state.trace_expanded {
                    // Overlay the selected span's detail on the panel's lower rows.
                    let detail_lines = trace_waterfall::span_detail_lines(
                        &state.current_trace,
                        state.trace_selected,
                        state.no_color,
                    );
                    if chunk.height > 4 {
                        let dh = u16::try_from(detail_lines.len()).unwrap_or(3).min(3);
                        let drect = ratatui::layout::Rect::new(
                            chunk.x + 1,
                            chunk.y + chunk.height.saturating_sub(dh + 1),
                            chunk.width.saturating_sub(2),
                            dh,
                        );
                        frame.render_widget(Paragraph::new(detail_lines), drect);
                    }
                }
            }
        }

        // ── Multi-agent fleet roster ──────────────────────────────────────
        if show_fleet {
            #[allow(clippy::cast_possible_truncation)]
            let rows = state.fleet.len() as u16;
            let fleet_h = (rows + 3).min(rail_rect.height / 3).max(4);
            if let Some(chunk) = take(fleet_h) {
                fleet_panel::FleetPanel {
                    fleet: &state.fleet,
                    mode: state.render_mode,
                    no_color: state.no_color,
                }
                .render(chunk, frame);
            }
        }

        // ── Plan step tracker ─────────────────────────────────────────────
        if show_plan {
            let plan_h = plan_panel::panel_height(state.plan_steps.len());
            if let Some(chunk) = take(plan_h) {
                let spinner =
                    ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'][state.spinner_tick as usize % 8];
                plan_panel::PlanPanel::new(&state.plan_steps, state.plan_current, spinner)
                    .render(chunk, frame);
            }
        }

        // ── Quality gate panel ────────────────────────────────────────────
        if show_quality {
            if let Some(chunk) = take(8) {
                quality_panel::QualityPanel::new(&state.quality_snapshot).render(chunk, frame);
            }
        }

        // ── Value / ROI panel ─────────────────────────────────────────────
        if show_value {
            if let Some(chunk) = take(7) {
                value_panel::ValuePanel::new(&state.value_snapshot).render(chunk, frame);
            }
        }

        // ── LSP panel (last — takes all rows the other panels leave) ──────
        if show_lsp {
            let remaining = rail_bottom.saturating_sub(rail_y);
            if remaining > 0 {
                let chunk =
                    ratatui::layout::Rect::new(rail_rect.x, rail_y, rail_rect.width, remaining);
                lsp_panel::LspPanel::new(&state.lsp_snapshot)
                    .with_graph(state.graph_symbols)
                    .render(chunk, frame);
            }
        }
    }

    // -- Session detail overlay -----------------------------------------------
    if let Some(ref detail) = state.session_detail_overlay {
        render_session_detail(frame, area, detail, p);
    }

    // -- Session config peek overlay (Ctrl+P in scroll mode) -----------------
    if state.show_session_peek {
        render_session_peek(frame, area, state, p);
    }

    // -- Cowork gate overlay --------------------------------------------------
    if !state.pending_cowork.is_empty() {
        let cw_rect = cowork_widget::overlay_rect(body_area);
        frame.render_widget(
            cowork_widget::CoworkWidget {
                items: &state.pending_cowork,
                modify_mode: state.cowork_modify_mode,
                modify_input: &state.cowork_modify_input,
            },
            cw_rect,
        );
    }

    // -- Diff overlay ---------------------------------------------------------
    if let Some((_idx, ref lines)) = state.diff_overlay {
        // Centre 80% of the main area.
        #[allow(
            clippy::cast_lossless,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let ow = (f32::from(area.width) * 0.8) as u16;
        #[allow(
            clippy::cast_lossless,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let oh = (f32::from(area.height) * 0.8) as u16;
        let ox = area.x + (area.width.saturating_sub(ow)) / 2;
        let oy = area.y + (area.height.saturating_sub(oh)) / 2;
        let overlay_rect = ratatui::layout::Rect::new(ox, oy, ow, oh);

        if state.diff_split_view && diff_viewer::is_diff_content(lines) {
            diff_viewer::render_split(
                lines,
                state.diff_scroll,
                overlay_rect,
                state.no_color,
                frame,
            );
        } else {
            diff_viewer::render_unified(
                lines,
                state.diff_scroll,
                overlay_rect,
                state.no_color,
                frame,
            );
        }
    }

    // -- Block browser overlay ------------------------------------------------
    if state.block_browser_open && !state.block_store.is_empty() {
        let total = state.block_store.len();
        let cursor = state.block_browser_cursor;
        let overlay_lines: Vec<Line<'_>> = state
            .block_store
            .blocks()
            .enumerate()
            .map(|(i, b)| {
                let status_icon = match &b.status {
                    blocks::BlockStatus::Complete => "\u{2713}",
                    blocks::BlockStatus::Failed => "\u{2717}",
                    blocks::BlockStatus::Streaming => "\u{22ef}",
                    blocks::BlockStatus::ToolCall { .. } => "\u{25c6}",
                };
                let text = format!(" {status_icon} turn {}", b.turn_n);
                if i == cursor {
                    Line::from(Span::styled(
                        text,
                        Style::default()
                            .fg(p.bg)
                            .bg(p.text_bright)
                            .add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(Span::styled(text, Style::default().fg(p.text)))
                }
            })
            .collect();
        let bb_title = format!("blocks {}/{}", cursor.saturating_add(1).min(total), total);
        #[allow(clippy::cast_possible_truncation)]
        let bb_h = (total + 2).min(body_area.height as usize) as u16;
        let bb_w = 24u16.min(body_area.width);
        let bb_rect = ratatui::layout::Rect::new(
            body_area.x + body_area.width.saturating_sub(bb_w),
            body_area.y,
            bb_w,
            bb_h,
        );
        frame.render_widget(Clear, bb_rect);
        frame.render_widget(
            Paragraph::new(overlay_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .title(bb_title),
            ),
            bb_rect,
        );
    }

    // -- Panel search bar -----------------------------------------------------
    if state.panel_search_mode {
        // Show the search query as a one-row overlay at the top of the main panel.
        let sb_rect = ratatui::layout::Rect::new(main_area.x, main_area.y, main_area.width, 1);
        let search_text = format!("/ {}_", state.panel_search_query);
        let search_style = if state.no_color {
            Style::default()
        } else {
            Style::default().fg(p.bg).bg(p.text_bright)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(search_text, search_style))),
            sb_rect,
        );
    }

    // -- Slash-completion popup -----------------------------------------------
    if state.slash_popup_visible && !state.slash_completions.is_empty() {
        render_slash_popup(frame, area, state);
    }

    // -- File picker overlay --------------------------------------------------
    if state.file_picker_open {
        render_file_picker(frame, area, state);
    }
}
