//! Stage 3 — one uniform quality bar.
//!
//! Every change is graded against the same seven dimensions regardless of
//! language. Deterministic-tool findings (Stage 1) and LLM findings (Stage 2)
//! are each tagged with the [`Dimension`] they score against; [`grade`] tallies
//! them, applies the per-dimension thresholds from `.smedja/quality.toml`, and
//! produces a composite A–F grade plus an overall pass/fail on the changed code.

use serde::{Deserialize, Serialize};

use crate::Finding;

/// The seven uniform quality dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    /// Tests exist for changed code and coverage meets the floor.
    TddCoverage,
    /// Code is formatted to the canonical style.
    Format,
    /// Imports are sorted / grouped canonically.
    ImportSort,
    /// SOLID design (LLM-assessed).
    Solid,
    /// Clean code: naming, no debug residue, error handling (lint + LLM).
    Clean,
    /// Files stay within the size budget.
    Size,
    /// Maintainability / complexity (LLM + complexity tools).
    Maintainable,
}

impl Dimension {
    /// All seven dimensions, in canonical order.
    pub const ALL: [Self; 7] = [
        Self::TddCoverage,
        Self::Format,
        Self::ImportSort,
        Self::Solid,
        Self::Clean,
        Self::Size,
        Self::Maintainable,
    ];

    /// The `snake_case` wire label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TddCoverage => "tdd_coverage",
            Self::Format => "format",
            Self::ImportSort => "import_sort",
            Self::Solid => "solid",
            Self::Clean => "clean",
            Self::Size => "size",
            Self::Maintainable => "maintainable",
        }
    }
}

/// Per-dimension thresholds, loaded from `.smedja/quality.toml`'s `[bar]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    /// Minimum line-coverage fraction (0.0–1.0) for the tdd/coverage dimension.
    pub coverage_min: f64,
    /// When `true`, the tdd/coverage dimension fails if no tests cover changes.
    pub require_tests: bool,
    /// Maximum tolerated findings per dimension before it fails.
    pub max_findings: [u32; 7],
    /// Minimum acceptable composite grade (`A`–`F`) for an overall pass.
    pub min_grade: char,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            coverage_min: 0.0,
            require_tests: true,
            // format / import-sort / size are zero-tolerance; design dimensions
            // allow a small number of advisory findings before failing.
            max_findings: dims([0, 0, 0, 2, 1, 0, 2]),
            min_grade: 'C',
        }
    }
}

/// Builds a per-dimension array in [`Dimension::ALL`] order.
const fn dims(v: [u32; 7]) -> [u32; 7] {
    v
}

impl Thresholds {
    /// Loads thresholds from `<workspace>/.smedja/quality.toml`, falling back to
    /// [`Thresholds::default`] for any missing field or absent file.
    #[must_use]
    pub fn load(workspace: &std::path::Path) -> Self {
        let path = workspace.join(".smedja").join("quality.toml");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
            .map(|v| Self::from_toml(&v))
            .unwrap_or_default()
    }

    /// Parses a `[bar]` table, tolerating missing keys.
    #[must_use]
    pub fn from_toml(v: &toml::Value) -> Self {
        let mut t = Self::default();
        let Some(bar) = v.get("bar") else {
            return t;
        };
        if let Some(c) = bar.get("coverage_min").and_then(toml::Value::as_float) {
            t.coverage_min = c.clamp(0.0, 1.0);
        }
        if let Some(r) = bar.get("require_tests").and_then(toml::Value::as_bool) {
            t.require_tests = r;
        }
        if let Some(g) = bar
            .get("min_grade")
            .and_then(toml::Value::as_str)
            .and_then(|s| s.chars().next())
        {
            t.min_grade = g.to_ascii_uppercase();
        }
        let keys = [
            "max_tdd_coverage",
            "max_format",
            "max_import_sort",
            "max_solid",
            "max_clean",
            "max_size",
            "max_maintainable",
        ];
        for (i, key) in keys.iter().enumerate() {
            if let Some(n) = bar.get(*key).and_then(toml::Value::as_integer) {
                t.max_findings[i] = u32::try_from(n.max(0)).unwrap_or(0);
            }
        }
        t
    }
}

/// One dimension's graded outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DimensionScore {
    /// Which dimension.
    pub dimension: Dimension,
    /// Whether the dimension passed its threshold.
    pub pass: bool,
    /// Number of findings attributed to this dimension.
    pub findings: u32,
    /// Human-readable detail (e.g. coverage percentage).
    pub detail: String,
}

/// The composite quality bar for a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityBar {
    /// Per-dimension outcomes, in [`Dimension::ALL`] order.
    pub dimensions: Vec<DimensionScore>,
    /// Composite letter grade `A`–`F`.
    pub grade: char,
    /// Overall pass/fail — `grade` at least the configured `min_grade`.
    pub pass: bool,
}

impl QualityBar {
    /// The count of passing dimensions.
    #[must_use]
    pub fn passing(&self) -> usize {
        self.dimensions.iter().filter(|d| d.pass).count()
    }
}

/// Grades a change against the seven dimensions.
///
/// * `findings` — every deterministic + LLM finding, each tagged with the
///   dimension it scores against.
/// * `tests_cover_changes` — whether changed code has covering tests
///   (the tdd/coverage dimension's boolean gate).
/// * `coverage` — measured line-coverage fraction, when a coverage tool ran.
#[must_use]
pub fn grade(
    findings: &[Finding],
    tests_cover_changes: bool,
    coverage: Option<f64>,
    thresholds: &Thresholds,
) -> QualityBar {
    let mut scores = Vec::with_capacity(7);
    for (i, dim) in Dimension::ALL.iter().enumerate() {
        let count = u32::try_from(findings.iter().filter(|f| f.dimension == *dim).count())
            .unwrap_or(u32::MAX);
        let max = thresholds.max_findings[i];
        let (pass, detail) = if *dim == Dimension::TddCoverage {
            grade_tdd(count, max, tests_cover_changes, coverage, thresholds)
        } else {
            (count <= max, format!("{count} finding(s), max {max}"))
        };
        scores.push(DimensionScore {
            dimension: *dim,
            pass,
            findings: count,
            detail,
        });
    }

    let passing = scores.iter().filter(|d| d.pass).count();
    let grade = grade_letter(passing, scores.len());
    let pass = grade_rank(grade) <= grade_rank(thresholds.min_grade);
    QualityBar {
        dimensions: scores,
        grade,
        pass,
    }
}

/// Grades the tdd/coverage dimension, combining the covering-tests gate, the
/// coverage floor, and any dimension-tagged findings.
fn grade_tdd(
    count: u32,
    max: u32,
    tests_cover_changes: bool,
    coverage: Option<f64>,
    thresholds: &Thresholds,
) -> (bool, String) {
    let findings_ok = count <= max;
    let tests_ok = !thresholds.require_tests || tests_cover_changes;
    let coverage_ok = coverage.is_none_or(|c| c >= thresholds.coverage_min);
    let pass = findings_ok && tests_ok && coverage_ok;
    let detail = match coverage {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(c) => format!(
            "tests={tests_ok}, coverage={}% (min {}%)",
            (c * 100.0).round() as u32,
            (thresholds.coverage_min * 100.0).round() as u32
        ),
        None => format!("tests={tests_ok}, coverage=n/a"),
    };
    (pass, detail)
}

/// Maps the fraction of passing dimensions to a letter grade.
fn grade_letter(passing: usize, total: usize) -> char {
    if total == 0 {
        return 'A';
    }
    // Ratio in tenths avoids floating point at the boundaries.
    let pct = passing * 100 / total;
    match pct {
        100 => 'A',
        p if p >= 85 => 'B',
        p if p >= 70 => 'C',
        p if p >= 55 => 'D',
        _ => 'F',
    }
}

/// Ranks a grade letter (`A` = 0 best) so thresholds compare by ordering.
fn grade_rank(grade: char) -> u8 {
    match grade.to_ascii_uppercase() {
        'A' => 0,
        'B' => 1,
        'C' => 2,
        'D' => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Severity;

    fn finding(dim: Dimension) -> Finding {
        Finding::new(Severity::Medium, dim, "a.rs", Some(1), "rule", "why")
    }

    #[test]
    fn clean_change_grades_a_and_passes() {
        let bar = grade(&[], true, Some(0.9), &Thresholds::default());
        assert_eq!(bar.grade, 'A');
        assert!(bar.pass);
        assert_eq!(bar.passing(), 7);
    }

    #[test]
    fn format_finding_fails_that_dimension() {
        let bar = grade(
            &[finding(Dimension::Format)],
            true,
            None,
            &Thresholds::default(),
        );
        let fmt = bar
            .dimensions
            .iter()
            .find(|d| d.dimension == Dimension::Format)
            .unwrap();
        assert!(!fmt.pass);
        // 6/7 pass → B.
        assert_eq!(bar.grade, 'B');
        assert!(bar.pass); // B is above the default C floor.
    }

    #[test]
    fn missing_tests_fails_tdd_dimension() {
        let bar = grade(&[], false, None, &Thresholds::default());
        let tdd = bar
            .dimensions
            .iter()
            .find(|d| d.dimension == Dimension::TddCoverage)
            .unwrap();
        assert!(!tdd.pass);
    }

    #[test]
    fn coverage_below_floor_fails_tdd() {
        let t = Thresholds {
            coverage_min: 0.8,
            ..Thresholds::default()
        };
        let bar = grade(&[], true, Some(0.5), &t);
        let tdd = &bar.dimensions[0];
        assert_eq!(tdd.dimension, Dimension::TddCoverage);
        assert!(!tdd.pass);
    }

    #[test]
    fn many_failures_grade_f_and_fail_overall() {
        let findings = vec![
            finding(Dimension::Format),
            finding(Dimension::ImportSort),
            finding(Dimension::Size),
            finding(Dimension::Solid),
            finding(Dimension::Solid),
            finding(Dimension::Solid),
        ];
        let bar = grade(&findings, false, None, &Thresholds::default());
        assert_eq!(bar.grade, 'F');
        assert!(!bar.pass);
    }

    #[test]
    fn thresholds_from_toml_overrides_defaults() {
        let raw = r#"
[bar]
coverage_min = 0.75
require_tests = false
min_grade = "B"
max_solid = 5
"#;
        let v: toml::Value = toml::from_str(raw).unwrap();
        let t = Thresholds::from_toml(&v);
        assert!((t.coverage_min - 0.75).abs() < f64::EPSILON);
        assert!(!t.require_tests);
        assert_eq!(t.min_grade, 'B');
        assert_eq!(t.max_findings[3], 5); // solid is index 3
    }

    #[test]
    fn grade_letter_boundaries() {
        assert_eq!(grade_letter(7, 7), 'A');
        assert_eq!(grade_letter(6, 7), 'B');
        assert_eq!(grade_letter(5, 7), 'C');
        assert_eq!(grade_letter(4, 7), 'D');
        assert_eq!(grade_letter(3, 7), 'F');
    }
}
