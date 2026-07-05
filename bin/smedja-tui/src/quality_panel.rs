//! Quality panel widget — Tier-1 deterministic gate scores in the right rail.
//!
//! Renders below the obs panel when `Ctrl-Q` is active. All data comes from
//! [`QualitySnapshot`]; the widget never fetches or blocks.

use crate::theme::palette;
use crate::viz::{pill, PillKind};
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
    /// Composite scores from recent turns (oldest → newest) for the trend
    /// sparkline. The current `score` is the last element.
    pub trend: Vec<u8>,
    /// Whether any turn has been scored yet. `false` renders an explicit
    /// "awaiting first turn" empty state rather than a broken-looking `0`.
    pub scored: bool,
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

    /// Overall verdict pill kind: green ≥ 90, amber ≥ 70, red below — the
    /// no-color-safe complement to the coloured letter-grade badge.
    fn verdict_pill(&self) -> PillKind {
        if self.score >= 90 {
            PillKind::Pass
        } else if self.score >= 70 {
            PillKind::Warn
        } else {
            PillKind::Fail
        }
    }

    /// Letter grade A–F over the 0–100 composite (codeburn-style badge).
    fn grade_letter(&self) -> &'static str {
        match self.score {
            90..=u8::MAX => "A",
            80..=89 => "B",
            70..=79 => "C",
            60..=69 => "D",
            _ => "F",
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

        let title = if snap.llm_reviewed {
            " quality [llm] "
        } else {
            " quality "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.border))
            .title(title);

        // ── Empty state: no turn scored yet ──────────────────────────────────
        // A bare `0` reads as "broken"; say plainly that scoring runs after the
        // first turn so an idle panel is self-explanatory.
        if !snap.scored {
            lines.push(Line::from(Span::styled(
                "awaiting first turn",
                Style::default().fg(p.text_dim),
            )));
            lines.push(Line::from(Span::styled(
                "gates score after a turn",
                Style::default().fg(p.text_dim),
            )));
            frame.render_widget(Paragraph::new(lines).block(block), area);
            return;
        }

        // ── Grade badge + score + gauge ──────────────────────────────────────
        // `[ A ] 92  ██████░░` — codeburn-style letter badge, the numeric score,
        // and a meter filled to score/100 in the zone colour (green→amber→red).
        let color = snap.score_color();
        let badge = Span::styled(
            format!(" {} ", snap.grade_letter()),
            Style::default()
                .bg(color)
                .fg(crate::theme::contrast_fg(color))
                .add_modifier(Modifier::BOLD),
        );
        // Reserve: badge (3) + space + "NNN" (3) + space.
        let gauge_w = inner_w.saturating_sub(8).clamp(3, 12);
        let gauge = crate::viz::microbar(f64::from(snap.score), 100.0, gauge_w);
        lines.push(Line::from(vec![
            badge,
            Span::styled(
                format!(" {:>3} ", snap.score),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(gauge, Style::default().fg(color)),
        ]));

        // ── Verdict pill + trend sparkline (quality over recent turns) ───────
        // The pill restates the verdict as text (`✔ PASS`) so the panel still
        // reads under `no_color`; the sparkline shows the score's recent history.
        let mut row: Vec<Span<'_>> = vec![pill(snap.verdict_pill(), false), Span::raw(" ")];
        if snap.trend.len() >= 2 {
            let keep = inner_w.saturating_sub(8).max(1);
            let recent: Vec<u8> = snap.trend.iter().rev().take(keep).rev().copied().collect();
            row.push(Span::styled(
                crate::viz::sparkline(&recent, 100),
                Style::default().fg(p.accent),
            ));
        }
        lines.push(Line::from(row));

        // ── Gate breakdown as filled/empty pips ──────────────────────────────
        lines.push(Line::from(vec![
            gate_pip("tdd", snap.tdd_pass),
            Span::raw(" "),
            gate_pip("clean", snap.clean_pass),
        ]));
        lines.push(Line::from(vec![
            gate_pip("size", snap.file_size_pass()),
            Span::raw(" "),
            gate_pip("skill", snap.skill_inject_pass()),
        ]));

        // ── First failing advisory, then any suggested command ───────────────
        let advisory = snap
            .file_advisories
            .first()
            .or_else(|| snap.skill_advisories.first());
        if let Some(text) = advisory {
            let full = format!("! {text}");
            let truncated: String = full.chars().take(inner_w).collect();
            lines.push(Line::from(Span::styled(
                truncated,
                Style::default().fg(p.warn),
            )));
        }
        if let Some(ref cmd) = snap.suggested_command {
            let hint: String = cmd.chars().take(inner_w).collect();
            lines.push(Line::from(Span::styled(
                hint,
                Style::default().fg(p.text_dim),
            )));
        }

        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

/// Renders one gate as a labelled pip: `● tdd` (filled/success when passing) or
/// `○ size` (hollow/dim when failing), so all four gates read at a glance.
fn gate_pip<'a>(label: &str, pass: bool) -> Span<'a> {
    let p = palette();
    if pass {
        Span::styled(format!("● {label}"), Style::default().fg(p.success))
    } else {
        Span::styled(format!("○ {label}"), Style::default().fg(p.error))
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
            scored: true,
            tdd_pass: true,
            clean_pass: true,
            file_advisories: vec!["main.rs 7880 L (threshold 600)".into()],
            ..QualitySnapshot::default()
        };
        let rendered = render_snapshot(&snap, 30, 10);
        assert!(rendered.contains("quality"), "title present: {rendered}");
    }

    #[test]
    fn unscored_panel_shows_awaiting_state() {
        let snap = QualitySnapshot::default();
        let rendered = render_snapshot(&snap, 30, 10);
        assert!(
            rendered.contains("awaiting"),
            "empty state is explicit, not a bare 0: {rendered}"
        );
    }

    #[test]
    fn scored_panel_shows_grade_badge() {
        let snap = QualitySnapshot {
            score: 100,
            scored: true,
            tdd_pass: true,
            clean_pass: true,
            trend: vec![50, 75, 100],
            ..QualitySnapshot::default()
        };
        let rendered = render_snapshot(&snap, 30, 10);
        assert!(rendered.contains('A'), "A-grade badge present: {rendered}");
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
            scored: true,
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
