//! Session overlay panels: the full-detail pop-up and the compact peek.

use super::*;

/// Renders a centred pop-up overlay with the full [`SessionDetail`] fields.
/// The overlay is dismissed by pressing Esc.
pub(super) fn render_session_detail(
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
pub(super) fn render_session_peek(
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
