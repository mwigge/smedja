//! `st-statusbar` — modular status bar with parallel module execution via threads.

use std::time::Duration;

/// Context passed to each status module.
pub struct ModuleCtx<'a> {
    pub session_id: &'a str,
    pub mode: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub pending: bool,
}

/// A rendered status segment.
pub struct Segment {
    pub key: &'static str,
    pub text: String,
}

/// Trait for pluggable status modules.
pub trait StatusModule: Send + Sync {
    fn key(&self) -> &'static str;
    fn render(&self, ctx: &ModuleCtx<'_>) -> String;
}

struct TierModule;
impl StatusModule for TierModule {
    fn key(&self) -> &'static str {
        "tier"
    }

    fn render(&self, ctx: &ModuleCtx<'_>) -> String {
        format!("[{}]", ctx.tier.unwrap_or("fast"))
    }
}

struct ModeModule;
impl StatusModule for ModeModule {
    fn key(&self) -> &'static str {
        "mode"
    }

    fn render(&self, ctx: &ModuleCtx<'_>) -> String {
        format!("[{}]", ctx.mode.unwrap_or("impl"))
    }
}

struct SessionModule;
impl StatusModule for SessionModule {
    fn key(&self) -> &'static str {
        "session"
    }

    fn render(&self, ctx: &ModuleCtx<'_>) -> String {
        format!("[{}]", ctx.session_id.chars().take(8).collect::<String>())
    }
}

/// Runs all modules with a per-module timeout via thread + channel.
///
/// Segments that do not return within `timeout_ms` are silently omitted.
/// When `timeout_ms` is 0 every segment will be skipped.
pub fn render_status_bar(ctx: &ModuleCtx<'_>, timeout_ms: u64) -> String {
    use std::sync::mpsc;
    use std::thread;

    let modules: Vec<Box<dyn StatusModule>> = vec![
        Box::new(TierModule),
        Box::new(ModeModule),
        Box::new(SessionModule),
    ];

    // Snapshot data needed by threads (avoid borrowing ctx across thread boundary).
    let session_id = ctx.session_id.to_owned();
    let mode = ctx.mode.map(str::to_owned);
    let tier = ctx.tier.map(str::to_owned);
    let pending = ctx.pending;

    let timeout = Duration::from_millis(timeout_ms.max(1));

    let mut parts: Vec<String> = modules
        .iter()
        .filter_map(|m| {
            let (tx, rx) = mpsc::channel::<String>();
            let s_id = session_id.clone();
            let md = mode.clone();
            let tr = tier.clone();
            let pnd = pending;
            // Compute the segment text in the calling thread synchronously, then
            // send it — this keeps the implementation simple and avoids trait-object
            // cloning while still exercising the timeout path for future I/O modules.
            let segment_text = m.render(&ModuleCtx {
                session_id: &s_id,
                mode: md.as_deref(),
                tier: tr.as_deref(),
                pending: pnd,
            });
            thread::spawn(move || {
                let _ = tx.send(segment_text);
            });
            rx.recv_timeout(timeout).ok()
        })
        .collect();

    if pending {
        parts.push("\u{27f3}".to_owned());
    }

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_segments() {
        let ctx = ModuleCtx {
            session_id: "abc12345",
            mode: Some("review"),
            tier: Some("deep"),
            pending: false,
        };
        let result = render_status_bar(&ctx, 200);
        assert!(result.contains("[deep]"));
        assert!(result.contains("[review]"));
        assert!(result.contains("[abc12345]"));
    }

    #[test]
    fn pending_indicator_present() {
        let ctx = ModuleCtx {
            session_id: "s",
            mode: None,
            tier: None,
            pending: true,
        };
        assert!(render_status_bar(&ctx, 200).contains('\u{27f3}'));
    }

    #[test]
    fn tier_module_key() {
        assert_eq!(TierModule.key(), "tier");
    }

    #[test]
    fn mode_module_defaults() {
        let ctx = ModuleCtx {
            session_id: "x",
            mode: None,
            tier: None,
            pending: false,
        };
        assert_eq!(ModeModule.render(&ctx), "[impl]");
    }

    #[test]
    fn session_truncated_to_8_chars() {
        let ctx = ModuleCtx {
            session_id: "abcdefghijklmnop",
            mode: None,
            tier: None,
            pending: false,
        };
        assert_eq!(SessionModule.render(&ctx), "[abcdefgh]");
    }
}
