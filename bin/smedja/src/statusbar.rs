//! Modular status bar — composable segments rendered to a single line.

/// Context passed to each status module at render time.
pub struct ModuleCtx<'a> {
    pub session_id: &'a str,
    pub mode: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub pending: bool,
}

/// A single status bar segment.
#[allow(dead_code)] // public API — used by future module implementations
pub struct Segment {
    pub text: String,
}

/// Renders the status bar as a single string from ordered segments.
pub fn render_status_bar(ctx: &ModuleCtx<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();

    // tier
    let tier = ctx.tier.unwrap_or("fast");
    parts.push(format!("[{tier}]"));

    // mode
    let mode = ctx.mode.unwrap_or("impl");
    parts.push(format!("[{mode}]"));

    // pending indicator
    if ctx.pending {
        parts.push("\u{27f3}".to_owned());
    }

    // session (truncated)
    let sess = ctx.session_id.chars().take(8).collect::<String>();
    parts.push(format!("[{sess}]"));

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
            pending: false,
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
            pending: true,
        };
        assert!(render_status_bar(&ctx).contains('\u{27f3}'));
    }

    #[test]
    fn no_pending_when_idle() {
        let ctx = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            pending: false,
        };
        assert!(!render_status_bar(&ctx).contains('\u{27f3}'));
    }
}
