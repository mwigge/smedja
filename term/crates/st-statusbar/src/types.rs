//! Core public types shared by every status-bar module, plus segment helpers.

/// A 24-bit RGB colour value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Visual style applied to a rendered [`Segment`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
}

/// A single rendered segment produced by a [`StatusModule`].
#[derive(Debug, Clone)]
pub struct Segment {
    /// The module name, used by [`crate::format_bar`] for `$name` token substitution.
    pub name: String,
    /// Human-readable text to display.
    pub text: String,
    /// Visual style to apply when rendering.
    pub style: SegmentStyle,
}

/// Context passed to every [`StatusModule::evaluate`] call.
#[derive(Debug, Clone)]
pub struct ModuleContext {
    /// Inference tier: `"local"`, `"fast"`, or `"deep"`.
    pub tier: Option<String>,
    /// Model identifier (e.g. `"gemma-4-27b-it"`).
    pub model: Option<String>,
    /// Number of tokens used in the current context window.
    pub context_used: usize,
    /// Maximum tokens the current context window supports.
    pub context_window: usize,
    /// Short description of the task currently in progress.
    pub active_task: Option<String>,
    /// Exit code of the last shell command (from OSC 133 D).
    pub last_exit_code: Option<i32>,
    /// Input token count from the most recent completed turn.
    pub input_tokens: Option<u64>,
    /// Output token count from the most recent completed turn.
    pub output_tokens: Option<u64>,
    /// Turn latency in milliseconds from the most recent completed turn.
    pub latency_ms: Option<u64>,
    /// W3C `traceparent` from the most recent completed turn.
    pub traceparent: Option<String>,
    /// Session or pane UUID (short form, first 8 chars used in displays).
    pub session_id: Option<String>,
    /// Current working directory of the terminal process.
    pub cwd: Option<String>,
    /// Interface mode: `"cli"` or `"tui"`.
    pub interface: Option<String>,
    /// Cumulative tokens saved by the token economy, when reported. `None` keeps
    /// the [`crate::EfficiencyModule`] silent rather than showing a misleading zero.
    pub tokens_saved: Option<u64>,
    /// Cumulative efficiency ratio `saved / (saved + billed_input)`, when reported.
    pub efficiency_ratio: Option<f64>,
}

// ── StatusModule trait ────────────────────────────────────────────────────────

/// A pluggable status-bar module.
///
/// Implementations must be `Send + Sync` so they can be evaluated across rayon
/// and `std::thread` boundaries.
pub trait StatusModule: Send + Sync {
    /// Short identifier used in format strings (e.g. `"tier"`, `"model"`).
    fn name(&self) -> &'static str;

    /// Produce a [`Segment`] for the given context, or `None` if this module
    /// has nothing to display.
    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment>;

    /// Maximum time in milliseconds the module may take before the render
    /// pipeline emits a `"?"` placeholder.
    fn timeout_ms(&self) -> u64 {
        30
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn plain_segment(name: &'static str, text: impl Into<String>) -> Segment {
    Segment {
        name: name.to_owned(),
        text: text.into(),
        style: SegmentStyle::default(),
    }
}

pub(crate) fn coloured_segment(name: &'static str, text: impl Into<String>, fg: Color) -> Segment {
    Segment {
        name: name.to_owned(),
        text: text.into(),
        style: SegmentStyle {
            fg: Some(fg),
            ..SegmentStyle::default()
        },
    }
}

pub(crate) fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_owned()
    } else {
        let mut t: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        t.push('\u{2026}'); // …
        t
    }
}

/// Shared test helper: a fully-empty [`ModuleContext`].
#[cfg(test)]
pub(crate) fn make_ctx() -> ModuleContext {
    ModuleContext {
        tier: None,
        model: None,
        context_used: 0,
        context_window: 0,
        active_task: None,
        last_exit_code: None,
        input_tokens: None,
        output_tokens: None,
        latency_ms: None,
        traceparent: None,
        session_id: None,
        cwd: None,
        interface: None,
        tokens_saved: None,
        efficiency_ratio: None,
    }
}
