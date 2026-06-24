//! The eval report and its threshold gate.
//!
//! [`EvalReport`] carries a [`CaseVerdict`] for every case (or a skip), the
//! aggregate pass rate, and a threshold check. It serialises to stable JSON for
//! the machine-readable summary the runners emit.

use serde::Serialize;

use crate::scoring::Verdict;

/// Whether a case passed, failed, or was skipped (e.g. in offline mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseStatus {
    /// The case passed.
    Pass,
    /// The case failed, carrying a short reason.
    Fail(String),
    /// The case was skipped, carrying a short reason.
    Skip(String),
}

impl CaseStatus {
    /// Builds a [`CaseStatus`] from a scoring [`Verdict`].
    #[must_use]
    pub fn from_verdict(verdict: &Verdict) -> Self {
        match verdict {
            Verdict::Pass => Self::Pass,
            Verdict::Fail(reason) => Self::Fail(reason.clone()),
        }
    }
}

/// The per-case entry in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaseVerdict {
    /// The case identifier.
    pub id: String,
    /// The case outcome.
    pub status: CaseStatus,
}

impl CaseVerdict {
    /// Returns `true` only when the case passed (skips and fails are not passes).
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self.status, CaseStatus::Pass)
    }

    /// Returns `true` when the case was skipped.
    #[must_use]
    pub fn is_skip(&self) -> bool {
        matches!(self.status, CaseStatus::Skip(_))
    }
}

/// The aggregate report for a suite run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalReport {
    /// The suite name.
    pub suite: String,
    /// The configured pass-rate threshold.
    pub threshold: f64,
    /// The per-case verdicts in case order.
    pub verdicts: Vec<CaseVerdict>,
}

impl EvalReport {
    /// Creates a report for `suite` with the given `threshold` and `verdicts`.
    #[must_use]
    pub fn new(suite: impl Into<String>, threshold: f64, verdicts: Vec<CaseVerdict>) -> Self {
        Self {
            suite: suite.into(),
            threshold,
            verdicts,
        }
    }

    /// The total number of cases (including skips).
    #[must_use]
    pub fn total(&self) -> usize {
        self.verdicts.len()
    }

    /// The number of skipped cases.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.verdicts.iter().filter(|v| v.is_skip()).count()
    }

    /// The number of cases that were scored (total minus skipped).
    #[must_use]
    pub fn scored(&self) -> usize {
        self.total() - self.skipped()
    }

    /// The number of cases that passed.
    #[must_use]
    pub fn passed(&self) -> usize {
        self.verdicts.iter().filter(|v| v.is_pass()).count()
    }

    /// The pass rate over scored cases, in `[0.0, 1.0]`.
    ///
    /// Skipped cases are excluded from the denominator. A suite with no scored
    /// cases is treated as a full pass rate (`1.0`) so an all-skipped offline
    /// run does not trip the threshold gate.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        let scored = self.scored();
        if scored == 0 {
            return 1.0;
        }
        let passed = u32::try_from(self.passed()).unwrap_or(u32::MAX);
        let scored = u32::try_from(scored).unwrap_or(u32::MAX);
        f64::from(passed) / f64::from(scored)
    }

    /// Returns `true` when the pass rate meets or exceeds `threshold`.
    #[must_use]
    pub fn meets_threshold(&self, threshold: f64) -> bool {
        self.pass_rate() >= threshold
    }

    /// Serialises the report to a stable JSON object.
    ///
    /// # Errors
    ///
    /// Returns the underlying `serde_json` error if serialisation fails.
    #[must_use = "the JSON summary must be written or returned"]
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let value = serde_json::json!({
            "suite": self.suite,
            "threshold": self.threshold,
            "total": self.total(),
            "scored": self.scored(),
            "skipped": self.skipped(),
            "passed": self.passed(),
            "pass_rate": self.pass_rate(),
            "meets_threshold": self.meets_threshold(self.threshold),
            "verdicts": self.verdicts,
        });
        serde_json::to_string_pretty(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(id: &str) -> CaseVerdict {
        CaseVerdict {
            id: id.to_owned(),
            status: CaseStatus::Pass,
        }
    }

    fn fail(id: &str) -> CaseVerdict {
        CaseVerdict {
            id: id.to_owned(),
            status: CaseStatus::Fail("mismatch".to_owned()),
        }
    }

    fn skip(id: &str) -> CaseVerdict {
        CaseVerdict {
            id: id.to_owned(),
            status: CaseStatus::Skip("offline".to_owned()),
        }
    }

    #[test]
    fn records_per_case_verdicts() {
        let report = EvalReport::new("routing", 1.0, vec![pass("a"), fail("b")]);
        assert_eq!(report.total(), 2);
        assert_eq!(report.passed(), 1);
        assert_eq!(report.verdicts[0].id, "a");
    }

    #[test]
    fn pass_rate_is_passed_over_scored() {
        let report = EvalReport::new("routing", 0.5, vec![pass("a"), fail("b"), pass("c")]);
        assert!((report.pass_rate() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn skips_excluded_from_pass_rate_denominator() {
        let report = EvalReport::new("agent", 1.0, vec![pass("a"), skip("b")]);
        assert_eq!(report.scored(), 1);
        assert_eq!(report.skipped(), 1);
        assert!((report.pass_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn meets_threshold_true_and_false() {
        let met = EvalReport::new("r", 0.5, vec![pass("a"), pass("b")]);
        assert!(met.meets_threshold(0.5));
        let unmet = EvalReport::new("r", 1.0, vec![pass("a"), fail("b")]);
        assert!(!unmet.meets_threshold(1.0));
    }

    #[test]
    fn to_json_is_stable_and_complete() {
        let report = EvalReport::new("routing", 1.0, vec![pass("a")]);
        let json = report.to_json().expect("serialise report");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(value["suite"], "routing");
        assert_eq!(value["total"], 1);
        assert_eq!(value["passed"], 1);
        assert_eq!(value["meets_threshold"], true);
        assert_eq!(value["verdicts"][0]["id"], "a");
        assert_eq!(value["verdicts"][0]["status"], "pass");
    }

    #[test]
    fn empty_scored_suite_passes() {
        let report = EvalReport::new("agent", 1.0, vec![skip("a")]);
        assert!((report.pass_rate() - 1.0).abs() < f64::EPSILON);
        assert!(report.meets_threshold(1.0));
    }
}
