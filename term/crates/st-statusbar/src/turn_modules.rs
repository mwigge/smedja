//! Agent and per-turn telemetry status modules: tier, model, context
//! utilisation, active task, token counts, latency, efficiency, and trace id.

use crate::{
    coloured_segment, floor_char_boundary, plain_segment, truncate, ModuleContext, Segment,
    StatusModule, FORGE_DEEP, FORGE_ERROR, FORGE_FAST, FORGE_LOCAL, FORGE_SUCCESS, FORGE_WARN,
};

/// Displays the current inference tier: `[local]`, `[fast]`, or `[deep]`.
pub struct TierModule;

impl StatusModule for TierModule {
    fn name(&self) -> &'static str {
        "tier"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let tier = ctx.tier.as_deref()?;
        let text = format!("[{tier}]");
        // Colour the badge by tier so the terminal reads like the TUI; an
        // unrecognised tier stays uncoloured rather than guessing a tone.
        match tier {
            "local" => Some(coloured_segment("tier", text, FORGE_LOCAL)),
            "fast" => Some(coloured_segment("tier", text, FORGE_FAST)),
            "deep" => Some(coloured_segment("tier", text, FORGE_DEEP)),
            _ => Some(plain_segment("tier", text)),
        }
    }
}

/// Displays the current model name, truncated to 20 characters.
pub struct ModelModule;

impl StatusModule for ModelModule {
    fn name(&self) -> &'static str {
        "model"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let model = ctx.model.as_deref()?;
        Some(plain_segment("model", truncate(model, 20)))
    }
}

/// Displays context-window utilisation with colour coding.
///
/// - [`FORGE_SUCCESS`] (forge green) — under 60 %
/// - [`FORGE_WARN`]    (forge amber) — 60–80 %
/// - [`FORGE_ERROR`]   (forge ember) — over 80 %
pub struct ContextPctModule;

impl StatusModule for ContextPctModule {
    fn name(&self) -> &'static str {
        "context_pct"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        if ctx.context_window == 0 {
            return None;
        }
        let pct = 100 * ctx.context_used / ctx.context_window;
        let text = format!("ctx: {pct}%");
        let fg = if pct < 60 {
            FORGE_SUCCESS
        } else if pct <= 80 {
            FORGE_WARN
        } else {
            FORGE_ERROR
        };
        Some(coloured_segment("context_pct", text, fg))
    }
}

/// Displays the active task description, truncated to 30 characters.
pub struct TaskModule;

impl StatusModule for TaskModule {
    fn name(&self) -> &'static str {
        "task"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let task = ctx.active_task.as_deref()?;
        Some(plain_segment("task", truncate(task, 30)))
    }
}

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
        let short = &trace_id[..floor_char_boundary(trace_id, 8)];
        Some(plain_segment("trace", format!("trace:{short}")))
    }
}
