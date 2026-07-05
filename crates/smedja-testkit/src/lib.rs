//! `smedja-testkit` — runner-agnostic test discovery, execution, and parsing.
//!
//! The crate turns "run the tests" into three pure-ish stages that every smedja
//! runner shares:
//!
//! 1. [`detect`] — recursively scan a workspace for test suites across languages
//!    (Cargo, npm `scripts.test`, pytest, Go, Maven, Gradle, .NET) and delegate
//!    to a monorepo meta-runner (nx, turbo, moon, just, task) when one is present.
//! 2. [`run`] — execute each detected suite requesting a machine-readable format
//!    (per framework) with a lenient JUnit-XML fallback.
//! 3. [`parse`] — normalise every framework's output into a single
//!    [`TestReport`].
//!
//! The report shape is deliberately uniform so the agent tool surface, the TUI
//! `/test` command, and the loop's fix guide can all consume one structure
//! rather than each re-implementing a naive substring counter.

pub mod detect;
pub mod parse;
pub mod run;

use serde::{Deserialize, Serialize};

pub use detect::{detect_suites, Runner, Suite};
pub use parse::Parsed;
pub use run::{run_all, run_suite, Scope};

/// A normalised, cross-framework test report: one entry per detected suite.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestReport {
    /// Per-suite results, in detection order.
    pub suites: Vec<SuiteReport>,
}

impl TestReport {
    /// Total passed tests across all suites.
    #[must_use]
    pub fn total_passed(&self) -> u32 {
        self.suites.iter().map(|s| s.passed).sum()
    }

    /// Total failed tests across all suites.
    #[must_use]
    pub fn total_failed(&self) -> u32 {
        self.suites.iter().map(|s| s.failed).sum()
    }

    /// Total skipped tests across all suites.
    #[must_use]
    pub fn total_skipped(&self) -> u32 {
        self.suites.iter().map(|s| s.skipped).sum()
    }

    /// `true` when no suite reported a failure. An empty report is vacuously
    /// green — callers that require at least one suite should check `suites`.
    #[must_use]
    pub fn green(&self) -> bool {
        self.total_failed() == 0
    }

    /// A one-line human summary: `"N passed, M failed, K skipped"`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} passed, {} failed, {} skipped",
            self.total_passed(),
            self.total_failed(),
            self.total_skipped()
        )
    }
}

/// The result of running a single suite, normalised across frameworks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuiteReport {
    /// The runner that produced this result (`cargo`, `npm`, `pytest`, …).
    pub runner: String,
    /// Workspace-relative directory the suite was rooted at.
    pub dir: String,
    /// Count of passing tests.
    pub passed: u32,
    /// Count of failing tests.
    pub failed: u32,
    /// Count of skipped/ignored tests.
    pub skipped: u32,
    /// Wall-clock duration of the suite run, in milliseconds.
    pub duration_ms: u64,
    /// The individual failures, with their names and messages.
    pub failures: Vec<Failure>,
    /// Optional diagnostic note (e.g. "runner not installed; skipped").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A single failing test.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// The test name / identifier as reported by the framework.
    pub name: String,
    /// A short failure message (assertion text, panic line, …), possibly empty.
    pub message: String,
}
