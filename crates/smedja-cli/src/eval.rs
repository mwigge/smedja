use super::*;

pub(crate) fn cmd_eval_run(
    suite_dir: &std::path::Path,
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
