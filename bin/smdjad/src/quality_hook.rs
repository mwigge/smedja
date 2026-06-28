//! Post-turn quality gate hook.
//!
//! After each [`TurnEvent::Completed`], smdjad calls [`run_after_turn`] on a
//! spawned Tokio task.  The hook runs the four Tier-1 deterministic gates, then
//! dispatches [`TurnEvent::QualitySnapshot`] on the shared dispatcher.
//!
//! All failures are soft — a broken git repo, missing diff, or any other error
//! produces a snapshot with the available gate results (defaulting skipped gates
//! to pass) rather than panicking or stalling the turn loop.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_methodology::quality_evaluate;

/// Scans `~/.claude/skills/` and `<workspace>/.smedja/skills/` for `*.md`
/// files and returns skill names derived by stripping the extension and
/// prepending `/`.  Absent directories are skipped silently.
#[must_use]
pub fn discover_session_skills(workspace_root: &Path) -> Vec<String> {
    let mut skills = Vec::new();

    let candidates = [
        dirs_home().map(|h| h.join(".claude").join("skills")),
        Some(workspace_root.join(".smedja").join("skills")),
    ];

    for maybe_dir in candidates.into_iter().flatten() {
        let Ok(entries) = std::fs::read_dir(&maybe_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    skills.push(format!("/{stem}"));
                }
            }
        }
    }

    skills
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Reads the file-size threshold from `.smedja/quality.toml`, falling back to
/// 600 on any error.
#[must_use]
pub fn load_file_size_threshold(workspace_root: &Path) -> usize {
    let path = workspace_root.join(".smedja").join("quality.toml");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return 600;
    };
    if let Some(t) = parse_quality_toml(&content) {
        t
    } else {
        tracing::warn!(path = %path.display(), "invalid .smedja/quality.toml; using default threshold 600");
        600
    }
}

fn parse_quality_toml(content: &str) -> Option<usize> {
    // Minimal TOML parse: look for `file_size_threshold = <N>`.
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("file_size_threshold") {
            if let Some(rest) = rest.trim_start().strip_prefix('=') {
                if let Ok(n) = rest.trim().parse::<usize>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Runs `git diff HEAD~1` in `workspace_root` and returns the unified diff text.
/// Returns an empty string if git is not available or the working tree has no
/// prior commit.
pub fn git_diff(workspace_root: &Path) -> String {
    let Ok(out) = Command::new("git")
        .args(["diff", "HEAD~1"])
        .current_dir(workspace_root)
        .output()
    else {
        return String::new();
    };
    if !out.status.success() {
        return String::new();
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Returns `(path, line_count)` pairs for files changed since `HEAD~1`.
/// Public alias used by the `quality.review` handler.
pub fn changed_file_sizes_for_review(workspace_root: &Path) -> Vec<(PathBuf, usize)> {
    changed_file_sizes(workspace_root)
}

fn changed_file_sizes(workspace_root: &Path) -> Vec<(PathBuf, usize)> {
    let Ok(out) = Command::new("git")
        .args(["diff", "--name-only", "HEAD~1"])
        .current_dir(workspace_root)
        .output()
    else {
        return vec![];
    };
    if !out.status.success() {
        return vec![];
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|rel| {
            let abs = workspace_root.join(rel);
            let content = std::fs::read_to_string(&abs).ok()?;
            Some((abs, content.lines().count()))
        })
        .collect()
}

/// Runs all four Tier-1 gates and dispatches [`TurnEvent::QualitySnapshot`].
///
/// This function is intended to be called from a spawned Tokio task.  Any
/// error is logged and suppressed — it must never propagate to the turn loop.
pub fn run_after_turn(
    turn_id: Option<String>,
    workspace_root: PathBuf,
    session_skills: Vec<String>,
    file_size_threshold: usize,
    dispatcher: Arc<Dispatcher>,
) {
    let diff = git_diff(&workspace_root);
    let changed_files = changed_file_sizes(&workspace_root);

    let score = quality_evaluate(
        &diff,
        &changed_files,
        &session_skills,
        Some(file_size_threshold),
    );

    let file_advisories: Vec<String> =
        smedja_methodology::file_size::check(&changed_files, file_size_threshold)
            .iter()
            .map(smedja_methodology::FileSizeAdvisory::summary)
            .collect();

    let skill_advisories: Vec<String> =
        smedja_methodology::skill_inject::check(&diff, &session_skills)
            .iter()
            .map(smedja_methodology::SkillAdvisory::summary)
            .collect();

    let event = TurnEvent::QualitySnapshot {
        score: score.score,
        tdd_pass: score.tdd_pass,
        clean_pass: score.clean_pass,
        file_advisories,
        skill_advisories,
        llm_reviewed: false,
        turn_id,
        correlation: CorrelationCtx::default(),
    };

    dispatcher.publish(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quality_toml_returns_threshold() {
        let toml = "[quality]\nfile_size_threshold = 800\n";
        assert_eq!(parse_quality_toml(toml), Some(800));
    }

    #[test]
    fn parse_quality_toml_ignores_comment_lines() {
        let toml = "# this is a comment\nfile_size_threshold = 500\n";
        assert_eq!(parse_quality_toml(toml), Some(500));
    }

    #[test]
    fn parse_quality_toml_returns_none_for_missing_key() {
        assert_eq!(parse_quality_toml("[quality]\n"), None);
    }

    #[test]
    fn parse_quality_toml_returns_none_for_bad_value() {
        assert_eq!(parse_quality_toml("file_size_threshold = abc\n"), None);
    }

    #[test]
    fn load_file_size_threshold_defaults_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_file_size_threshold(dir.path()), 600);
    }

    #[test]
    fn load_file_size_threshold_reads_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("quality.toml"), "file_size_threshold = 1200\n").unwrap();
        assert_eq!(load_file_size_threshold(dir.path()), 1200);
    }

    #[test]
    fn load_file_size_threshold_falls_back_on_bad_toml() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("quality.toml"), "not_the_key = 999\n").unwrap();
        assert_eq!(load_file_size_threshold(dir.path()), 600);
    }

    #[test]
    fn discover_session_skills_skips_absent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // Neither ~/.claude/skills nor .smedja/skills exists under tmp.
        // Must not panic; returns empty.
        let skills = discover_session_skills(dir.path());
        // We can't assert empty because the real ~/.claude/skills may exist,
        // so just assert it doesn't panic and returns a Vec.
        let _ = skills;
    }

    #[test]
    fn discover_session_skills_reads_smedja_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".smedja").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("rust.md"), "# rust skill").unwrap();
        std::fs::write(skills_dir.join("tdd-workflow.md"), "# tdd").unwrap();
        // Non-.md file should be ignored.
        std::fs::write(skills_dir.join("ignore.txt"), "not a skill").unwrap();

        let skills = discover_session_skills(dir.path());
        assert!(skills.contains(&"/rust".to_string()));
        assert!(skills.contains(&"/tdd-workflow".to_string()));
        assert!(!skills.contains(&"/ignore".to_string()));
    }
}
