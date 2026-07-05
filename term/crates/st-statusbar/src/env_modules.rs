//! Shell and environment status modules: git branch, time, exit code,
//! app name, session id, and working directory.

use std::path::Path;

use chrono::Local;

use crate::{
    coloured_segment, floor_char_boundary, plain_segment, ModuleContext, Segment, StatusModule,
    FORGE_ERROR,
};

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
            FORGE_ERROR,
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
        let short = &sid[..floor_char_boundary(sid, 8)];
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
        // Keep the trailing 40 *characters*, not bytes: slicing `&cwd[len-40..]`
        // at a raw byte offset can start mid-codepoint (accented/CJK paths) and
        // panic the renderer.
        let char_count = cwd.chars().count();
        let short = if char_count <= 40 {
            cwd.to_owned()
        } else {
            let tail: String = cwd.chars().skip(char_count - 40).collect();
            format!("\u{2026}{tail}")
        };
        Some(plain_segment("cwd", short))
    }
}
