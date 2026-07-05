//! Overlay pop-ups: the slash-command menu and the file picker.

use super::*;

pub(super) fn render_slash_popup(
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

pub(super) fn render_file_picker(
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
