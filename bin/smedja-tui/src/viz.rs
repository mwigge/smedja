//! Reusable visualization primitives — the shared vocabulary every panel draws
//! from so the whole TUI reads as one system.
//!
//! Three foundations live here:
//!
//! - [`microbar`] / [`magnitude_glyph`] — the single block-ramp bar helper reused
//!   by gauges, roster rows, and the trace waterfall, plus [`zone_color`] for the
//!   green/amber/red budget zones.
//! - [`pill`] — inverted-background status badges (`✔ PASS`, `✘ FAIL`, …) reused
//!   across cards and panels.
//! - [`RenderMode`] — a btop-style tiered glyph fallback (Braille → Block →
//!   ASCII) with a single [`detect_render_mode`] detection point so graphs
//!   degrade gracefully on a plain console.

use crate::theme::{contrast_fg, palette};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

// ---------------------------------------------------------------------------
// Tiered glyph fallback (btop-style)
// ---------------------------------------------------------------------------

/// Glyph richness the terminal can render. Detected once at startup via
/// [`detect_render_mode`]; smedja's own terminal supports Braille/Block, so that
/// is the default. A plain console degrades to ASCII.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Braille dot matrix — the richest graphs (default).
    Braille,
    /// Unicode block elements — bars and gauges without Braille.
    Block,
    /// Pure 7-bit ASCII — the universal fallback.
    Ascii,
}

impl RenderMode {
    /// Whether this mode can render Unicode block/ramp glyphs (Block or Braille).
    #[must_use]
    pub fn has_blocks(self) -> bool {
        matches!(self, Self::Braille | Self::Block)
    }
}

/// The single detection point for glyph richness.
///
/// Honours an explicit `SMEDJA_GLYPHS=ascii|block|braille` override, otherwise
/// falls back to ASCII only when the terminal advertises a dumb/limited
/// `TERM`; everything else defaults to the richest mode (Braille), which
/// smedja's own terminal supports.
#[must_use]
pub fn detect_render_mode() -> RenderMode {
    if let Ok(forced) = std::env::var("SMEDJA_GLYPHS") {
        match forced.trim().to_ascii_lowercase().as_str() {
            "ascii" => return RenderMode::Ascii,
            "block" => return RenderMode::Block,
            "braille" => return RenderMode::Braille,
            _ => {}
        }
    }
    match std::env::var("TERM").as_deref() {
        Ok("dumb" | "") | Err(_) => RenderMode::Ascii,
        Ok(_) => RenderMode::Braille,
    }
}

// ---------------------------------------------------------------------------
// Block-ramp bars
// ---------------------------------------------------------------------------

/// Fractional horizontal eighth-blocks, 1/8 … 7/8 (a full cell is `█`).
const H_EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// Renders `value/max` as a `width`-cell horizontal bar using the eighth-block
/// ramp for sub-cell precision, right-padded with spaces to exactly `width`
/// columns. The single bar helper reused everywhere.
///
/// A zero `max` or `width` yields an all-space string of the requested width.
#[must_use]
pub fn microbar(value: f64, max: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if max <= 0.0 || value <= 0.0 {
        return " ".repeat(width);
    }
    let frac = (value / max).clamp(0.0, 1.0);
    // Total eighths of a cell to fill across the whole bar.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let total_eighths = (frac * width as f64 * 8.0).round() as usize;
    let full = total_eighths / 8;
    let rem = total_eighths % 8;
    let mut s = String::with_capacity(width * 3);
    for _ in 0..full.min(width) {
        s.push('█');
    }
    let mut cells = full.min(width);
    if rem > 0 && cells < width {
        s.push(H_EIGHTHS[rem - 1]);
        cells += 1;
    }
    for _ in cells..width {
        s.push(' ');
    }
    s
}

/// ASCII-mode counterpart to [`microbar`] — `#` for filled cells, `-` for empty,
/// so bars still read on a 7-bit console.
#[must_use]
pub fn microbar_ascii(value: f64, max: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let frac = if max <= 0.0 {
        0.0
    } else {
        (value / max).clamp(0.0, 1.0)
    };
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let filled = (frac * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('#');
    }
    for _ in filled..width {
        s.push('-');
    }
    s
}

/// Mode-aware bar: [`microbar`] when the terminal has block glyphs, otherwise
/// [`microbar_ascii`].
#[must_use]
pub fn microbar_mode(value: f64, max: f64, width: usize, mode: RenderMode) -> String {
    if mode.has_blocks() {
        microbar(value, max, width)
    } else {
        microbar_ascii(value, max, width)
    }
}

// ---------------------------------------------------------------------------
// Zone colours
// ---------------------------------------------------------------------------

/// Green below 60 %, amber 60–84 %, red at/above 85 % — the shared budget-zone
/// thresholds used by every gauge (mirrors the obs panel's `budget_color`).
#[must_use]
pub fn zone_color(pct: u8) -> Color {
    let p = palette();
    if pct >= 85 {
        p.error
    } else if pct >= 60 {
        p.warn
    } else {
        p.success
    }
}

// ---------------------------------------------------------------------------
// Status pills
// ---------------------------------------------------------------------------

/// A status badge kind rendered by [`pill`]. Each maps to a glyph, a short
/// label, and a background colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillKind {
    /// Passed / succeeded (green).
    Pass,
    /// Failed / errored (red).
    Fail,
    /// Warning / advisory (amber).
    Warn,
    /// Skipped / not applicable (dim).
    Skip,
    /// Currently running (molten).
    Running,
    /// Awaiting a human decision (amber).
    Await,
    /// Completed / done (green, softer than Pass).
    Done,
}

impl PillKind {
    /// Glyph + label shown inside the pill, e.g. `("✔", "PASS")`.
    #[must_use]
    pub fn glyph_label(self) -> (&'static str, &'static str) {
        match self {
            Self::Pass => ("✔", "PASS"),
            Self::Fail => ("✘", "FAIL"),
            Self::Warn => ("⚠", "WARN"),
            Self::Skip => ("⊘", "SKIP"),
            Self::Running => ("⧗", "RUNNING"),
            Self::Await => ("⏸", "AWAIT"),
            Self::Done => ("✔", "DONE"),
        }
    }

    /// Background colour of the inverted badge.
    #[must_use]
    pub fn bg(self) -> Color {
        let p = palette();
        match self {
            Self::Pass | Self::Done => p.success,
            Self::Fail => p.error,
            Self::Warn | Self::Await => p.warn,
            Self::Skip => p.text_dim,
            Self::Running => p.molten,
        }
    }
}

/// Builds an inverted-background status pill span: ` ✔ PASS ` on a coloured
/// field with an auto-contrasted foreground. When `no_color` is set the badge
/// degrades to a plain bracketed label so it still reads.
#[must_use]
pub fn pill(kind: PillKind, no_color: bool) -> Span<'static> {
    let (glyph, label) = kind.glyph_label();
    if no_color {
        return Span::raw(format!("[{glyph} {label}]"));
    }
    let bg = kind.bg();
    Span::styled(
        format!(" {glyph} {label} "),
        Style::default()
            .bg(bg)
            .fg(contrast_fg(bg))
            .add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microbar_zero_and_full() {
        assert_eq!(microbar(0.0, 100.0, 4), "    ");
        assert_eq!(microbar(100.0, 100.0, 4), "████");
    }

    #[test]
    fn microbar_pads_to_exact_width() {
        // char count (not byte len) must equal the requested width.
        for v in [0.0, 12.5, 33.3, 50.0, 87.0, 100.0] {
            let bar = microbar(v, 100.0, 10);
            assert_eq!(bar.chars().count(), 10, "width for value {v}");
        }
    }

    #[test]
    fn microbar_uses_fractional_eighth() {
        // Half of one cell in a width-1 bar → the 4/8 glyph '▌'.
        assert_eq!(microbar(50.0, 100.0, 1), "▌");
        // 1/8 of a single cell → the thinnest glyph.
        assert_eq!(microbar(12.5, 100.0, 1), "▏");
    }

    #[test]
    fn microbar_clamps_over_max() {
        assert_eq!(microbar(200.0, 100.0, 3), "███");
    }

    #[test]
    fn microbar_zero_max_is_blank() {
        assert_eq!(microbar(5.0, 0.0, 3), "   ");
        assert_eq!(microbar(5.0, 10.0, 0), "");
    }

    #[test]
    fn microbar_ascii_fills_and_pads() {
        assert_eq!(microbar_ascii(0.0, 10.0, 4), "----");
        assert_eq!(microbar_ascii(10.0, 10.0, 4), "####");
        assert_eq!(microbar_ascii(5.0, 10.0, 4), "##--");
    }

    #[test]
    fn microbar_mode_switches_glyphs() {
        assert_eq!(microbar_mode(5.0, 10.0, 4, RenderMode::Ascii), "##--");
        assert!(microbar_mode(10.0, 10.0, 4, RenderMode::Block).contains('█'));
        assert!(microbar_mode(10.0, 10.0, 4, RenderMode::Braille).contains('█'));
    }

    #[test]
    fn zone_color_thresholds() {
        assert_eq!(zone_color(0), palette().success);
        assert_eq!(zone_color(59), palette().success);
        assert_eq!(zone_color(60), palette().warn);
        assert_eq!(zone_color(84), palette().warn);
        assert_eq!(zone_color(85), palette().error);
        assert_eq!(zone_color(100), palette().error);
    }

    #[test]
    fn render_mode_has_blocks() {
        assert!(RenderMode::Braille.has_blocks());
        assert!(RenderMode::Block.has_blocks());
        assert!(!RenderMode::Ascii.has_blocks());
    }

    #[test]
    fn pill_no_color_is_bracketed_label() {
        let span = pill(PillKind::Pass, true);
        assert_eq!(span.content.as_ref(), "[✔ PASS]");
    }

    #[test]
    fn pill_colored_has_inverted_bg() {
        let span = pill(PillKind::Fail, false);
        assert_eq!(span.style.bg, Some(palette().error));
        assert!(span.content.contains("FAIL"));
        assert!(span.content.contains('✘'));
    }

    #[test]
    fn pill_kinds_have_distinct_glyphs_and_bgs() {
        assert_eq!(PillKind::Running.glyph_label().0, "⧗");
        assert_eq!(PillKind::Running.bg(), palette().molten);
        assert_eq!(PillKind::Await.glyph_label().0, "⏸");
        assert_eq!(PillKind::Skip.bg(), palette().text_dim);
        assert_eq!(PillKind::Warn.glyph_label(), ("⚠", "WARN"));
    }
}
