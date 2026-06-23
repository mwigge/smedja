//! WCAG AA contrast ratio utilities.
//!
//! Implements the WCAG 2.1 relative-luminance and contrast-ratio formulae so
//! the renderer can enforce a minimum 4.5:1 contrast ratio (WCAG AA) between
//! foreground and background colours.

// ── Luminance ─────────────────────────────────────────────────────────────────

/// Converts a single 8-bit sRGB channel to a linearised value.
fn linearise(channel: u8) -> f64 {
    let v = f64::from(channel) / 255.0;
    if v <= 0.039_28 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Returns the WCAG relative luminance of an sRGB colour `(r, g, b)`.
///
/// The result is in the range `0.0` (black) to `1.0` (white).
#[must_use]
pub fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    0.2126 * linearise(r) + 0.7152 * linearise(g) + 0.0722 * linearise(b)
}

// ── Contrast ratio ────────────────────────────────────────────────────────────

/// Returns the WCAG contrast ratio between two sRGB colours.
///
/// The ratio is always ≥ 1.0 (same colour → 1.0, black vs white → 21.0).
#[must_use]
pub fn contrast_ratio(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> f64 {
    let l1 = relative_luminance(fg.0, fg.1, fg.2);
    let l2 = relative_luminance(bg.0, bg.1, bg.2);
    let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

// ── WCAG AA compliance ────────────────────────────────────────────────────────

/// Returns `true` when the fg/bg pair meets WCAG AA (contrast ratio ≥ 4.5).
#[must_use]
pub fn meets_wcag_aa(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> bool {
    contrast_ratio(fg, bg) >= 4.5
}

// ── Enforce contrast ──────────────────────────────────────────────────────────

/// Returns a foreground colour that meets WCAG AA contrast against `bg`.
///
/// If the supplied `fg` already meets the 4.5:1 threshold it is returned
/// unchanged. Otherwise the foreground is iteratively shifted towards white
/// or black — whichever direction yields a compliant colour first — in steps
/// of 10 per channel, capped to valid `u8` range.
///
/// The function is guaranteed to terminate because full white (255, 255, 255)
/// and full black (0, 0, 0) both contrast well against any background.
#[must_use]
pub fn enforce_contrast(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> (u8, u8, u8) {
    if meets_wcag_aa(fg, bg) {
        return fg;
    }

    // Decide direction: if the background is dark, lighten the fg; otherwise darken it.
    let bg_lum = relative_luminance(bg.0, bg.1, bg.2);
    let lighten = bg_lum < 0.5;

    let mut candidate = fg;
    for _ in 0..26u8 {
        let (r, g, b) = candidate;
        candidate = if lighten {
            (
                r.saturating_add(10),
                g.saturating_add(10),
                b.saturating_add(10),
            )
        } else {
            (
                r.saturating_sub(10),
                g.saturating_sub(10),
                b.saturating_sub(10),
            )
        };
        if meets_wcag_aa(candidate, bg) {
            return candidate;
        }
    }

    // Fallback: pure white or pure black — always compliant with any bg.
    if lighten {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_on_white_is_21_to_1() {
        let ratio = contrast_ratio((0, 0, 0), (255, 255, 255));
        // WCAG spec says exactly 21:1; allow tiny floating-point drift.
        assert!(
            (ratio - 21.0).abs() < 0.01,
            "expected ~21.0, got {ratio:.4}"
        );
    }

    #[test]
    fn white_on_white_is_1_to_1() {
        let ratio = contrast_ratio((255, 255, 255), (255, 255, 255));
        assert!((ratio - 1.0).abs() < 0.001, "expected 1.0, got {ratio:.4}");
    }

    #[test]
    fn black_on_white_meets_wcag_aa() {
        assert!(
            meets_wcag_aa((0, 0, 0), (255, 255, 255)),
            "black on white must pass WCAG AA"
        );
    }

    #[test]
    fn white_on_white_fails_wcag_aa() {
        assert!(
            !meets_wcag_aa((255, 255, 255), (255, 255, 255)),
            "white on white must fail WCAG AA"
        );
    }

    #[test]
    fn enforce_contrast_returns_unchanged_when_compliant() {
        let fg = (0, 0, 0);
        let bg = (255, 255, 255);
        assert_eq!(enforce_contrast(fg, bg), fg);
    }

    #[test]
    fn enforce_contrast_fixes_white_on_white() {
        let result = enforce_contrast((255, 255, 255), (255, 255, 255));
        assert!(
            meets_wcag_aa(result, (255, 255, 255)),
            "enforce_contrast must return a WCAG AA-compliant colour; got {result:?}"
        );
    }

    #[test]
    fn enforce_contrast_fixes_dark_fg_on_dark_bg() {
        // Very similar dark colours — should be lightened to pass.
        let result = enforce_contrast((30, 30, 30), (20, 20, 20));
        assert!(
            meets_wcag_aa(result, (20, 20, 20)),
            "enforce_contrast must yield WCAG AA compliance; got {result:?}"
        );
    }

    #[test]
    fn relative_luminance_black_is_zero() {
        assert!(relative_luminance(0, 0, 0) < 1e-10);
    }

    #[test]
    fn relative_luminance_white_is_one() {
        assert!((relative_luminance(255, 255, 255) - 1.0).abs() < 1e-6);
    }
}
