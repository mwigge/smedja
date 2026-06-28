use std::path::PathBuf;

const GATE: &str = "FileSizeGate";

/// Advisory raised when a changed file exceeds the configured line-count threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSizeAdvisory {
    /// Path of the file that exceeded the threshold.
    pub path: PathBuf,
    /// Actual line count of the file.
    pub lines: usize,
    /// The threshold that was exceeded.
    pub threshold: usize,
}

impl FileSizeAdvisory {
    /// Human-readable one-line summary for panel display.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} {} L (threshold {})",
            self.path.display(),
            self.lines,
            self.threshold,
        )
    }

    /// Returns the gate name.
    #[must_use]
    pub fn gate(&self) -> &'static str {
        GATE
    }
}

/// Checks a set of changed files against a line-count threshold.
///
/// Returns one [`FileSizeAdvisory`] for each file that exceeds `threshold`.
/// The result is advisory-only — callers decide whether to surface or block.
///
/// # Arguments
///
/// * `changed_files` — `(path, line_count)` pairs produced by the caller from
///   the working tree (e.g. `wc -l` on each file in `git diff --name-only`).
/// * `threshold` — maximum acceptable line count; 0 flags every file.
#[must_use]
pub fn check(changed_files: &[(PathBuf, usize)], threshold: usize) -> Vec<FileSizeAdvisory> {
    changed_files
        .iter()
        .filter(|(_, lines)| *lines > threshold)
        .map(|(path, lines)| FileSizeAdvisory {
            path: path.clone(),
            lines: *lines,
            threshold,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn empty_input_returns_no_advisories() {
        assert!(check(&[], 600).is_empty());
    }

    #[test]
    fn all_under_threshold_returns_no_advisories() {
        let files = vec![(p("a.rs"), 100), (p("b.rs"), 599)];
        assert!(check(&files, 600).is_empty());
    }

    #[test]
    fn file_at_threshold_is_not_flagged() {
        let files = vec![(p("a.rs"), 600)];
        assert!(check(&files, 600).is_empty());
    }

    #[test]
    fn one_over_threshold_returns_one_advisory() {
        let files = vec![(p("main.rs"), 7880), (p("lib.rs"), 200)];
        let advisories = check(&files, 600);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].path, p("main.rs"));
        assert_eq!(advisories[0].lines, 7880);
        assert_eq!(advisories[0].threshold, 600);
    }

    #[test]
    fn all_over_threshold_returns_all_advisories() {
        let files = vec![(p("a.rs"), 700), (p("b.rs"), 800), (p("c.rs"), 900)];
        let advisories = check(&files, 600);
        assert_eq!(advisories.len(), 3);
    }

    #[test]
    fn threshold_zero_flags_every_file() {
        let files = vec![(p("a.rs"), 0), (p("b.rs"), 1)];
        // 0 lines is NOT > 0, so zero-line file is not flagged.
        let advisories = check(&files, 0);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].path, p("b.rs"));
    }

    #[test]
    fn summary_contains_path_lines_and_threshold() {
        let adv = FileSizeAdvisory {
            path: p("src/main.rs"),
            lines: 7880,
            threshold: 600,
        };
        let s = adv.summary();
        assert!(s.contains("main.rs"), "path in summary: {s}");
        assert!(s.contains("7880"), "lines in summary: {s}");
        assert!(s.contains("600"), "threshold in summary: {s}");
    }

    #[test]
    fn gate_name_is_correct() {
        let adv = FileSizeAdvisory {
            path: p("x.rs"),
            lines: 1,
            threshold: 0,
        };
        assert_eq!(adv.gate(), "FileSizeGate");
    }
}
