//! `st-statusbar` — modular status bar with parallel module execution via rayon + threads.
//!
//! Each [`StatusModule`] is evaluated in a dedicated `std::thread` so that slow
//! modules (e.g. git probes) cannot block the rendering pipeline beyond their
//! individual [`StatusModule::timeout_ms`] budget.

// ── UTF-8 helpers ──────────────────────────────────────────────────────────────

/// Returns the largest byte index `<= max` that lies on a UTF-8 char boundary.
///
/// Status text (`trace_id`, `session_id`, …) can carry multibyte codepoints, so
/// slicing a raw byte offset like `&s[..8]` may land mid-codepoint and panic the
/// renderer. Flooring to the nearest boundary makes `&s[..floor_char_boundary(s,
/// max)]` always safe without exceeding the requested byte budget.
#[must_use]
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// ── Public types ──────────────────────────────────────────────────────────────

/// A 24-bit RGB colour value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

// ── Forge palette ───────────────────────────────────────────────────────────────
//
// Shared status-bar accent tones, matching the smedja-tui forge palette exactly
// so the GPU terminal and the TUI read as one system.

/// Forge green — success / healthy / low utilisation.
pub const FORGE_SUCCESS: Color = Color {
    r: 122,
    g: 202,
    b: 142,
};
/// Forge amber — warning / mid utilisation.
pub const FORGE_WARN: Color = Color {
    r: 243,
    g: 197,
    b: 92,
};
/// Forge ember — error / high utilisation.
pub const FORGE_ERROR: Color = Color {
    r: 240,
    g: 120,
    b: 72,
};
/// Forge teal — the `local` inference tier.
pub const FORGE_LOCAL: Color = Color {
    r: 78,
    g: 185,
    b: 178,
};
/// Forge gold — the `fast` inference tier.
pub const FORGE_FAST: Color = Color {
    r: 247,
    g: 199,
    b: 126,
};
/// Forge copper — the `deep` inference tier.
pub const FORGE_DEEP: Color = Color {
    r: 169,
    g: 101,
    b: 47,
};

/// Returns the published maximum context-window size (in tokens) for a model id.
///
/// The match is a case-insensitive substring test against well-known model
/// families; the returned figures are each vendor's *published maximum* context
/// window, not a per-request limit. Unknown models fall back to a conservative
/// 128 K default.
#[must_use]
pub fn model_context_window(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    let has = |needle: &str| m.contains(needle);
    if has("claude") || has("opus") || has("sonnet") || has("haiku") {
        200_000
    } else if has("gemini") {
        1_000_000
    } else if has("gemma") {
        8_192
    } else if has("qwen") {
        32_768
    } else {
        // gpt-4o / gpt-4.1 / o1 / o3 / llama, and any unrecognised model, all
        // land on the conservative 128 K default.
        128_000
    }
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
    /// The module name, used by [`format_bar`] for `$name` token substitution.
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
    /// the [`EfficiencyModule`] silent rather than showing a misleading zero.
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

// ── Helper ────────────────────────────────────────────────────────────────────

fn plain_segment(name: &'static str, text: impl Into<String>) -> Segment {
    Segment {
        name: name.to_owned(),
        text: text.into(),
        style: SegmentStyle::default(),
    }
}

fn coloured_segment(name: &'static str, text: impl Into<String>, fg: Color) -> Segment {
    Segment {
        name: name.to_owned(),
        text: text.into(),
        style: SegmentStyle {
            fg: Some(fg),
            ..SegmentStyle::default()
        },
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_owned()
    } else {
        let mut t: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        t.push('\u{2026}'); // …
        t
    }
}
// ── Submodules ──────────────────────────────────────────────────────────────────

mod env_modules;
mod render;
mod starship;
mod turn_modules;

pub use env_modules::{
    AppNameModule, CwdModule, ExitCodeModule, GitBranchModule, SessionIdModule, TimeModule,
};
pub use render::{format_bar, render_status_bar_parallel};
pub use starship::{load_starship_fallback, StarshipConfig};
pub use turn_modules::{
    ContextPctModule, EfficiencyModule, LatencyModule, ModelModule, TaskModule, TierModule,
    TokensModule, TraceModule,
};

#[cfg(test)]
mod tests;
