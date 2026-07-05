//! Change-detection alerts (dolphie-style) — state transitions rendered as
//! highlighted transcript divider lines so a model swap, tier change, or a
//! context compaction is a visible event rather than a silent one.
//!
//! `── model changed: sonnet → opus ──`
//! `── tier: fast → deep ──`
//! `── context compacted 48K→9K ──`

use crate::theme::palette;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Formats a token count compactly (`48K`, `9K`, `1.5K`, `512`). Whole
/// thousands drop the decimal.
#[must_use]
pub fn fmt_k(n: u64) -> String {
    if n >= 1_000 {
        #[allow(clippy::cast_precision_loss)]
        let v = n as f64 / 1000.0;
        if (v.fract()).abs() < 0.05 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                format!("{}K", v.round() as u64)
            }
        } else {
            format!("{v:.1}K")
        }
    } else {
        n.to_string()
    }
}

/// Builds a centered divider line `── <label> ──` filled to `width`, in `color`.
#[must_use]
pub fn divider_line(label: &str, width: usize, color: Color, no_color: bool) -> Line<'static> {
    let core = format!(" {label} ");
    let core_w = core.chars().count();
    let total_dashes = width.saturating_sub(core_w);
    let left = total_dashes / 2;
    let right = total_dashes.saturating_sub(left);
    let style = if no_color {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    Line::from(vec![Span::styled(
        format!("{}{core}{}", "─".repeat(left), "─".repeat(right)),
        style,
    )])
}

/// `── model changed: <from> → <to> ──` (molten — a primary transition).
#[must_use]
pub fn model_change_line(from: &str, to: &str, width: usize, no_color: bool) -> Line<'static> {
    divider_line(
        &format!("model changed: {from} → {to}"),
        width,
        palette().molten,
        no_color,
    )
}

/// `── tier: <from> → <to> ──` (accent).
#[must_use]
pub fn tier_change_line(from: &str, to: &str, width: usize, no_color: bool) -> Line<'static> {
    divider_line(
        &format!("tier: {from} → {to}"),
        width,
        palette().accent,
        no_color,
    )
}

/// `── context compacted 48K→9K ──` (warn — surfaces a normally-silent event).
#[must_use]
pub fn compaction_line(before: u64, after: u64, width: usize, no_color: bool) -> Line<'static> {
    divider_line(
        &format!("context compacted {}→{}", fmt_k(before), fmt_k(after)),
        width,
        palette().warn,
        no_color,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn fmt_k_scales() {
        assert_eq!(fmt_k(512), "512");
        assert_eq!(fmt_k(1500), "1.5K");
        assert_eq!(fmt_k(48_000), "48K");
    }

    #[test]
    fn divider_fills_to_width() {
        let l = divider_line("hi", 20, palette().accent, true);
        assert_eq!(text(&l).chars().count(), 20);
    }

    #[test]
    fn divider_centers_label() {
        let t = text(&divider_line("x", 11, palette().accent, true));
        assert!(t.contains("─ x ─"), "centered: {t}");
    }

    #[test]
    fn model_change_reads() {
        let t = text(&model_change_line("sonnet", "opus", 40, true));
        assert!(t.contains("model changed: sonnet → opus"), "{t}");
    }

    #[test]
    fn tier_change_reads() {
        let t = text(&tier_change_line("fast", "deep", 40, true));
        assert!(t.contains("tier: fast → deep"), "{t}");
    }

    #[test]
    fn compaction_reads() {
        let t = text(&compaction_line(48_000, 9_000, 40, true));
        assert!(t.contains("context compacted 48K→9K"), "{t}");
    }

    #[test]
    fn narrow_width_never_panics() {
        for w in 0..8 {
            let _ = model_change_line("a", "b", w, true);
        }
    }
}
