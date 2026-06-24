//! Routing regression gate.
//!
//! Loads the workspace `evals/routing` suite and asserts it passes at 100%
//! against `Assayer::default_rules()`. This runs inside `cargo test`, so a
//! change to the routing table that a labelled case does not expect fails CI
//! immediately. It makes no model call and is fully deterministic.

use std::path::PathBuf;

use smedja_assayer::Assayer;
use smedja_eval::case::load_suite;
use smedja_eval::engine::{run_suite_with, LoopDriver};
use smedja_eval::scoring::{Judge, Outcome, Verdict};

/// A driver that must never be invoked by a routing-only suite.
struct UnusedDriver;

impl LoopDriver for UnusedDriver {
    fn drive(&self, _scenario: &str) -> Outcome {
        panic!("routing suite must not drive the loop");
    }
    fn is_live(&self) -> bool {
        false
    }
}

/// A judge that must never be invoked by a routing-only suite.
struct UnusedJudge;

impl Judge for UnusedJudge {
    fn judge(&self, _rubric: &str, _output: &str) -> Verdict {
        panic!("routing suite must not call the judge");
    }
}

/// Resolves the workspace-relative `evals/routing` directory from the crate's
/// manifest directory (`crates/smedja-eval`).
fn routing_suite_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("evals")
        .join("routing")
}

#[test]
fn routing_suite_passes_at_100_percent_against_default_rules() {
    let suite = load_suite(&routing_suite_dir()).expect("load evals/routing suite");
    assert!(!suite.cases.is_empty(), "routing suite must have cases");

    let router = Assayer::default_rules();
    // The offline flag is irrelevant for routing cases; force it on to prove
    // the suite needs no model access.
    let report = run_suite_with(&suite, &router, &UnusedDriver, &UnusedJudge, true);

    assert_eq!(
        report.passed(),
        report.total(),
        "routing suite must pass 100% against default_rules; report: {}",
        report.to_json().expect("serialise report")
    );
    assert!((report.pass_rate() - 1.0).abs() < f64::EPSILON);
    assert!(report.meets_threshold(suite.config.pass_threshold));
}
