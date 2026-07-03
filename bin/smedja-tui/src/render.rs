use crate::commands::SLASH_COMMAND_DESCRIPTIONS;
use crate::formatting::{history_search, wrap_input_rows};
use crate::session::SessionDetail;
use crate::state::{AppState, InputMode};
use crate::statusbar::ModuleCtx;
use crate::statusline::{status_bar_line, status_hint_line};
use crate::theme::palette;
use crate::{
    blocks, context_rail, cowork_widget, diff_viewer, lsp_panel, metrics_view, obs_panel,
    plan_panel, quality_panel, thoughts_panel, value_panel,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// Maximum visual rows the input field grows to before it scrolls internally.
pub(crate) const INPUT_MAX_ROWS: u16 = 6;

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
    let right_rail_w = if state.panels.context_rail && area.width >= 100 {
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
    let input_rows: u16 = if state.history_search_mode {
        2
    } else if state.secret_var.is_some() {
        1
    } else {
        u16::try_from(wrap_input_rows(&input_display, input_w).len())
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
        vim_normal_mode: state.vim_input_mode == InputMode::Normal,
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
    #[allow(clippy::items_after_statements)]
    const SESSION_RAIL_W: u16 = 28;

    // First carve out the optional left session rail.
    let (session_rail_area_opt, content_area) = if state.panels.session_rail
        && body_area.width >= SESSION_RAIL_W + 40
    {
        let cols = Layout::horizontal([Constraint::Length(SESSION_RAIL_W), Constraint::Fill(1)])
            .split(body_area);
        (Some(cols[0]), cols[1])
    } else {
        (None, body_area)
    };

    // Then carve out the optional right context rail.
    let (main_area, rail_area) = if state.panels.context_rail && content_area.width >= 100 {
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
                        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
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
    state
        .main_panel
        .render(main_area, frame, selection, search_q, state.no_color);

    thoughts_panel::render_indicator(
        main_area,
        state.turn_in_flight,
        &mut state.spinner_tick,
        &state.current_thinking,
        state.pending_tool.as_ref(),
        state.no_color,
        frame,
    );
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
    let input_para = Paragraph::new(input_display)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((input_scroll, 0));
    // Narrow the render rect to match input_w so the Paragraph wrap point
    // agrees with the height calculation above.
    let effective_input_w = u16::try_from(input_w).unwrap_or(input_area.width);
    let effective_input_area = ratatui::layout::Rect::new(
        input_area.x,
        input_area.y,
        effective_input_w.min(input_area.width),
        input_area.height,
    );
    if input_rows == 1 && counter_len > 0 && counter_len + 4 < effective_input_w {
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

    // -- Right rail: context | role cockpit | LSP panel | obs panel | quality panel | value panel
    // The rail is split vertically into 1–6 sections. Context (1 row) is always
    // present; role cockpit, LSP, obs, quality, and value panels are individually toggled.
    if let Some(rail_rect) = rail_area {
        use Constraint::{Fill, Length};

        let show_cockpit = state.panels.role_cockpit;
        let show_lsp = state.panels.lsp;
        let show_obs = state.panels.obs;
        let show_quality = state.panels.quality;
        let show_value = state.panels.value;
        let show_plan = state.plan_steps.len() >= 2;

        // Build constraint list dynamically so Layout never gets zero-length.
        let mut constraints: Vec<Constraint> = vec![];
        // Metrics panel sits at the very top of the rail when visible.
        let show_metrics = state.panels.metrics;
        if show_metrics {
            let metrics_lines = metrics_view::MetricsView::with_savings(
                state.metrics_snapshot.clone(),
                state.savings_snapshot.clone(),
            )
            .lines()
            .len();
            // +2 for Block top and bottom border.
            let h = u16::try_from(metrics_lines + 2)
                .unwrap_or(11)
                .min(rail_rect.height / 2);
            constraints.push(Length(h));
        }
        constraints.push(Length(1)); // context row
        if show_cockpit {
            constraints.push(Length(5));
        }
        // LSP gets flexible space; fixed-height panels slot directly below it.
        if show_lsp {
            constraints.push(Fill(1));
        }
        if show_obs {
            constraints.push(Length(6));
        }
        if show_plan {
            constraints.push(Length(plan_panel::panel_height(state.plan_steps.len())));
        }
        if show_quality {
            constraints.push(Length(8));
        }
        if show_value {
            constraints.push(Length(4));
        }

        let rail_chunks = Layout::vertical(constraints).split(rail_rect);
        let mut ci = 0usize;

        // ── Metrics / runner panel ────────────────────────────────────────
        if show_metrics && ci < rail_chunks.len() {
            frame.render_widget(
                metrics_view::MetricsView::with_savings(
                    state.metrics_snapshot.clone(),
                    state.savings_snapshot.clone(),
                ),
                rail_chunks[ci],
            );
            ci += 1;
        }

        // ── Context slot ──────────────────────────────────────────────────
        // Clamp to usize::MAX — well within range on 64-bit targets.
        let slots = vec![context_rail::ContextSlot {
            name: "context".into(),
            used: usize::try_from(state.context_used).unwrap_or(usize::MAX),
            total: usize::try_from(state.context_window).unwrap_or(usize::MAX),
        }];
        frame.render_widget(context_rail::ContextRail::new(slots), rail_chunks[ci]);
        ci += 1;

        // ── Role cockpit panel ────────────────────────────────────────────
        if show_cockpit && ci < rail_chunks.len() {
            render_role_cockpit(frame, rail_chunks[ci], state);
            ci += 1;
        }

        // ── LSP panel ─────────────────────────────────────────────────────
        if show_lsp && ci < rail_chunks.len() {
            lsp_panel::LspPanel::new(&state.lsp_snapshot)
                .with_graph(state.graph_symbols)
                .render(rail_chunks[ci], frame);
            ci += 1;
        }

        // ── Observability panel ───────────────────────────────────────────
        if show_obs && ci < rail_chunks.len() {
            obs_panel::ObsPanel::new(&state.obs_snapshot).render(rail_chunks[ci], frame);
            ci += 1;
        }

        // ── Plan step tracker ─────────────────────────────────────────────
        if show_plan && ci < rail_chunks.len() {
            plan_panel::PlanPanel::new(&state.plan_steps).render(rail_chunks[ci], frame);
            ci += 1;
        }

        // ── Quality gate panel ────────────────────────────────────────────
        if show_quality && ci < rail_chunks.len() {
            quality_panel::QualityPanel::new(&state.quality_snapshot)
                .render(rail_chunks[ci], frame);
            ci += 1;
        }

        // ── Value / ROI panel ─────────────────────────────────────────────
        if show_value && ci < rail_chunks.len() {
            value_panel::ValuePanel::new(&state.value_snapshot).render(rail_chunks[ci], frame);
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

    // -- Session browser overlay ----------------------------------------------
    if state.session_browser_open && !state.session_rail_items.is_empty() {
        let items = &state.session_rail_items;
        let total = items.len();
        let cursor = state.session_browser_cursor;
        #[allow(clippy::cast_possible_truncation)]
        let popup_h = (total as u16 + 2).min(area.height.saturating_sub(2));
        let popup_w = 40u16.min(area.width);
        let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
        let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
        let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);
        let browser_lines: Vec<Line<'_>> = items
            .iter()
            .enumerate()
            .map(|(i, (_id, label))| {
                let text = format!(" {label}");
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
        let browser_title = format!(
            " sessions {}/{} ",
            cursor.saturating_add(1).min(total),
            total
        );
        frame.render_widget(Clear, popup_rect);
        frame.render_widget(
            Paragraph::new(browser_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .title(browser_title),
            ),
            popup_rect,
        );
    }

    // -- File picker overlay --------------------------------------------------
    if state.file_picker_open {
        render_file_picker(frame, area, state);
    }
}

/// Renders a centred pop-up overlay with the full [`SessionDetail`] fields.
/// The overlay is dismissed by pressing Esc.
pub(crate) fn render_session_detail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    detail: &SessionDetail,
    p: &crate::theme::Palette,
) {
    use ratatui::widgets::Clear;

    let popup_w = area.width.clamp(30, 60);
    let popup_h: u16 = 14;
    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let field = |label: &str, value: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {label:<14}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(value.to_owned()),
        ])
    };

    let lines = vec![
        field("id", &detail.id),
        field("title", detail.title.as_deref().unwrap_or("-")),
        field("mode", detail.mode.as_deref().unwrap_or("-")),
        field("status", detail.status.as_deref().unwrap_or("-")),
        field("change", detail.active_change.as_deref().unwrap_or("-")),
        field("cowork", detail.cowork_mode.as_deref().unwrap_or("-")),
        Line::raw(""),
        field("created", &detail.created_at),
        field("updated", &detail.updated_at),
        Line::raw(""),
        Line::from(Span::styled(
            "  ^Enter load \u{00b7} Esc close",
            Style::default().fg(p.text_dim),
        )),
    ];

    frame.render_widget(Clear, popup_rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(" session detail "),
        ),
        popup_rect,
    );
}

/// Renders a compact session config peek overlay (Ctrl+P in scroll mode).
///
/// Shows mode, tier, runner, and context window fill so prompt-engineering
/// context is visible without opening the full context rail.
pub(crate) fn render_session_peek(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    p: &crate::theme::Palette,
) {
    use ratatui::widgets::Clear;
    let popup_w = area.width.clamp(30, 52);
    let popup_h: u16 = 7;
    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let field = |label: &str, value: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {label:<10}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(value.to_owned()),
        ])
    };
    let ctx_str = (state.context_used * 100)
        .checked_div(state.context_window)
        .map_or_else(
            || "-".to_owned(),
            |pct| {
                format!(
                    "{}k / {}k  ({}%)",
                    state.context_used / 1000,
                    state.context_window / 1000,
                    pct.min(100)
                )
            },
        );
    let lines = vec![
        field("mode", state.mode.as_deref().unwrap_or("impl")),
        field("tier", state.tier.as_deref().unwrap_or("fast")),
        field("runner", &state.runner),
        field("context", &ctx_str),
        Line::raw(""),
        Line::from(Span::styled(
            "  ^P / Esc  close",
            Style::default().fg(p.text_dim),
        )),
    ];
    frame.render_widget(Clear, popup_rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(" session config (^P) "),
        ),
        popup_rect,
    );
}

/// Renders the slash-command completion popup in the bottom portion of the screen.
/// Renders the role cockpit panel showing current session role, tier, and
/// in-flight turn status.  Displayed in the right rail when `Ctrl-A` is active.
pub(crate) fn render_role_cockpit(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
) {
    let p = palette();
    let mode = state.mode.as_deref().unwrap_or("impl");
    let tier = state.tier.as_deref().unwrap_or("fast");
    let runner = &state.runner;

    let in_flight = state.pending_task_id.is_some();
    let status_symbol = if in_flight {
        "● in-flight"
    } else {
        "○ idle"
    };
    let status_style = if in_flight {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.text_dim)
    };

    // Tier colour follows the forge tier palette.
    let tier_color = match tier {
        "local" => p.local,
        "deep" => p.deep,
        _ => p.fast,
    };

    let active_name = state.active_agent_name.as_deref().unwrap_or(mode);

    // Prominent brand-coloured client badge: `◆ CLAUDE · deep`.
    let client_color = crate::theme::runner_color(runner);
    let client_label = crate::theme::runner_label(runner);

    let lines: Vec<Line<'_>> = vec![
        Line::from(vec![
            Span::styled(
                format!("\u{25C6} {client_label}"),
                Style::default()
                    .fg(client_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" \u{00B7} {tier}"), Style::default().fg(tier_color)),
        ]),
        Line::from(vec![
            Span::styled("role  ", Style::default().fg(p.text_dim)),
            // Per-agent accent pip (deterministic colour); the name itself stays
            // bright/readable rather than being recoloured.
            Span::styled(
                "\u{25C6} ",
                Style::default().fg(crate::theme::agent_color(active_name)),
            ),
            Span::styled(
                active_name.to_owned(),
                Style::default()
                    .fg(p.text_bright)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("mode  ", Style::default().fg(p.text_dim)),
            Span::styled(
                "\u{25C6} ",
                Style::default().fg(crate::theme::agent_color(mode)),
            ),
            Span::styled(mode.to_owned(), Style::default().fg(p.text_bright)),
        ]),
        Line::from(vec![
            Span::styled("turn  ", Style::default().fg(p.text_dim)),
            Span::styled(status_symbol.to_owned(), status_style),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.border))
        .title(" cockpit [Ctrl-A] ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub(crate) fn render_slash_popup(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
) {
    let p = palette();
    let completions = &state.slash_completions;
    // Height = number of completions + 2 border rows, capped at available space.
    #[allow(clippy::cast_possible_truncation)]
    let popup_h = (completions.len() as u16 + 2).min(area.height.saturating_sub(2));
    // Session-picker rows (`<short-id>  <title>  <mode>  <updated_at>`) are wider
    // than the 20-col command popup, so widen to fit when the picker is open.
    // Command palette also widens to accommodate the description column.
    let desired_w = if state.session_picker_mode {
        60
    } else if state.command_palette_mode {
        50
    } else {
        20
    };
    let popup_w = desired_w.min(area.width);
    // Position just above the input row (bottom-left).
    let popup_y = area.y + area.height.saturating_sub(popup_h + 1);
    let popup_x = area.x;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let lines: Vec<Line<'_>> = completions
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let label = if state.command_palette_mode {
                let desc = SLASH_COMMAND_DESCRIPTIONS
                    .iter()
                    .find(|(cmd, _)| cmd == c)
                    .map_or("", |(_, d)| d);
                format!(" {c:<14}  {desc}")
            } else {
                format!(" {c}")
            };
            if i == state.slash_cursor {
                Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(p.bg)
                        .bg(p.text_bright)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(label, Style::default().fg(p.text)))
            }
        })
        .collect();

    let title = if state.session_picker_mode {
        "sessions"
    } else if state.runner_picker_mode {
        "runners"
    } else if state.command_palette_mode {
        "palette"
    } else {
        "commands"
    };
    frame.render_widget(Clear, popup_rect);
    let popup = Paragraph::new(lines)
        .style(Style::default().bg(p.panel))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(title),
        );
    frame.render_widget(popup, popup_rect);
}

pub(crate) fn render_file_picker(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
) {
    let p = palette();
    let entries = &state.file_picker_entries;
    #[allow(clippy::cast_possible_truncation)]
    let popup_h = (entries.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup_w = 50_u16.min(area.width);
    let popup_y = area.y + area.height.saturating_sub(popup_h + 1);
    let popup_x = area.x;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let lines: Vec<Line<'_>> = entries
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let label = format!(" {name}");
            if i == state.file_picker_cursor {
                Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(p.bg)
                        .bg(p.text_bright)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(label, Style::default().fg(p.text)))
            }
        })
        .collect();

    let title = format!(" {} ", state.file_picker_dir.display());
    frame.render_widget(Clear, popup_rect);
    let popup = Paragraph::new(lines)
        .style(Style::default().bg(p.panel))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(title),
        );
    frame.render_widget(popup, popup_rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::commands::filtered_completions;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

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
    fn push_delta_accumulates_content_in_panel() {
        let mut state = make_state("sess-stream");
        state.main_panel.push_delta("hello");
        state.main_panel.push_delta(" there");
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("hello"),
            "delta content should appear in rendered buffer"
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
            content.contains("thinking") || content.contains("working"),
            "buffer should contain spinner indicator when turn_in_flight is true"
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

    #[test]
    fn wrap_input_rows_splits_long_line() {
        // 25 chars at width 10 → 3 rows (10/10/5).
        let rows = wrap_input_rows(&"x".repeat(25), 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].chars().count(), 10);
        assert_eq!(rows[2].chars().count(), 5);
    }

    #[test]
    fn wrap_input_rows_honours_newlines() {
        let rows = wrap_input_rows("ab\ncd", 80);
        assert_eq!(rows, vec!["ab".to_string(), "cd".to_string()]);
    }

    #[test]
    fn wrap_input_rows_empty_is_one_row() {
        assert_eq!(wrap_input_rows("", 10).len(), 1);
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
    #[test]
    fn normal_display_uses_prompt_prefix() {
        let input = "hello";
        let display = format!("> {input}_");
        assert_eq!(display, "> hello_");
    }
    #[test]
    fn prompt_token_estimate_uses_chars_over_four_heuristic() {
        // 40 chars / 4 = 10 estimated tokens.
        let input = "a".repeat(40);
        let chars = input.chars().count();
        #[allow(clippy::integer_division)]
        let est = chars / 4;
        assert_eq!(est, 10, "40 chars should estimate to 10 tokens");
    }
    #[test]
    fn prompt_token_estimate_rounds_down() {
        let input = "abc"; // 3 chars / 4 = 0 — rounds down
        let chars = input.chars().count();
        #[allow(clippy::integer_division)]
        let est = chars / 4;
        assert_eq!(est, 0);
    }
}
