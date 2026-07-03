//! Context-derived display modules: tier, model, context %, task, exit code,
//! app name, session id, cwd, and interface.

use crate::types::{coloured_segment, plain_segment, truncate};
use crate::{Color, ModuleContext, Segment, StatusModule};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::make_ctx;

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
        let seg = ExitCodeModule
            .evaluate(&ctx)
            .expect("should return Some for exit 1");
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

    // 11
    #[test]
    fn app_name_module_always_returns_smedja() {
        let ctx = make_ctx();
        let seg = AppNameModule
            .evaluate(&ctx)
            .expect("AppNameModule must return Some");
        assert_eq!(seg.text, "smedja");
    }

    #[test]
    fn session_id_module_returns_first_eight_chars() {
        let ctx = ModuleContext {
            session_id: Some("abcdef1234567890".to_owned()),
            ..make_ctx()
        };
        let seg = SessionIdModule
            .evaluate(&ctx)
            .expect("SessionIdModule must return Some");
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
        let seg = CwdModule
            .evaluate(&ctx)
            .expect("CwdModule must return Some");
        assert!(
            seg.text.starts_with('\u{2026}'),
            "long cwd must start with ellipsis"
        );
        assert!(
            seg.text.chars().count() <= 41,
            "truncated cwd must be at most 41 chars (ellipsis + 40)"
        );
    }

    #[test]
    fn cwd_module_returns_full_short_path() {
        let ctx = ModuleContext {
            cwd: Some("/home/user".to_owned()),
            ..make_ctx()
        };
        let seg = CwdModule
            .evaluate(&ctx)
            .expect("CwdModule must return Some");
        assert_eq!(seg.text, "/home/user");
    }
}
