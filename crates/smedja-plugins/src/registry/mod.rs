//! [`SkillRegistry`] — scans, finds, installs, updates and removes skills.

use std::path::PathBuf;

mod ops;
mod scan;
mod sync;

pub use sync::SyncResult;

/// Manages Claude Code skill files stored under a skills directory.
///
/// Skills are stored as either:
/// - `<skills_dir>/<name>/SKILL.md` (directory-based), or
/// - `<skills_dir>/<name>.md` (flat file).
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    pub(crate) skills_dir: PathBuf,
}

impl SkillRegistry {
    /// Opens the registry rooted at `skills_dir`. Does not scan immediately.
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            skills_dir: skills_dir.into(),
        }
    }

    /// Returns `~/.claude/skills` as the default registry path.
    ///
    /// # Panics
    ///
    /// Panics when the home directory cannot be determined (i.e. `$HOME` is
    /// unset). This is intentional: a tool that cannot locate its own config
    /// directory has no safe fallback.
    #[must_use]
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("HOME environment variable must be set");
        PathBuf::from(home).join(".claude").join("skills")
    }
}

#[cfg(test)]
mod tests {
    use super::SkillRegistry;

    #[test]
    fn default_path_ends_with_claude_skills() {
        let path = SkillRegistry::default_path();
        let components: Vec<_> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert!(
            components
                .windows(2)
                .any(|w| w[0] == ".claude" && w[1] == "skills"),
            "default path must end with .claude/skills, got: {}",
            path.display()
        );
    }
}
