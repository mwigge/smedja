//! Failure mining — persists loop failure patterns as Markdown guides.
//!
//! After a `LoopFailed` event, the engine calls [`write_failure_guide`] to
//! store role-scoped failure patterns under `.smedja/guides/<role>.md`.
//! These files are read by agents on subsequent attempts to avoid repeating
//! the same mistakes.

use std::path::Path;

use anyhow::Result;

/// Writes failure patterns for `role` to `.smedja/guides/<role>.md` inside
/// `workspace`.
///
/// Creates the guides directory if it does not yet exist.  An empty `patterns`
/// slice writes a guide with an empty patterns section rather than failing.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or the file cannot be
/// written.
pub fn write_failure_guide(role: &str, patterns: &[String], workspace: &Path) -> Result<()> {
    let guides_dir = workspace.join(".smedja").join("guides");
    std::fs::create_dir_all(&guides_dir)?;

    let path = guides_dir.join(format!("{role}.md"));
    let pattern_lines = if patterns.is_empty() {
        String::new()
    } else {
        patterns
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        "# Failure guide for `{role}`\n\nLast updated by loop engine after failure.\n\n## Patterns\n\n{pattern_lines}\n"
    );

    std::fs::write(path, content)?;
    Ok(())
}

/// Reads the failure guide for `role` from `.smedja/guides/<role>.md` inside
/// `workspace`, returning the raw Markdown content.
///
/// Returns `Ok(None)` when no guide exists for the role yet.
///
/// # Errors
///
/// Returns an error only on unexpected I/O failures (not on file-not-found).
pub fn read_failure_guide(role: &str, workspace: &Path) -> Result<Option<String>> {
    let path = workspace
        .join(".smedja")
        .join("guides")
        .join(format!("{role}.md"));

    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_failure_guide_creates_file() {
        let dir = TempDir::new().unwrap();
        let patterns = vec!["forgot to run tests".to_owned(), "wrong branch".to_owned()];

        write_failure_guide("implementer", &patterns, dir.path()).unwrap();

        let path = dir
            .path()
            .join(".smedja")
            .join("guides")
            .join("implementer.md");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Failure guide for `implementer`"));
        assert!(content.contains("- forgot to run tests"));
        assert!(content.contains("- wrong branch"));
    }

    #[test]
    fn write_failure_guide_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        // .smedja/guides should not exist yet.
        assert!(!dir.path().join(".smedja").exists());
        write_failure_guide("reviewer", &[], dir.path()).unwrap();
        assert!(dir
            .path()
            .join(".smedja")
            .join("guides")
            .join("reviewer.md")
            .exists());
    }

    #[test]
    fn write_failure_guide_with_empty_patterns_succeeds() {
        let dir = TempDir::new().unwrap();
        write_failure_guide("tester", &[], dir.path()).unwrap();
        let path = dir.path().join(".smedja").join("guides").join("tester.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Patterns"));
    }

    #[test]
    fn read_failure_guide_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let result = read_failure_guide("nonexistent", dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_failure_guide_returns_written_content() {
        let dir = TempDir::new().unwrap();
        let patterns = vec!["pattern one".to_owned()];
        write_failure_guide("fix", &patterns, dir.path()).unwrap();

        let content = read_failure_guide("fix", dir.path()).unwrap();
        assert!(content.is_some());
        assert!(content.unwrap().contains("pattern one"));
    }

    #[test]
    fn write_failure_guide_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let p1 = vec!["old pattern".to_owned()];
        let p2 = vec!["new pattern".to_owned()];

        write_failure_guide("proposer", &p1, dir.path()).unwrap();
        write_failure_guide("proposer", &p2, dir.path()).unwrap();

        let content = read_failure_guide("proposer", dir.path()).unwrap().unwrap();
        assert!(content.contains("new pattern"));
        assert!(!content.contains("old pattern"));
    }
}
