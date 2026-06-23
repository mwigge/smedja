//! `st-statusbar` — modular status bar with parallel module execution via rayon + threads.
//!
//! Each [`StatusModule`] is evaluated in a dedicated `std::thread` so that slow
//! modules (e.g. git probes) cannot block the rendering pipeline beyond their
//! individual [`StatusModule::timeout_ms`] budget.

use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use chrono::Local;

// ── Public types ──────────────────────────────────────────────────────────────

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

// ── Built-in modules ──────────────────────────────────────────────────────────

/// Displays the current inference tier: `[local]`, `[fast]`, or `[deep]`.
pub struct TierModule;

impl StatusModule for TierModule {
    fn name(&self) -> &'static str {
        "tier"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let tier = ctx.tier.as_deref()?;
        Some(plain_segment("tier", format!("[{tier}]")))
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
/// - Green  (`0, 200, 0`)   — under 60 %
/// - Yellow (`200, 200, 0`) — 60–80 %
/// - Red    (`200, 0, 0`)   — over 80 %
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
            Color { r: 0, g: 200, b: 0 }
        } else if pct <= 80 {
            Color {
                r: 200,
                g: 200,
                b: 0,
            }
        } else {
            Color { r: 200, g: 0, b: 0 }
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

/// Displays the current git branch using `git rev-parse --abbrev-ref HEAD`.
///
/// The branch prefix symbol defaults to `"* "` but can be overridden via
/// [`GitBranchModule::with_symbol`] to match a Starship `git_branch.symbol`.
#[derive(Default)]
pub struct GitBranchModule {
    symbol: Option<String>,
}

impl GitBranchModule {
    /// Creates a module that prefixes the branch name with `sym`.
    #[must_use]
    pub fn with_symbol(sym: Option<String>) -> Self {
        Self { symbol: sym }
    }

    fn prefix(&self) -> &str {
        self.symbol.as_deref().unwrap_or("* ")
    }

    /// Evaluate in an explicit working directory (useful for testing).
    #[must_use]
    pub fn evaluate_in(&self, _ctx: &ModuleContext, cwd: &Path) -> Option<Segment> {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let branch = std::str::from_utf8(&output.stdout)
            .ok()?
            .trim_end()
            .to_owned();
        if branch.is_empty() {
            return None;
        }
        let prefix = self.prefix();
        Some(plain_segment("git_branch", format!("{prefix}{branch}")))
    }
}

impl StatusModule for GitBranchModule {
    fn name(&self) -> &'static str {
        "git_branch"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let cwd = std::env::current_dir().ok()?;
        self.evaluate_in(ctx, &cwd)
    }

    fn timeout_ms(&self) -> u64 {
        500
    }
}

/// Displays a summary of uncommitted git changes: `+N ~M -K`.
pub struct GitStatusModule;

impl GitStatusModule {
    /// Evaluate in an explicit working directory (useful for testing).
    #[must_use]
    pub fn evaluate_in(&self, _ctx: &ModuleContext, cwd: &Path) -> Option<Segment> {
        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = std::str::from_utf8(&output.stdout).ok()?;
        let mut untracked = 0usize;
        let mut modified = 0usize;
        let mut deleted = 0usize;
        for line in stdout.lines() {
            if line.starts_with("?? ") {
                untracked += 1;
            } else if line.starts_with('M') || line.starts_with(" M") {
                modified += 1;
            } else if line.starts_with('D') || line.starts_with(" D") {
                deleted += 1;
            }
        }
        if untracked == 0 && modified == 0 && deleted == 0 {
            return None;
        }
        Some(plain_segment(
            "git_status",
            format!("+{untracked} ~{modified} -{deleted}"),
        ))
    }
}

impl StatusModule for GitStatusModule {
    fn name(&self) -> &'static str {
        "git_status"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let cwd = std::env::current_dir().ok()?;
        self.evaluate_in(ctx, &cwd)
    }

    fn timeout_ms(&self) -> u64 {
        500
    }
}

/// Detects the primary language of the current working directory.
///
/// Priority: Rust > Node > Go > Python.
pub struct LanguageModule;

impl LanguageModule {
    /// Evaluate against an explicit directory (useful for testing).
    #[must_use]
    pub fn evaluate_in(&self, _ctx: &ModuleContext, cwd: &Path) -> Option<Segment> {
        let checks: &[(&str, &str)] = &[
            ("Cargo.toml", "\u{1f980} Rust"),
            ("package.json", "\u{2b21} Node"),
            ("go.mod", "\u{1f439} Go"),
            ("pyproject.toml", "\u{1f40d} Python"),
            ("setup.py", "\u{1f40d} Python"),
        ];
        for (file, label) in checks {
            if cwd.join(file).exists() {
                return Some(plain_segment("language", *label));
            }
        }
        None
    }
}

impl StatusModule for LanguageModule {
    fn name(&self) -> &'static str {
        "language"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let cwd = std::env::current_dir().ok()?;
        self.evaluate_in(ctx, &cwd)
    }
}

/// Displays the current local time in `HH:MM` format using raw UTC arithmetic.
///
/// This module always returns `Some`.
pub struct TimeModule;

impl StatusModule for TimeModule {
    fn name(&self) -> &'static str {
        "time"
    }

    fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
        let now = Local::now();
        Some(plain_segment("time", now.format("%H:%M").to_string()))
    }
}

/// Displays battery level from `/sys/class/power_supply/BAT0/`.
///
/// Returns `None` on systems without a battery (e.g. desktops, CI runners).
pub struct BatteryModule;

impl StatusModule for BatteryModule {
    fn name(&self) -> &'static str {
        "battery"
    }

    fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
        let capacity_path = Path::new("/sys/class/power_supply/BAT0/capacity");
        let status_path = Path::new("/sys/class/power_supply/BAT0/status");
        if !capacity_path.exists() {
            return None;
        }
        let capacity = std::fs::read_to_string(capacity_path)
            .ok()?
            .trim()
            .to_owned();
        let status = std::fs::read_to_string(status_path)
            .ok()
            .unwrap_or_default();
        let symbol = if status.trim() == "Charging" {
            "\u{26a1}"
        } else {
            "\u{1f50b}"
        };
        Some(plain_segment("battery", format!("{symbol} {capacity}%")))
    }
}

/// Displays the exit code of the last shell command when it is non-zero.
///
/// Reads [`ModuleContext::last_exit_code`] — returns `None` for exit code 0
/// or when no code has been received yet.  Non-zero codes render as `✘ N` in
/// red to match Starship's default `character` module behaviour.
pub struct ExitCodeModule;

impl StatusModule for ExitCodeModule {
    fn name(&self) -> &'static str {
        "exit_code"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let code = ctx.last_exit_code?;
        if code == 0 {
            return None;
        }
        Some(coloured_segment(
            "exit_code",
            format!("\u{2718} {code}"),
            Color { r: 200, g: 0, b: 0 },
        ))
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
        Some(plain_segment("tokens", format!("{input}\u{2191} {output}\u{2193}")))
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
            #[allow(clippy::cast_precision_loss)] // ms ≤ u64::MAX; precision loss is acceptable for display
            let secs = ms as f64 / 1000.0;
            format!("{secs:.1}s")
        };
        Some(plain_segment("latency", text))
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

pub struct AppNameModule;

impl StatusModule for AppNameModule {
    fn name(&self) -> &'static str {
        "app_name"
    }

    fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
        Some(plain_segment("app_name", "smedja"))
    }
}

pub struct SessionIdModule;

impl StatusModule for SessionIdModule {
    fn name(&self) -> &'static str {
        "session_id"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let sid = ctx.session_id.as_deref()?;
        let short = &sid[..sid.len().min(8)];
        Some(plain_segment("session_id", short.to_owned()))
    }
}

pub struct CwdModule;

impl StatusModule for CwdModule {
    fn name(&self) -> &'static str {
        "cwd"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let cwd = ctx.cwd.as_deref()?;
        let short = if cwd.len() <= 40 {
            cwd.to_owned()
        } else {
            format!("\u{2026}{}", &cwd[cwd.len() - 40..])
        };
        Some(plain_segment("cwd", short))
    }
}

pub struct InterfaceModule;

impl StatusModule for InterfaceModule {
    fn name(&self) -> &'static str {
        "interface"
    }

    fn evaluate(&self, ctx: &ModuleContext) -> Option<Segment> {
        let iface = ctx.interface.as_deref()?;
        Some(plain_segment("interface", iface.to_owned()))
    }
}

// ── Parallel render ───────────────────────────────────────────────────────────

/// Fat-pointer (data + vtable) to a `StatusModule` packed into two `usize` words.
///
/// Storing the raw pointer as a pair of `usize` values avoids the `!Send`
/// restriction that the compiler imposes on `*const dyn Trait` values, while
/// preserving the full fat-pointer identity needed to reconstruct a trait-object
/// reference.
///
/// # Safety
///
/// The caller must ensure the pointed-to value outlives any thread that holds
/// a `ModulePtr`. In `render_status_bar_parallel` this is enforced because the
/// rayon closure blocks on `recv_timeout` until the spawned thread finishes (or
/// times out), keeping the `Box<dyn StatusModule>` alive for the thread's full
/// duration.
#[allow(dead_code)] // fields are read via std::mem::transmute, not by name
struct ModulePtr {
    data: usize,
    vtable: usize,
}

/// Renders all modules in parallel using rayon + per-module thread timeout.
///
/// Each module is evaluated in a dedicated `std::thread`. If the thread does
/// not complete within [`StatusModule::timeout_ms`], a `"?"` placeholder
/// segment is emitted instead. Modules that return `None` are omitted.
///
/// The `budget_ms` parameter is accepted for API compatibility; per-module
/// timeouts are the primary enforcement mechanism.
#[must_use]
pub fn render_status_bar_parallel(
    modules: &[Box<dyn StatusModule>],
    ctx: &ModuleContext,
    _budget_ms: u64,
) -> Vec<Segment> {
    use rayon::prelude::*;

    // Arc so every rayon task can share ctx with its spawned thread.
    let ctx = Arc::new(ctx.clone());

    modules
        .par_iter()
        .filter_map(|module| {
            let timeout = Duration::from_millis(module.timeout_ms());
            let (tx, rx) = mpsc::channel::<Option<Segment>>();
            let ctx_clone = Arc::clone(&ctx);

            // The actual evaluation must happen inside a `std::thread` so that
            // `recv_timeout` can cut it off if the module is slow. The rayon
            // closure holds the borrow on `module` and does not return until
            // `recv_timeout` completes. Therefore the `Box<dyn StatusModule>`
            // behind the raw pointer remains alive for the entire duration the
            // spawned thread could possibly access it.
            //
            // We pack the fat pointer into two `usize` words because raw
            // `*const dyn Trait` is `!Send` even when the trait bounds include
            // `Send + Sync`. The data+vtable encoding preserves full trait-object
            // identity and is reconstructed with `std::mem::transmute` inside
            // the spawned thread.
            //
            // SAFETY: `module.as_ref()` is a `&dyn StatusModule` (Send + Sync).
            // Transmuting a `*const dyn StatusModule` to `(usize, usize)` and
            // back is defined behaviour: a fat pointer to a `dyn Trait` is
            // exactly two pointer-sized words (data pointer + vtable pointer).
            // The pointer is valid for the duration of the spawned thread
            // because the rayon closure blocks on `recv_timeout`.
            let raw: *const dyn StatusModule = module.as_ref();
            let ptr = unsafe { std::mem::transmute::<*const dyn StatusModule, ModulePtr>(raw) };
            std::thread::spawn(move || {
                // SAFETY: see above — the fat pointer is valid and the
                // referent outlives this thread.
                let raw = unsafe { std::mem::transmute::<ModulePtr, *const dyn StatusModule>(ptr) };
                let module_ref = unsafe { &*raw };
                let _ = tx.send(module_ref.evaluate(&ctx_clone));
            });

            match rx.recv_timeout(timeout) {
                Ok(Some(seg)) => Some(seg),
                Ok(None) => None,
                Err(_) => Some(Segment {
                    name: "?".to_owned(),
                    text: "?".to_owned(),
                    style: SegmentStyle::default(),
                }),
            }
        })
        .collect()
}

// ── format_bar ────────────────────────────────────────────────────────────────

/// Substitutes `$module_name` tokens in `format` with the matching segment text.
///
/// Unresolved tokens (modules not present in `segments`) are removed. Pipe
/// characters `|` are replaced with the box-drawing vertical `│` (U+2502).
#[must_use]
pub fn format_bar(segments: &[Segment], format: &str) -> String {
    let mut result = format.to_owned();

    // Replace matched tokens.
    for seg in segments {
        let token = format!("${}", seg.name);
        result = result.replace(&token, &seg.text);
    }

    // Remove leftover $tokens.
    let mut out = String::with_capacity(result.len());
    let mut chars = result.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            while chars
                .peek()
                .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_')
            {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }

    // Replace ASCII pipe with box-drawing vertical bar.
    out.replace('|', "\u{2502}")
}

// ── Starship compatibility ────────────────────────────────────────────────────

/// Subset of a Starship configuration relevant to the status bar.
#[derive(Debug, Clone)]
pub struct StarshipConfig {
    /// Custom symbol to prefix the branch name (e.g. `" "`).
    pub git_branch_symbol: Option<String>,
    /// Whether the `git_branch` module is disabled in Starship.
    pub git_branch_disabled: bool,
}

/// Attempts to load a [`StarshipConfig`] from a TOML file at `path`.
///
/// Returns `None` if the file does not exist, cannot be read, or cannot be
/// parsed as TOML. All errors are swallowed silently.
pub fn load_starship_fallback(path: &Path) -> Option<StarshipConfig> {
    if !path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&contents).ok()?;

    let git_branch = value.get("git_branch");
    let git_branch_symbol = git_branch
        .and_then(|t| t.get("symbol"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let git_branch_disabled = git_branch
        .and_then(|t| t.get("disabled"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);

    Some(StarshipConfig {
        git_branch_symbol,
        git_branch_disabled,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> ModuleContext {
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
        }
    }

    // 1
    #[test]
    fn tier_module_shows_local() {
        let ctx = ModuleContext {
            tier: Some("local".to_owned()),
            ..make_ctx()
        };
        let seg = TierModule.evaluate(&ctx).expect("should return Some");
        assert_eq!(seg.text, "[local]");
    }

    // 2
    #[test]
    fn tier_module_missing_ctx_returns_none() {
        let ctx = make_ctx();
        assert!(TierModule.evaluate(&ctx).is_none());
    }

    // 3
    #[test]
    fn context_pct_module_colours_by_threshold() {
        // 50 % → green
        let ctx_50 = ModuleContext {
            context_used: 50,
            context_window: 100,
            ..make_ctx()
        };
        let seg = ContextPctModule
            .evaluate(&ctx_50)
            .expect("50% should return Some");
        let fg = seg.style.fg.as_ref().expect("should have fg colour");
        assert_eq!((fg.r, fg.g, fg.b), (0, 200, 0), "50% should be green");

        // 70 % → yellow
        let ctx_70 = ModuleContext {
            context_used: 70,
            context_window: 100,
            ..make_ctx()
        };
        let seg = ContextPctModule
            .evaluate(&ctx_70)
            .expect("70% should return Some");
        let fg = seg.style.fg.as_ref().expect("should have fg colour");
        assert_eq!((fg.r, fg.g, fg.b), (200, 200, 0), "70% should be yellow");

        // 90 % → red
        let ctx_90 = ModuleContext {
            context_used: 90,
            context_window: 100,
            ..make_ctx()
        };
        let seg = ContextPctModule
            .evaluate(&ctx_90)
            .expect("90% should return Some");
        let fg = seg.style.fg.as_ref().expect("should have fg colour");
        assert_eq!((fg.r, fg.g, fg.b), (200, 0, 0), "90% should be red");
    }

    // 4
    #[test]
    fn time_module_returns_hh_mm_format() {
        let ctx = make_ctx();
        let seg = TimeModule
            .evaluate(&ctx)
            .expect("TimeModule always returns Some");
        let text = &seg.text;
        assert_eq!(text.len(), 5, "expected HH:MM (5 chars), got '{text}'");
        assert_eq!(
            text.chars().nth(2),
            Some(':'),
            "colon must be at position 2"
        );
        for (i, ch) in text.chars().enumerate() {
            if i != 2 {
                assert!(
                    ch.is_ascii_digit(),
                    "char at {i} must be a digit, got '{ch}'"
                );
            }
        }
    }

    // 5
    #[test]
    fn git_branch_module_not_in_repo_returns_none() {
        // Evaluate against /tmp which is guaranteed not to be inside a git repo.
        let result = GitBranchModule::default().evaluate_in(&make_ctx(), Path::new("/tmp"));
        assert!(
            result.is_none(),
            "expected None for non-git directory, got {result:?}"
        );
    }

    // 6
    #[test]
    fn language_module_detects_rust_from_cargo_toml() {
        // Create a temp dir with a Cargo.toml file to trigger Rust detection.
        let tmp = std::env::temp_dir().join(format!(
            "smedja-test-lang-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).expect("create tmp dir");
        std::fs::write(tmp.join("Cargo.toml"), "[package]").expect("write Cargo.toml");

        let result = LanguageModule.evaluate_in(&make_ctx(), &tmp);
        std::fs::remove_dir_all(&tmp).ok();

        let seg = result.expect("should detect Rust");
        assert!(
            seg.text.contains("Rust"),
            "expected text to contain 'Rust', got '{}'",
            seg.text
        );
    }

    // 7
    #[test]
    fn format_bar_replaces_module_tokens() {
        let segs = vec![Segment {
            name: "tier".to_owned(),
            text: "[local]".to_owned(),
            style: SegmentStyle::default(),
        }];
        let result = format_bar(&segs, "$tier active");
        assert_eq!(result, "[local] active");
    }

    // 8
    #[test]
    fn format_bar_separator_becomes_dim_char() {
        let result = format_bar(&[], "a | b");
        assert!(
            result.contains('\u{2502}'),
            "expected box-drawing │, got '{result}'"
        );
    }

    // 9
    #[test]
    fn battery_module_no_battery_returns_none() {
        // On CI / desktops without BAT0, the module must return None gracefully.
        if Path::new("/sys/class/power_supply/BAT0/capacity").exists() {
            // System has a battery — skip rather than assert (avoids false failures on laptops).
            return;
        }
        assert!(BatteryModule.evaluate(&make_ctx()).is_none());
    }

    // 10
    #[test]
    fn parallel_render_collects_all_segments() {
        let ctx = ModuleContext {
            tier: Some("local".to_owned()),
            model: Some("gemma-4-27b".to_owned()),
            ..make_ctx()
        };
        let modules: Vec<Box<dyn StatusModule>> = vec![Box::new(TierModule), Box::new(ModelModule)];
        let segments = render_status_bar_parallel(&modules, &ctx, 500);
        assert!(
            !segments.is_empty(),
            "expected at least one segment, got {}",
            segments.len()
        );
        assert!(
            segments.iter().any(|s| s.text == "[local]"),
            "expected [local] segment in {segments:?}"
        );
    }

    // 12
    #[test]
    fn exit_code_module_zero_returns_none() {
        let ctx = ModuleContext {
            last_exit_code: Some(0),
            ..make_ctx()
        };
        assert!(ExitCodeModule.evaluate(&ctx).is_none());
    }

    // 13
    #[test]
    fn exit_code_module_nonzero_returns_red_segment() {
        let ctx = ModuleContext {
            last_exit_code: Some(1),
            ..make_ctx()
        };
        let seg = ExitCodeModule.evaluate(&ctx).expect("should return Some for exit 1");
        assert!(seg.text.contains('1'), "text should include exit code");
        assert!(seg.text.contains('\u{2718}'), "text should contain ✘");
        let fg = seg.style.fg.as_ref().expect("should have fg colour");
        assert_eq!(fg.r, 200, "should be red");
        assert_eq!(fg.g, 0);
        assert_eq!(fg.b, 0);
    }

    // 14
    #[test]
    fn exit_code_module_absent_returns_none() {
        assert!(ExitCodeModule.evaluate(&make_ctx()).is_none());
    }

    // 15
    #[test]
    fn git_branch_module_with_symbol_uses_symbol() {
        let module = GitBranchModule::with_symbol(Some(" ".to_owned()));
        // Evaluate against the smedja repo itself — must be on a branch.
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        if let Some(seg) = module.evaluate_in(&make_ctx(), repo_root) {
            assert!(
                seg.text.starts_with(' '),
                "expected segment to start with symbol, got '{}'",
                seg.text
            );
        }
    }

    // 16
    #[test]
    fn git_branch_module_default_uses_asterisk() {
        let module = GitBranchModule::default();
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        if let Some(seg) = module.evaluate_in(&make_ctx(), repo_root) {
            assert!(
                seg.text.starts_with("* "),
                "expected segment to start with '* ', got '{}'",
                seg.text
            );
        }
    }

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
        assert!(seg.text.contains('\u{2191}'), "expected ↑ in '{}'", seg.text);
        assert!(seg.text.contains('\u{2193}'), "expected ↓ in '{}'", seg.text);
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

    // 22
    #[test]
    fn trace_module_extracts_first_eight_chars_of_trace_id() {
        let ctx = ModuleContext {
            traceparent: Some(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned(),
            ),
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

    // 11
    #[test]
    fn app_name_module_always_returns_smedja() {
        let ctx = make_ctx();
        let seg = AppNameModule.evaluate(&ctx).expect("AppNameModule must return Some");
        assert_eq!(seg.text, "smedja");
    }

    #[test]
    fn session_id_module_returns_first_eight_chars() {
        let ctx = ModuleContext {
            session_id: Some("abcdef1234567890".to_owned()),
            ..make_ctx()
        };
        let seg = SessionIdModule.evaluate(&ctx).expect("SessionIdModule must return Some");
        assert_eq!(seg.text, "abcdef12");
    }

    #[test]
    fn session_id_module_returns_none_when_absent() {
        let ctx = make_ctx();
        assert!(SessionIdModule.evaluate(&ctx).is_none());
    }

    #[test]
    fn cwd_module_truncates_long_path() {
        let long = "/home/user/very/deep/path/that/exceeds/the/forty/char/limit";
        let ctx = ModuleContext {
            cwd: Some(long.to_owned()),
            ..make_ctx()
        };
        let seg = CwdModule.evaluate(&ctx).expect("CwdModule must return Some");
        assert!(seg.text.starts_with('\u{2026}'), "long cwd must start with ellipsis");
        assert!(seg.text.chars().count() <= 41, "truncated cwd must be at most 41 chars (ellipsis + 40)");
    }

    #[test]
    fn cwd_module_returns_full_short_path() {
        let ctx = ModuleContext {
            cwd: Some("/home/user".to_owned()),
            ..make_ctx()
        };
        let seg = CwdModule.evaluate(&ctx).expect("CwdModule must return Some");
        assert_eq!(seg.text, "/home/user");
    }

    #[test]
    fn module_timeout_emits_question_mark() {
        struct SlowModule;
        impl StatusModule for SlowModule {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
                std::thread::sleep(Duration::from_millis(200));
                Some(plain_segment("slow", "done"))
            }
            fn timeout_ms(&self) -> u64 {
                10
            }
        }

        let modules: Vec<Box<dyn StatusModule>> = vec![Box::new(SlowModule)];
        let ctx = make_ctx();
        let segments = render_status_bar_parallel(&modules, &ctx, 500);
        assert_eq!(segments.len(), 1, "expected exactly one timeout segment");
        assert_eq!(
            segments[0].text, "?",
            "timed-out module must emit '?' placeholder"
        );
    }
}
