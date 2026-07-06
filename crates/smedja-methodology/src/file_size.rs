use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const GATE: &str = "FileSizeGate";

/// Default line-count threshold when a workspace ships no `quality.toml`.
pub const DEFAULT_THRESHOLD: usize = 600;

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

/// Error parsing a [`Baseline`] from TOML text.
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    /// The baseline document could not be parsed as TOML.
    #[error("invalid file-size baseline: {0}")]
    Parse(String),
}

/// Normalises a path into the repo-relative, forward-slash key used in the
/// baseline document.
fn rel_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A checked-in record of files that already exceed the size threshold, each
/// grandfathered at its recorded line count.
///
/// This encodes the "clean-as-you-code" contract: existing oversized files are
/// tolerated at their current size (the ceiling), but any growth past that
/// ceiling — and any *new* file crossing the threshold — is a violation. The
/// ceiling only ever ratchets down: as a file is split, regenerating the
/// baseline lowers or removes its entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    threshold: usize,
    files: BTreeMap<String, usize>,
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            files: BTreeMap::new(),
        }
    }
}

/// Serde shape of the on-disk baseline document.
#[derive(Debug, Deserialize, Serialize)]
struct BaselineDoc {
    #[serde(default = "default_threshold")]
    threshold: usize,
    #[serde(default)]
    files: BTreeMap<String, usize>,
}

fn default_threshold() -> usize {
    DEFAULT_THRESHOLD
}

const BASELINE_HEADER: &str = "\
# File-size gate baseline — grandfathered oversized files.
#
# GENERATED FILE. Do not edit by hand. Regenerate with:
#     scripts/file-size-gate.sh --regenerate
#
# The gate ALLOWS each listed file at or below its recorded line count and
# FAILS if it grows past it. Any file NOT listed here that crosses `threshold`
# also FAILS. The baseline is a ceiling that only ratchets DOWN.
";

impl Baseline {
    /// Parses a [`Baseline`] from the contents of `file-size-baseline.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`BaselineError::Parse`] when the text is not valid TOML.
    pub fn from_toml_str(s: &str) -> Result<Self, BaselineError> {
        let doc: BaselineDoc =
            toml::from_str(s).map_err(|e| BaselineError::Parse(e.to_string()))?;
        Ok(Self {
            threshold: doc.threshold,
            files: doc.files,
        })
    }

    /// The threshold recorded in the baseline document.
    #[must_use]
    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// Number of grandfathered files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the baseline grandfathers no files.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The grandfathered ceiling for `rel_path`, if it is baselined.
    #[must_use]
    pub fn ceiling(&self, rel_path: &str) -> Option<usize> {
        self.files.get(rel_path).copied()
    }

    /// Builds a fresh baseline that grandfathers every file currently over
    /// `threshold` at its current line count.
    ///
    /// This is the ratchet: feeding it the present-day counts records the new,
    /// smaller ceilings (and drops any file that has fallen to or below the
    /// threshold), so the baseline can only ever shrink.
    #[must_use]
    pub fn regenerate(files: &[(PathBuf, usize)], threshold: usize) -> Self {
        let files = files
            .iter()
            .filter(|(_, lines)| *lines > threshold)
            .map(|(path, lines)| (rel_key(path), *lines))
            .collect();
        Self { threshold, files }
    }

    /// Renders the baseline back to TOML for check-in. Keys are emitted in
    /// sorted order (the map is a [`BTreeMap`]) so the output is deterministic.
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        let doc = BaselineDoc {
            threshold: self.threshold,
            files: self.files.clone(),
        };
        let body = toml::to_string(&doc).unwrap_or_default();
        format!("{BASELINE_HEADER}{body}")
    }
}

/// A hard file-size violation raised by the enforcing gate.
///
/// Unlike [`FileSizeAdvisory`], a violation *blocks*: the caller is expected to
/// fail the commit / gate run when any are returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSizeViolation {
    /// Path of the offending file.
    pub path: PathBuf,
    /// Actual line count.
    pub lines: usize,
    /// The configured threshold.
    pub threshold: usize,
    /// The grandfathered ceiling when the file is baselined but grew past it;
    /// `None` when the file is not baselined at all.
    pub allowed: Option<usize>,
}

impl FileSizeViolation {
    /// Human-readable one-line summary explaining why the file blocks.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.allowed {
            Some(ceiling) => format!(
                "{}: {} L grew past baseline ceiling {} (threshold {})",
                self.path.display(),
                self.lines,
                ceiling,
                self.threshold,
            ),
            None => format!(
                "{}: {} L exceeds threshold {} and is not baselined",
                self.path.display(),
                self.lines,
                self.threshold,
            ),
        }
    }

    /// Returns the gate name.
    #[must_use]
    pub fn gate(&self) -> &'static str {
        GATE
    }
}

/// The enforcing, ratcheting file-size gate.
///
/// For each changed `(path, line_count)`:
///
/// * at or below `threshold` → allowed;
/// * over `threshold` and baselined at `ceiling`, with `lines <= ceiling` →
///   allowed (grandfathered, existing size tolerated);
/// * over `threshold` and baselined but `lines > ceiling` → **violation**
///   (grew past its ceiling);
/// * over `threshold` and *not* baselined → **violation** (new oversized file).
///
/// Paths must be repo-relative (matching the baseline keys); the caller is
/// responsible for stripping any workspace-root prefix first.
#[must_use]
pub fn enforce(
    changed_files: &[(PathBuf, usize)],
    threshold: usize,
    baseline: &Baseline,
) -> Vec<FileSizeViolation> {
    changed_files
        .iter()
        .filter(|(_, lines)| *lines > threshold)
        .filter_map(|(path, lines)| {
            let key = rel_key(path);
            match baseline.ceiling(&key) {
                Some(ceiling) if *lines <= ceiling => None,
                Some(ceiling) => Some(FileSizeViolation {
                    path: path.clone(),
                    lines: *lines,
                    threshold,
                    allowed: Some(ceiling),
                }),
                None => Some(FileSizeViolation {
                    path: path.clone(),
                    lines: *lines,
                    threshold,
                    allowed: None,
                }),
            }
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

    // ── Baseline + enforcing gate ─────────────────────────────────────────

    fn baseline_with(entries: &[(&str, usize)]) -> Baseline {
        Baseline::regenerate(
            &entries
                .iter()
                .map(|(path, lines)| (p(path), *lines))
                .collect::<Vec<_>>(),
            600,
        )
    }

    #[test]
    fn baselined_file_at_recorded_count_passes() {
        let baseline = baseline_with(&[("big.rs", 900)]);
        let changed = vec![(p("big.rs"), 900)];
        assert!(
            enforce(&changed, 600, &baseline).is_empty(),
            "a grandfathered file at its ceiling must pass",
        );
    }

    #[test]
    fn baselined_file_below_recorded_count_passes() {
        let baseline = baseline_with(&[("big.rs", 900)]);
        let changed = vec![(p("big.rs"), 850)];
        assert!(enforce(&changed, 600, &baseline).is_empty());
    }

    #[test]
    fn baselined_file_grown_by_one_fails() {
        let baseline = baseline_with(&[("big.rs", 900)]);
        let changed = vec![(p("big.rs"), 901)];
        let violations = enforce(&changed, 600, &baseline);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, p("big.rs"));
        assert_eq!(violations[0].lines, 901);
        assert_eq!(violations[0].allowed, Some(900));
        assert!(violations[0].summary().contains("grew past"));
    }

    #[test]
    fn non_baselined_file_over_threshold_fails() {
        let baseline = baseline_with(&[("big.rs", 900)]);
        let changed = vec![(p("new_big.rs"), 601)];
        let violations = enforce(&changed, 600, &baseline);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, p("new_big.rs"));
        assert_eq!(violations[0].allowed, None);
        assert!(violations[0].summary().contains("not baselined"));
    }

    #[test]
    fn new_small_file_passes() {
        let baseline = baseline_with(&[("big.rs", 900)]);
        let changed = vec![(p("small.rs"), 42), (p("also_small.rs"), 600)];
        assert!(enforce(&changed, 600, &baseline).is_empty());
    }

    #[test]
    fn baseline_regeneration_reflects_a_shrunk_file() {
        // Originally grandfathered at 900.
        let original = baseline_with(&[("big.rs", 900), ("other.rs", 700)]);
        assert_eq!(original.ceiling("big.rs"), Some(900));

        // After splitting, big.rs is now 650; other.rs dropped to 500.
        let now = vec![(p("big.rs"), 650), (p("other.rs"), 500)];
        let regenerated = Baseline::regenerate(&now, 600);

        // Ceiling ratcheted down…
        assert_eq!(regenerated.ceiling("big.rs"), Some(650));
        // …and a file that fell to/under the threshold drops out entirely.
        assert_eq!(regenerated.ceiling("other.rs"), None);
        assert_eq!(regenerated.len(), 1);

        // The new, lower ceiling is now enforced: 651 fails.
        let violations = enforce(&[(p("big.rs"), 651)], 600, &regenerated);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].allowed, Some(650));
    }

    #[test]
    fn baseline_toml_round_trips() {
        let baseline = baseline_with(&[("bin/a.rs", 800), ("crates/b.rs", 1200)]);
        let toml = baseline.to_toml_string();
        assert!(toml.contains("threshold = 600"));
        assert!(toml.contains("\"bin/a.rs\" = 800"));
        let parsed = Baseline::from_toml_str(&toml).unwrap();
        assert_eq!(parsed, baseline);
        assert_eq!(parsed.threshold(), 600);
    }

    #[test]
    fn baseline_regenerate_excludes_files_at_or_under_threshold() {
        let files = vec![(p("over.rs"), 601), (p("at.rs"), 600), (p("under.rs"), 10)];
        let baseline = Baseline::regenerate(&files, 600);
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline.ceiling("over.rs"), Some(601));
        assert_eq!(baseline.ceiling("at.rs"), None);
    }

    #[test]
    fn missing_baseline_defaults_are_sane() {
        let baseline = Baseline::default();
        assert_eq!(baseline.threshold(), DEFAULT_THRESHOLD);
        assert!(baseline.is_empty());
        // With an empty baseline, any file over threshold blocks.
        let violations = enforce(&[(p("x.rs"), 700)], 600, &baseline);
        assert_eq!(violations.len(), 1);
    }
}
