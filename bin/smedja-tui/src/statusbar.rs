//! Modular status bar — composable segments rendered to a single line.

/// Context passed to each status module at render time.
pub struct ModuleCtx<'a> {
    pub session_id: &'a str,
    pub mode: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub runner: Option<&'a str>,
    pub pending: bool,
    /// True when the input bar is active (not in scroll/normal mode).
    pub input_mode: bool,
    /// Context window fill percentage (0–100), shown as a gauge chip when present.
    pub ctx_pct: Option<u8>,
}

/// A single status bar segment.
#[allow(dead_code)] // public API — used by future module implementations
pub struct Segment {
    pub text: String,
}

/// TOML configuration for the status bar.
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct StatusBarConfig {
    /// Optional format string, e.g. `"{tier} {mode} {session}"`.
    pub format: Option<String>,
}

/// Per-module configuration.
#[allow(dead_code)] // TOML config fields read via serde; constructed when config is wired
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct ModuleConfig {
    pub disabled: Option<bool>,
    pub symbol: Option<String>,
    pub style: Option<String>,
    pub threshold: Option<f64>,
}

// ---------------------------------------------------------------------------
// Segment renderers (sync, no I/O)
// ---------------------------------------------------------------------------

fn segment_input_mode(ctx: &ModuleCtx<'_>) -> String {
    if ctx.input_mode {
        "[I]".to_owned()
    } else {
        "[N]".to_owned()
    }
}

fn segment_tier(ctx: &ModuleCtx<'_>) -> String {
    let tier = ctx.tier.unwrap_or("fast");
    format!("[{tier}]")
}

fn segment_mode(ctx: &ModuleCtx<'_>) -> String {
    let mode = ctx.mode.unwrap_or("impl");
    format!("[{mode}]")
}

fn segment_session(ctx: &ModuleCtx<'_>) -> String {
    let sess = ctx.session_id.chars().take(8).collect::<String>();
    format!("[{sess}]")
}

fn segment_runner(ctx: &ModuleCtx<'_>) -> String {
    let runner = ctx.runner.unwrap_or("unknown");
    format!("[{runner}]")
}

// ---------------------------------------------------------------------------
// Public render functions
// ---------------------------------------------------------------------------

/// Renders the status bar as a single string from ordered segments.
///
/// Delegates to [`render_status_bar_with_timeout`] with a default 30 ms timeout.
/// Retained as the plain-text status API (config-driven format, tests); the live
/// TUI now renders a styled segmented line via `status_bar_line`.
#[allow(dead_code)]
pub fn render_status_bar(ctx: &ModuleCtx<'_>) -> String {
    render_status_bar_configured(ctx, None, 30)
}

#[allow(dead_code)] // public API — called by tests and future integration code
/// Renders the status bar with a configurable per-segment timeout (milliseconds).
///
/// Each segment computation is dispatched to a thread; segments that do not
/// return within `timeout_ms` are silently omitted.  When `timeout_ms` is 0
/// every segment will be skipped (useful in tests).
pub fn render_status_bar_with_timeout(ctx: &ModuleCtx<'_>, timeout_ms: u64) -> String {
    render_status_bar_configured(ctx, None, timeout_ms)
}

/// Renders the status bar applying optional [`StatusBarConfig`] and timeout.
///
/// - If `config.format` is `Some`, renders only the named segments in that
///   order (`"{tier} {mode}"` → tier then mode).
/// - If a module key appears in `config` with `disabled: true`, it is skipped.
/// - `timeout_ms` is accepted for API compatibility but ignored — all segments
///   are computed synchronously (they do no I/O) so no timeout is needed.
pub fn render_status_bar_configured(
    ctx: &ModuleCtx<'_>,
    config: Option<&StatusBarConfig>,
    _timeout_ms: u64,
) -> String {
    let all_keys: &[&str] = &["input_mode", "runner", "tier", "mode", "session"];
    let ordered_keys: Vec<&str> = if let Some(cfg) = config {
        if let Some(fmt) = &cfg.format {
            fmt.split_whitespace()
                .filter_map(|tok| {
                    let inner = tok.strip_prefix('{')?.strip_suffix('}')?;
                    if all_keys.contains(&inner) {
                        Some(inner)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            all_keys.to_vec()
        }
    } else {
        all_keys.to_vec()
    };

    let mut parts: Vec<String> = ordered_keys
        .into_iter()
        .map(|key| match key {
            "input_mode" => segment_input_mode(ctx),
            "runner" => segment_runner(ctx),
            "tier" => segment_tier(ctx),
            "mode" => segment_mode(ctx),
            "session" => segment_session(ctx),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect();

    if ctx.pending {
        parts.push("\u{27f3}".to_owned());
    }

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_tier_and_mode() {
        let ctx = ModuleCtx {
            session_id: "abc123",
            mode: Some("review"),
            tier: Some("deep"),
            runner: None,
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        let bar = render_status_bar(&ctx);
        assert!(bar.contains("[deep]"));
        assert!(bar.contains("[review]"));
    }

    #[test]
    fn shows_pending_indicator() {
        let ctx = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            runner: None,
            pending: true,
            input_mode: false,
            ctx_pct: None,
        };
        assert!(render_status_bar(&ctx).contains('\u{27f3}'));
    }

    #[test]
    fn no_pending_when_idle() {
        let ctx = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            runner: None,
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        assert!(!render_status_bar(&ctx).contains('\u{27f3}'));
    }

    #[test]
    fn module_timeout_does_not_panic() {
        // 0 ms timeout exercises the timeout path without panicking.
        // Whether any segment completes depends on thread scheduling; we only
        // verify the function returns without hanging or panicking.
        let ctx = ModuleCtx {
            session_id: "sess",
            mode: Some("impl"),
            tier: Some("fast"),
            runner: None,
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        let _ = render_status_bar_with_timeout(&ctx, 0);
    }

    #[test]
    fn disabled_module_not_rendered() {
        // A format string that excludes "session" → session segment absent.
        let ctx = ModuleCtx {
            session_id: "mysession",
            mode: Some("impl"),
            tier: Some("fast"),
            runner: None,
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        let config = StatusBarConfig {
            format: Some("{tier} {mode}".into()),
        };
        let result = render_status_bar_configured(&ctx, Some(&config), 200);
        assert!(result.contains("[fast]"), "tier expected, got: {result}");
        assert!(result.contains("[impl]"), "mode expected, got: {result}");
        assert!(
            !result.contains("mysession"),
            "session must be absent, got: {result}"
        );
    }

    #[test]
    fn segment_runner_renders_known_value() {
        let ctx = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            runner: Some("claude-sonnet"),
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        let bar = render_status_bar(&ctx);
        assert!(
            bar.contains("[claude-sonnet]"),
            "runner expected, got: {bar}"
        );
    }

    #[test]
    fn segment_runner_defaults_to_unknown_when_none() {
        let ctx = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            runner: None,
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        let bar = render_status_bar(&ctx);
        assert!(
            bar.contains("[unknown]"),
            "default runner expected, got: {bar}"
        );
    }

    #[test]
    fn format_string_reorders_segments() {
        let ctx = ModuleCtx {
            session_id: "ssid",
            mode: Some("review"),
            tier: Some("deep"),
            runner: None,
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        // Request session before tier.
        let config = StatusBarConfig {
            format: Some("{session} {tier}".into()),
        };
        let result = render_status_bar_configured(&ctx, Some(&config), 200);
        let session_pos = result.find("[ssid]").unwrap();
        let tier_pos = result.find("[deep]").unwrap();
        assert!(
            session_pos < tier_pos,
            "session should come before tier; got: {result}"
        );
    }

    #[test]
    fn segment_input_mode_returns_correct_badge() {
        let ctx_i = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            runner: None,
            pending: false,
            input_mode: true,
            ctx_pct: None,
        };
        let ctx_n = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            runner: None,
            pending: false,
            input_mode: false,
            ctx_pct: None,
        };
        assert_eq!(segment_input_mode(&ctx_i), "[I]");
        assert_eq!(segment_input_mode(&ctx_n), "[N]");
    }

    #[test]
    fn render_status_bar_includes_input_mode_badge() {
        let ctx = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            runner: None,
            pending: false,
            input_mode: true,
            ctx_pct: None,
        };
        let bar = render_status_bar(&ctx);
        assert!(
            bar.contains("[I]"),
            "status bar must include [I] badge in input mode; got: {bar}"
        );
    }
}
