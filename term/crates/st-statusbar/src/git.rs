//! Git-backed modules: current branch and working-tree status.

use std::path::Path;

use crate::types::plain_segment;
use crate::{ModuleContext, Segment, StatusModule};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::make_ctx;

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
}
