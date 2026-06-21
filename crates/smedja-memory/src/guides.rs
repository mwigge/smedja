//! Failure guide writer for the smedja loop failure-mining pipeline.

use std::io::Write as _;

/// Writes a failure guide for `role` to `guides_dir/<role>.md`.
///
/// Creates `guides_dir` (and any parent directories) if it does not exist.
/// Overwrites any existing file for the role.
///
/// The output format is:
/// ```markdown
/// ## Failure Patterns
///
/// - <pattern 1>
/// - <pattern 2>
/// ```
///
/// # Errors
///
/// Returns an `std::io::Error` if the directory cannot be created or the file
/// cannot be written.
pub fn write_failure_guide(
    role: &str,
    failure_patterns: &[&str],
    guides_dir: &std::path::Path,
) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(guides_dir)?;

    let mut content = String::from("## Failure Patterns\n");
    for pattern in failure_patterns {
        content.push('\n');
        content.push_str("- ");
        content.push_str(pattern);
    }
    content.push('\n');

    let path = guides_dir.join(format!("{role}.md"));
    let mut file = std::fs::File::create(&path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_file_with_expected_content() {
        let tmp = tempfile::tempdir().unwrap();
        let guides_dir = tmp.path().join("guides");
        write_failure_guide("coder", &["missed test", "bad import"], &guides_dir).unwrap();

        let path = guides_dir.join("coder.md");
        assert!(path.exists(), "guide file must be created");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Failure Patterns"));
        assert!(content.contains("- missed test"));
        assert!(content.contains("- bad import"));
    }

    #[test]
    fn overwrites_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let guides_dir = tmp.path().join("guides");

        write_failure_guide("reviewer", &["first pattern"], &guides_dir).unwrap();
        write_failure_guide("reviewer", &["second pattern"], &guides_dir).unwrap();

        let content = std::fs::read_to_string(guides_dir.join("reviewer.md")).unwrap();
        assert!(
            content.contains("second pattern"),
            "file must contain new content"
        );
        assert!(
            !content.contains("first pattern"),
            "file must NOT contain old content after overwrite"
        );
    }

    #[test]
    fn creates_directory_if_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let guides_dir = tmp.path().join("a").join("b").join("guides");
        write_failure_guide("tester", &[], &guides_dir).unwrap();
        assert!(guides_dir.exists());
    }
}
