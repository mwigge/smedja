//! Role cockpit panel: current session role, tier, and in-flight turn status.

use super::*;

/// Renders the slash-command completion popup in the bottom portion of the screen.
/// Renders the role cockpit panel showing current session role, tier, and
/// in-flight turn status.  Displayed in the right rail when `Ctrl-A` is active.
pub(super) fn render_role_cockpit(
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
        Line::from(vec![
            Span::styled("gate  ", Style::default().fg(p.text_dim)),
            {
                // Awaiting a human decision at the cowork gate takes priority; then
                // in-flight (running); otherwise idle/skip.
                let kind = if !state.pending_cowork.is_empty() {
                    viz::PillKind::Await
                } else if in_flight {
                    viz::PillKind::Running
                } else {
                    viz::PillKind::Skip
                };
                viz::pill(kind, state.no_color)
            },
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.border))
        .title(" cockpit [Ctrl-A] ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
