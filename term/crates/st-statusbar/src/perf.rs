//! Turn-performance modules: token counts, latency, efficiency, and trace id.

use crate::types::plain_segment;
use crate::{ModuleContext, Segment, StatusModule};

/// Displays the most recent turn's token counts: `"{input}↑ {output}↓"`.
///
/// Returns `None` when either token count is absent.
pub struct TokensModule;

impl StatusModule for TokensModule {
    fn name(&self) -> &'static str {
        "tokens"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let input = ctx.input_tokens?;
        let output = ctx.output_tokens?;
        Some(plain_segment(
            "tokens",
            format!("{input}\u{2191} {output}\u{2193}"),
        ))
    }
}

/// Displays the most recent turn's latency.
///
/// Format: `"{n}ms"` for under 1 second, `"{n:.1}s"` otherwise.
/// Returns `None` when latency is absent.
pub struct LatencyModule;

impl StatusModule for LatencyModule {
    fn name(&self) -> &'static str {
        "latency"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let ms = ctx.latency_ms?;
        let text = if ms < 1000 {
            format!("{ms}ms")
        } else {
            #[allow(clippy::cast_precision_loss)]
            // ms ≤ u64::MAX; precision loss is acceptable for display
            let secs = ms as f64 / 1000.0;
            format!("{secs:.1}s")
        };
        Some(plain_segment("latency", text))
    }
}

/// Displays the cumulative token-economy efficiency headline.
///
/// Renders `"⬇ {pct}%"` from the efficiency ratio, falling back to
/// `"−{n} tok"` (tokens saved) when only the saved count is available. Returns
/// `None` when neither figure is present, so the segment never shows a
/// misleading zero — it simply does not render until the economy reports a value.
pub struct EfficiencyModule;

impl StatusModule for EfficiencyModule {
    fn name(&self) -> &'static str {
        "efficiency"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        if let Some(ratio) = ctx.efficiency_ratio {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // ratio is in [0, 1]; the rounded percentage fits a u32 for display
            let pct = (ratio * 100.0).round() as u32;
            return Some(plain_segment("efficiency", format!("\u{2b07} {pct}%")));
        }
        if let Some(saved) = ctx.tokens_saved {
            return Some(plain_segment("efficiency", format!("\u{2212}{saved} tok")));
        }
        None
    }
}

/// Displays the first 8 characters of the `trace_id` from the most recent turn.
///
/// Parses the W3C `traceparent` header (`version-trace_id-parent_id-flags`).
/// Returns `None` when no traceparent is available.
pub struct TraceModule;

impl StatusModule for TraceModule {
    fn name(&self) -> &'static str {
        "trace"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let tp = ctx.traceparent.as_deref()?;
        let trace_id = tp.split('-').nth(1).unwrap_or(tp);
        let short = &trace_id[..trace_id.len().min(8)];
        Some(plain_segment("trace", format!("trace:{short}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::make_ctx;

    // 17
    #[test]
    fn tokens_module_formats_up_down_arrows() {
        let ctx = ModuleContext {
            input_tokens: Some(412),
            output_tokens: Some(88),
            ..make_ctx()
        };
        let seg = TokensModule.evaluate(&ctx).expect("should return Some");
        assert!(
            seg.text.contains("412"),
            "expected input count in '{}'",
            seg.text
        );
        assert!(
            seg.text.contains("88"),
            "expected output count in '{}'",
            seg.text
        );
        assert!(
            seg.text.contains('\u{2191}'),
            "expected ↑ in '{}'",
            seg.text
        );
        assert!(
            seg.text.contains('\u{2193}'),
            "expected ↓ in '{}'",
            seg.text
        );
    }

    // 18
    #[test]
    fn tokens_module_none_when_missing() {
        assert!(TokensModule.evaluate(&make_ctx()).is_none());
        let ctx = ModuleContext {
            input_tokens: Some(10),
            ..make_ctx()
        };
        assert!(
            TokensModule.evaluate(&ctx).is_none(),
            "missing output_tokens must return None"
        );
    }

    // 19
    #[test]
    fn latency_module_sub_second_shows_ms() {
        let ctx = ModuleContext {
            latency_ms: Some(800),
            ..make_ctx()
        };
        let seg = LatencyModule.evaluate(&ctx).expect("should return Some");
        assert_eq!(seg.text, "800ms");
    }

    // 20
    #[test]
    fn latency_module_multi_second_shows_decimal_s() {
        let ctx = ModuleContext {
            latency_ms: Some(4200),
            ..make_ctx()
        };
        let seg = LatencyModule.evaluate(&ctx).expect("should return Some");
        assert_eq!(seg.text, "4.2s");
    }

    // 21
    #[test]
    fn latency_module_none_when_missing() {
        assert!(LatencyModule.evaluate(&make_ctx()).is_none());
    }

    #[test]
    fn efficiency_module_renders_ratio_as_percentage() {
        let ctx = ModuleContext {
            efficiency_ratio: Some(0.41),
            ..make_ctx()
        };
        let seg = EfficiencyModule.evaluate(&ctx).expect("should return Some");
        assert_eq!(seg.text, "\u{2b07} 41%");
    }

    #[test]
    fn efficiency_module_falls_back_to_tokens_saved() {
        let ctx = ModuleContext {
            efficiency_ratio: None,
            tokens_saved: Some(2_300_000),
            ..make_ctx()
        };
        let seg = EfficiencyModule.evaluate(&ctx).expect("should return Some");
        assert_eq!(seg.text, "\u{2212}2300000 tok");
    }

    #[test]
    fn efficiency_module_none_when_absent_no_misleading_zero() {
        // Neither figure present → no segment, rather than a misleading 0%.
        assert!(EfficiencyModule.evaluate(&make_ctx()).is_none());
    }

    // 22
    #[test]
    fn trace_module_extracts_first_eight_chars_of_trace_id() {
        let ctx = ModuleContext {
            traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned()),
            ..make_ctx()
        };
        let seg = TraceModule.evaluate(&ctx).expect("should return Some");
        assert_eq!(seg.text, "trace:4bf92f35");
    }

    // 23
    #[test]
    fn trace_module_none_when_missing() {
        assert!(TraceModule.evaluate(&make_ctx()).is_none());
    }
}
