//! `smedja-review` — one quality bar for any language.
//!
//! The crate adds the deterministic, polyglot half of smedja's review pipeline
//! that the LLM auditor cannot measure by inspection:
//!
//! 1. [`detect`] — infer the language(s) touched from a set of changed paths.
//! 2. [`deterministic`] — per language, run the canonical format / import-sort /
//!    lint tools on the *changed files only*, each behind an availability check
//!    (skip-not-fail when the tool is absent), normalising every tool's output
//!    into the SARIF-shaped [`Finding`] and emitting a SARIF log.
//! 3. [`bar`] — grade the change against seven uniform dimensions
//!    (tdd/coverage, format, import-sort, solid, clean, size, maintainable) with
//!    thresholds from `.smedja/quality.toml`, yielding a composite A–F grade and
//!    a pass/fail on the changed code.
//!
//! The LLM stage (SOLID / clean naming / correctness / maintainability) lives in
//! the daemon, which feeds this crate's Stage-1 findings into the auditor. This
//! crate owns everything deterministic so every runner grades against the same
//! bar.

pub mod bar;
pub mod detect;
pub mod deterministic;

use serde::{Deserialize, Serialize};

pub use bar::{grade, Dimension, QualityBar, Thresholds};
pub use detect::{languages_from_paths, Language};
pub use deterministic::{run_deterministic, to_sarif, DeterministicOutcome};

/// Severity of a [`Finding`], SARIF-aligned and ordered most-to-least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A critical defect.
    Critical,
    /// A high-impact defect.
    High,
    /// A medium-impact defect.
    Medium,
    /// A low-impact defect or smell.
    Low,
    /// Informational note.
    Info,
}

impl Severity {
    /// The lowercase wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }

    /// The SARIF `level` string for this severity.
    #[must_use]
    pub fn sarif_level(self) -> &'static str {
        match self {
            Self::Critical | Self::High => "error",
            Self::Medium | Self::Low => "warning",
            Self::Info => "note",
        }
    }
}

/// A single normalised review finding.
///
/// Field-compatible with the daemon's `AuditFinding` (severity, file, line,
/// rule, rationale) so deterministic-tool findings and LLM findings merge into
/// one list, plus the [`Dimension`] the finding scores against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Severity of the finding.
    pub severity: Severity,
    /// Workspace-relative file the finding concerns.
    pub file: String,
    /// Optional 1-based line number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Short rule slug (e.g. `rustfmt`, `ruff-I`, `clippy`).
    pub rule: String,
    /// One-sentence rationale.
    pub rationale: String,
    /// The quality dimension this finding scores against.
    pub dimension: Dimension,
}

impl Finding {
    /// Constructs a finding.
    #[must_use]
    pub fn new(
        severity: Severity,
        dimension: Dimension,
        file: impl Into<String>,
        line: Option<u32>,
        rule: impl Into<String>,
        rationale: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            file: file.into(),
            line,
            rule: rule.into(),
            rationale: rationale.into(),
            dimension,
        }
    }
}
