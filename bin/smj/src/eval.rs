//! `smj eval` — run an eval case suite and gate on its pass-rate threshold.

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum EvalCmd {
    /// Load a suite directory, run it, print a report, and gate on the threshold
    Run {
        /// Path to the suite directory (contains `suite.toml` and case files)
        #[arg(long)]
        suite: std::path::PathBuf,
        /// Run graded (rubric / live-driver) cases instead of skipping them
        #[arg(long)]
        online: bool,
        /// Write the machine-readable JSON summary to stdout
        #[arg(long)]
        json: bool,
        /// Override the suite's configured pass-rate threshold (in [0.0, 1.0])
        #[arg(long)]
        threshold: Option<f64>,
    },
}

/// Dispatches a `smj eval` subcommand.
pub(crate) fn run(action: EvalCmd) -> Result<()> {
    match action {
        EvalCmd::Run {
            suite,
            online,
            json,
            threshold,
        } => cmd_eval_run(&suite, online, json, threshold),
    }
}

/// Loads a suite, runs it through the eval engine, prints a human report
/// (and the JSON summary when `--json` is set), emits `OTel` metrics, and exits
/// non-zero when the pass rate is below the (possibly overridden) threshold.
///
/// Routing cases are scored deterministically against `Assayer::default_rules`.
/// Agent cases are graded only when `--online` is set; the real loop driver and
/// LLM judge are wired at the daemon boundary, so this operator path supplies
/// an unwired driver/judge that surfaces a clear failure if an online agent
/// case is run without that wiring.
fn cmd_eval_run(
    suite_dir: &Path,
    online: bool,
    json: bool,
    threshold_override: Option<f64>,
) -> Result<()> {
    use smedja_eval::case::load_suite;
    use smedja_eval::engine::{run_suite_with, LoopDriver};
    use smedja_eval::scoring::{Judge, Outcome, Verdict};
    use smedja_eval::telemetry::{record_eval_duration, record_eval_metrics};

    /// An unwired loop driver: the real driver lives at the daemon boundary.
    struct UnwiredDriver;
    impl LoopDriver for UnwiredDriver {
        fn drive(&self, _scenario: &str) -> Outcome {
            Outcome::Loop {
                final_state: "failed".to_owned(),
                slices_completed: 0,
            }
        }
    }

    /// An unwired judge: the real LLM judge lives at the daemon boundary.
    struct UnwiredJudge;
    impl Judge for UnwiredJudge {
        fn judge(&self, _rubric: &str, _output: &str) -> Verdict {
            Verdict::Fail("no LLM judge wired in smj eval (run via the daemon)".to_owned())
        }
    }

    let suite = load_suite(suite_dir)
        .with_context(|| format!("failed to load suite at {}", suite_dir.display()))?;
    let threshold = threshold_override.unwrap_or(suite.config.pass_threshold);

    let router = smedja_assayer::Assayer::default_rules();
    let started = std::time::Instant::now();
    // `--online` runs graded cases; without it the offline switch skips rubric
    // and live-driver cases. The offline driver above is not live, so
    // deterministic agent cases still run offline.
    let report = run_suite_with(&suite, &router, &UnwiredDriver, &UnwiredJudge, !online);
    let elapsed = started.elapsed().as_secs_f64();

    let passed = u64::try_from(report.passed()).unwrap_or(u64::MAX);
    let total = u64::try_from(report.total()).unwrap_or(u64::MAX);
    record_eval_metrics(&report.suite, total, passed);
    record_eval_duration(&report.suite, elapsed);

    println!("Suite: {}", report.suite);
    for verdict in &report.verdicts {
        let label = match &verdict.status {
            smedja_eval::report::CaseStatus::Pass => "PASS".to_owned(),
            smedja_eval::report::CaseStatus::Fail(reason) => format!("FAIL ({reason})"),
            smedja_eval::report::CaseStatus::Skip(reason) => format!("SKIP ({reason})"),
        };
        println!("  {:<30} {label}", verdict.id);
    }
    println!(
        "{} / {} passed ({} scored, {} skipped) — pass_rate {:.3}, threshold {:.3}",
        report.passed(),
        report.total(),
        report.scored(),
        report.skipped(),
        report.pass_rate(),
        threshold,
    );

    if json {
        println!("{}", report.to_json().context("serialise eval report")?);
    }

    if report.meets_threshold(threshold) {
        Ok(())
    } else {
        std::process::exit(1);
    }
}
