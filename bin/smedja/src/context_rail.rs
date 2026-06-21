//! Context rail widget — sidebar showing working memory slot usage.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};

/// Fill threshold levels for slot colour coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStyle {
    Low,    // < 60%
    Medium, // 60–80%
    High,   // > 80%
}

impl SlotStyle {
    #[must_use]
    pub fn from_pct(pct: f64) -> Self {
        if pct > 80.0 {
            Self::High
        } else if pct >= 60.0 {
            Self::Medium
        } else {
            Self::Low
        }
    }

    #[must_use]
    pub fn color(self) -> Color {
        match self {
            Self::Low => Color::Green,
            Self::Medium => Color::Yellow,
            Self::High => Color::Red,
        }
    }
}

/// A single context slot entry.
#[derive(Debug, Clone)]
pub struct ContextSlot {
    pub name: String,
    pub used: usize,
    pub total: usize,
}

impl ContextSlot {
    #[must_use]
    pub fn pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            // SAFETY: context slot counts are UI values well within f64's
            // exact integer range (< 2^52); precision loss is acceptable for
            // a percentage display.
            #[allow(clippy::cast_precision_loss)]
            let result = (self.used as f64 / self.total as f64) * 100.0;
            result
        }
    }

    /// Renders a Unicode fill bar of `width` characters using block elements.
    ///
    /// Uses '█' for filled and '░' for empty.
    #[must_use]
    pub fn fill_bar(&self, width: usize) -> String {
        let pct = self.pct();
        // Truncation is intentional: we want whole character columns; values
        // are bounded by `width` so no negative or excessive values can occur.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let filled = ((pct / 100.0) * width as f64).round() as usize;
        let filled = filled.min(width);
        let empty = width - filled;
        format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
    }
}

/// The context rail widget.
pub struct ContextRail {
    pub slots: Vec<ContextSlot>,
    #[allow(dead_code)] // toggled by Ctrl-R keybinding; visibility checked by caller
    pub visible: bool,
}

impl ContextRail {
    pub const WIDTH: u16 = 25;

    #[must_use]
    pub fn new(slots: Vec<ContextSlot>) -> Self {
        Self {
            slots,
            visible: true,
        }
    }

    #[allow(dead_code)] // called via Ctrl-R keybinding via AppState.context_rail_visible toggle
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }
}

impl Widget for ContextRail {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let bar_width = (area.width as usize).saturating_sub(9).max(1);
        let lines: Vec<Line<'_>> = self
            .slots
            .iter()
            .map(|slot| {
                let pct = slot.pct();
                let color = SlotStyle::from_pct(pct).color();
                let bar = slot.fill_bar(bar_width);
                let name_trunc = &slot.name[..slot.name.len().min(6)];
                let label = format!(" {name_trunc:<6}{pct:>3.0}%");
                Line::from(vec![
                    Span::styled(bar, Style::default().fg(color)),
                    Span::raw(label),
                ])
            })
            .collect();
        Paragraph::new(Text::from(lines)).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_bar_full_at_100_pct() {
        let slot = ContextSlot {
            name: "test".into(),
            used: 100,
            total: 100,
        };
        let bar = slot.fill_bar(10);
        assert_eq!(bar, "\u{2588}".repeat(10));
        assert_eq!(bar.chars().filter(|&c| c == '\u{2588}').count(), 10);
    }

    #[test]
    fn fill_bar_empty_at_0_pct() {
        let slot = ContextSlot {
            name: "test".into(),
            used: 0,
            total: 100,
        };
        let bar = slot.fill_bar(10);
        assert_eq!(bar, "\u{2591}".repeat(10));
    }

    #[test]
    fn fill_bar_half_at_50_pct() {
        let slot = ContextSlot {
            name: "test".into(),
            used: 50,
            total: 100,
        };
        let bar = slot.fill_bar(10);
        assert_eq!(bar.chars().filter(|&c| c == '\u{2588}').count(), 5);
    }

    #[test]
    fn slot_style_low_below_60() {
        assert_eq!(SlotStyle::from_pct(59.9), SlotStyle::Low);
        assert_eq!(SlotStyle::from_pct(0.0), SlotStyle::Low);
    }

    #[test]
    fn slot_style_medium_at_60() {
        assert_eq!(SlotStyle::from_pct(60.0), SlotStyle::Medium);
        assert_eq!(SlotStyle::from_pct(79.9), SlotStyle::Medium);
    }

    #[test]
    fn slot_style_high_above_80() {
        assert_eq!(SlotStyle::from_pct(80.1), SlotStyle::High);
        assert_eq!(SlotStyle::from_pct(100.0), SlotStyle::High);
    }

    #[test]
    fn color_thresholds_applied() {
        assert_eq!(SlotStyle::from_pct(50.0).color(), Color::Green);
        assert_eq!(SlotStyle::from_pct(70.0).color(), Color::Yellow);
        assert_eq!(SlotStyle::from_pct(90.0).color(), Color::Red);
    }
}
