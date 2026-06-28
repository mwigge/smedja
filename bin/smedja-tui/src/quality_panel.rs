//! Quality panel widget — Tier-1 deterministic gate scores in the right rail.
//!
//! Renders below the obs panel when `Ctrl-Q` is active. All data comes from
//! [`QualitySnapshot`]; the widget never fetches or blocks.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Immutable snapshot of quality gate results the panel needs to render.
#[derive(Debug, Clone, Default)]
pub struct QualitySnapshot {
    /// Composite 0–100 quality score.
    pub score: u8,
    /// Whether the TDD backstop gate passed.
    pub tdd_pass: bool,
    /// Whether the clean-code gate passed.
    pub clean_pass: bool,
    /// Whether this snapshot was produced by a Tier-2 LLM review.
    pub llm_reviewed: bool,
    /// Human-readable file-size advisories (one per flagged file).
    pub file_advisories: Vec<String>,
    /// Human-readable skill-inject advisories (one per missing skill).
    pub skill_advisories: Vec<String>,
    /// Slash command suggested by the LLM reviewer, if any.
    pub suggested_command: Option<String>,
}

impl QualitySnapshot {
    fn score_color(&self) -> ratatui::style::Color {
        let p = palette();
        if self.score >= 90 {
            p.success
        } else if self.score >= 70 {
            p.warn
        } else {
            p.error
        }
    }

    fn file_size_pass(&self) -> bool {
        self.file_advisories.is_empty()
    }

    fn skill_inject_pass(&self) -> bool {
        self.skill_advisories.is_empty()
    }
}

/// The quality rail panel.
pub struct QualityPanel<'a> {
    pub snapshot: &'a QualitySnapshot,
}

impl<'a> QualityPanel<'a> {
    #[must_use]
    pub fn new(snapshot: &'a QualitySnapshot) -> Self {
        Self { snapshot }
    }

    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if area.height < 3 {
            return;
        }

        let p = palette();
        let snap = self.snapshot;
        let inner_w = (area.width as usize).saturating_sub(2).max(1);
        let mut lines: Vec<Line<'_>> = Vec::new();

        // ── Score line ───────────────────────────────────────────────────────
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}", snap.score),
                Style::default()
                    .fg(snap.score_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" / 100", Style::default().fg(p.text_dim)),
        ]));

        // ── Gate lines ───────────────────────────────────────────────────────
        lines.push(gate_line("tdd", snap.tdd_pass, None, inner_w));
        lines.push(gate_line("clean", snap.clean_pass, None, inner_w));
        lines.push(gate_line(
            "size",
            snap.file_size_pass(),
            snap.file_advisories.first().map(String::as_str),
            inner_w,
        ));
        lines.push(gate_line(
            "skill",
            snap.skill_inject_pass(),
            snap.skill_advisories.first().map(String::as_str),
            inner_w,
        ));

        // ── Suggested command hint (dim, LLM-reviewed only) ──────────────────
        if let Some(ref cmd) = snap.suggested_command {
            let hint: String = cmd.chars().take(inner_w).collect();
            lines.push(Line::from(vec![Span::styled(
                hint,
                Style::default().fg(p.text_dim),
            )]));
        }

        let title = if snap.llm_reviewed {
            " quality [llm] "
        } else {
            " quality "
        };

        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .title(title),
            ),
            area,
        );
    }
}

/// Renders one gate status line: `✓ label` or `! label: advisory`.
fn gate_line<'a>(label: &str, pass: bool, advisory: Option<&str>, max_w: usize) -> Line<'a> {
    let p = palette();
    if pass {
        Line::from(vec![
            Span::styled("✓ ", Style::default().fg(p.success)),
            Span::styled(label.to_owned(), Style::default().fg(p.text_dim)),
        ])
    } else {
        let detail = advisory.unwrap_or("advisory");
        let full = format!("! {label}: {detail}");
        // Truncate to panel width to avoid overflow.
        let truncated: String = full.chars().take(max_w).collect();
        Line::from(vec![Span::styled(truncated, Style::default().fg(p.warn))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_snapshot(snap: &QualitySnapshot, w: u16, h: u16) -> String {
        let panel = QualityPanel::new(snap);
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| panel.render(f.area(), f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn panel_renders_without_panic() {
        let snap = QualitySnapshot {
            score: 75,
            tdd_pass: true,
            clean_pass: true,
            file_advisories: vec!["main.rs 7880 L (threshold 600)".into()],
            ..QualitySnapshot::default()
        };
        let rendered = render_snapshot(&snap, 30, 10);
        assert!(rendered.contains("quality"), "title present: {rendered}");
    }

    #[test]
    fn panel_hides_when_height_below_3() {
        let snap = QualitySnapshot::default();
        let panel = QualityPanel::new(&snap);
        let backend = TestBackend::new(30, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        // Should not panic and should render nothing visible.
        terminal.draw(|f| panel.render(f.area(), f)).unwrap();
    }

    #[test]
    fn score_color_green_at_90_plus() {
        let snap = QualitySnapshot {
            score: 90,
            ..QualitySnapshot::default()
        };
        assert_eq!(snap.score_color(), palette().success);

        let snap_100 = QualitySnapshot {
            score: 100,
            ..QualitySnapshot::default()
        };
        assert_eq!(snap_100.score_color(), palette().success);
    }

    #[test]
    fn score_color_warn_at_70_to_89() {
        for score in [70u8, 75, 89] {
            let snap = QualitySnapshot {
                score,
                ..QualitySnapshot::default()
            };
            assert_eq!(
                snap.score_color(),
                palette().warn,
                "score {score} must be warn"
            );
        }
    }

    #[test]
    fn score_color_error_below_70() {
        for score in [0u8, 50, 69] {
            let snap = QualitySnapshot {
                score,
                ..QualitySnapshot::default()
            };
            assert_eq!(
                snap.score_color(),
                palette().error,
                "score {score} must be error"
            );
        }
    }

    #[test]
    fn panel_renders_score_value() {
        let snap = QualitySnapshot {
            score: 75,
            tdd_pass: true,
            clean_pass: true,
            ..QualitySnapshot::default()
        };
        let rendered = render_snapshot(&snap, 30, 10);
        assert!(rendered.contains("75"), "score value in output: {rendered}");
    }

    #[test]
    fn panel_renders_at_narrow_width_without_panic() {
        let snap = QualitySnapshot {
            score: 50,
            file_advisories: vec!["main.rs 7880 L (threshold 600)".into()],
            skill_advisories: vec!["/security-review — diff touches auth headers".into()],
            ..QualitySnapshot::default()
        };
        let rendered = render_snapshot(&snap, 30, 10);
        assert!(
            rendered.contains("quality"),
            "title at narrow width: {rendered}"
        );
    }

    #[test]
    fn panel_renders_at_wide_width_without_panic() {
        let snap = QualitySnapshot {
            score: 100,
            tdd_pass: true,
            clean_pass: true,
            ..QualitySnapshot::default()
        };
        let rendered = render_snapshot(&snap, 200, 10);
        assert!(
            rendered.contains("quality"),
            "title at wide width: {rendered}"
        );
    }
}
