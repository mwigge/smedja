//! Inline cowork gate approval widget.
//!
//! Rendered as a centred overlay when a tool call is awaiting human approval.
//! Keyboard shortcuts: `y` approve, `n` deny, `m` modify (enter instruction mode).

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

/// A single pending cowork approval, deserialized from `cowork.pending`.
#[derive(Debug, Clone)]
pub struct CoworkItem {
    /// Approval UUID returned by the daemon.
    pub id: String,
    /// Tool name (e.g. `"bash"`, `"edit_file"`).
    pub tool: String,
    /// Step index within the current turn.
    pub step_n: u32,
    /// Compact string representation of tool arguments.
    pub args_display: String,
    /// Agent's reasoning for invoking this tool.
    pub reasoning: String,
}

/// The cowork gate overlay widget.
///
/// Renders the first pending item. Remaining items are shown as a count in the
/// header so the user knows there are more queued behind this one.
pub struct CoworkWidget<'a> {
    pub items: &'a [CoworkItem],
    /// Whether the user has pressed `m` and is typing a modify instruction.
    pub modify_mode: bool,
    /// Current content of the modify instruction input.
    pub modify_input: &'a str,
}

impl Widget for CoworkWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(item) = self.items.first() else {
            return;
        };

        let queue_suffix = if self.items.len() > 1 {
            format!("  +{} queued", self.items.len() - 1)
        } else {
            String::new()
        };

        let title = format!(
            " COWORK  step {} · {}{} ",
            item.step_n, item.tool, queue_suffix
        );

        // Truncate args to fit widget width minus 2 padding chars.
        let inner_width = (area.width as usize).saturating_sub(4).max(1);
        let args_truncated = truncate_str(&item.args_display, inner_width);
        let reasoning_truncated = truncate_str(&item.reasoning, inner_width);

        let footer_line = if self.modify_mode {
            Line::from(vec![
                Span::raw(" instruction: "),
                Span::styled(
                    format!("{}_", self.modify_input),
                    Style::default().fg(Color::Cyan),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " [y] ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("approve  "),
                Span::styled(
                    "[n] ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw("deny  "),
                Span::styled(
                    "[m] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("modify"),
            ])
        };

        let lines = vec![
            Line::from(Span::styled(
                format!(" {args_truncated} "),
                Style::default().fg(Color::White),
            )),
            Line::from(Span::styled(
                format!(" {reasoning_truncated} "),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            footer_line,
        ];

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_alignment(Alignment::Left);

        // Clear the area first so the overlay is opaque.
        Clear.render(area, buf);
        Paragraph::new(Text::from(lines))
            .block(block)
            .render(area, buf);
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Computes the overlay rectangle — centred, 80% of `parent` width, max 7 rows.
#[must_use]
pub fn overlay_rect(parent: Rect) -> Rect {
    #[allow(
        clippy::cast_lossless,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let w = ((parent.width as f32) * 0.80) as u16;
    let w = w.clamp(40, parent.width);
    let h = 7u16.min(parent.height);
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn item(step_n: u32, tool: &str) -> CoworkItem {
        CoworkItem {
            id: "test-id".into(),
            tool: tool.into(),
            step_n,
            args_display: r#"{"cmd":"ls -la"}"#.into(),
            reasoning: "list files for inspection".into(),
        }
    }

    fn render_widget(items: &[CoworkItem], modify_mode: bool, modify_input: &str) -> String {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let widget = CoworkWidget {
                    items,
                    modify_mode,
                    modify_input,
                };
                widget.render(area, f.buffer_mut());
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut rows = Vec::new();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .map(|x| {
                    buf.cell((x, y))
                        .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            rows.push(row.trim_end().to_owned());
        }
        rows.join("\n")
    }

    #[test]
    fn widget_renders_tool_name_in_header() {
        let items = vec![item(1, "bash")];
        let output = render_widget(&items, false, "");
        assert!(
            output.contains("bash"),
            "widget must show tool name; got:\n{output}"
        );
    }

    #[test]
    fn widget_renders_approve_deny_modify_shortcuts() {
        let items = vec![item(2, "edit_file")];
        let output = render_widget(&items, false, "");
        assert!(output.contains("[y]"), "must show [y] approve");
        assert!(output.contains("[n]"), "must show [n] deny");
        assert!(output.contains("[m]"), "must show [m] modify");
    }

    #[test]
    fn modify_mode_shows_instruction_prompt() {
        let items = vec![item(1, "bash")];
        let output = render_widget(&items, true, "revert the file");
        assert!(
            output.contains("instruction:"),
            "modify mode must show instruction prompt; got:\n{output}"
        );
        assert!(
            output.contains("revert the file"),
            "must show current input"
        );
    }

    #[test]
    fn empty_items_renders_nothing() {
        let output = render_widget(&[], false, "");
        assert!(
            !output.contains("COWORK"),
            "empty items must not render COWORK header"
        );
    }

    #[test]
    fn queue_count_shown_when_multiple_items() {
        let items = vec![item(1, "bash"), item(2, "edit_file"), item(3, "read")];
        let output = render_widget(&items, false, "");
        assert!(
            output.contains("+2"),
            "must show +2 queued for 3 items; got:\n{output}"
        );
    }

    #[test]
    fn overlay_rect_is_centred_and_bounded() {
        let parent = Rect::new(0, 0, 100, 30);
        let r = overlay_rect(parent);
        assert!(r.width <= parent.width);
        assert!(r.height <= parent.height);
        // x is centred: left margin ≈ right margin
        let left = r.x;
        let right = parent.width - r.x - r.width;
        assert!(
            left.abs_diff(right) <= 1,
            "rect must be horizontally centred"
        );
    }
}
