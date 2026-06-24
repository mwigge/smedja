//! The run engine.
//!
//! [`run_suite`] takes the side-effecting collaborators behind traits — a
//! [`RouteEvaluator`] (default: a real [`smedja_assayer::Assayer`]), a
//! [`LoopDriver`] (a fake in tests; the real driver is wired at the daemon
//! boundary), and a [`crate::scoring::Judge`] — and returns a pure
//! [`EvalReport`]. Aggregation, repetition handling, the offline switch, and
//! the threshold check all live here and are unit-testable with fakes.

use smedja_assayer::{AgentRole, Assayer, Complexity};

use crate::case::{
    AgentExpectation, CaseKind, EvalCase, Expectation, Input, RoutingExpectation, Suite,
};
use crate::report::{CaseStatus, CaseVerdict, EvalReport};
use crate::scoring::{Deterministic, ExactMatch, Judge, Outcome, Rubric, Scorer, Verdict};

/// The environment variable that forces an offline run.
pub const OFFLINE_ENV: &str = "SMEDJA_EVAL_OFFLINE";

/// Evaluates a routing input into a destination outcome.
///
/// The default implementation is backed by a real [`Assayer`]; a wrong-rules
/// assayer can be injected in tests to drive a failing verdict.
pub trait RouteEvaluator {
    /// Routes `role` × `complexity` to a destination [`Outcome`].
    fn evaluate(&self, role: AgentRole, complexity: Complexity) -> Outcome;
}

impl RouteEvaluator for Assayer {
    fn evaluate(&self, role: AgentRole, complexity: Complexity) -> Outcome {
        let decision = self.route_decision(role, complexity);
        Outcome::Routing {
            runner: decision.runner(),
            tier: decision.tier(),
        }
    }
}

/// Drives an agent case to a captured outcome.
///
/// The real driver wraps `smedja_loop::engine::drive` at the daemon boundary;
/// tests supply a fake. [`LoopDriver::is_live`] reports whether the driver
/// needs network/model access so the offline switch can skip live cases while
/// still running deterministic cases against an offline driver.
pub trait LoopDriver {
    /// Drives `scenario` and returns the captured [`Outcome`].
    fn drive(&self, scenario: &str) -> Outcome;

    /// Returns `true` when the driver requires live network/model access.
    ///
    /// Live drivers are skipped under [`OFFLINE_ENV`].
    fn is_live(&self) -> bool {
        true
    }
}

/// Returns `true` when the offline switch is set in the environment.
#[must_use]
pub fn offline_from_env() -> bool {
    std::env::var(OFFLINE_ENV).as_deref() == Ok("1")
}

/// Aggregates a single case's `repetitions` runs into a [`Verdict`] using
/// k-of-n: the case passes when at least `pass_threshold_k` runs pass.
fn aggregate_runs(verdicts: &[Verdict], pass_threshold_k: u32) -> Verdict {
    let passes = u32::try_from(verdicts.iter().filter(|v| v.is_pass()).count()).unwrap_or(u32::MAX);
    if passes >= pass_threshold_k {
        Verdict::Pass
    } else {
        Verdict::Fail(format!(
            "passed {passes} of {} run(s); need {pass_threshold_k}",
            verdicts.len()
        ))
    }
}

/// Scores a routing case (a single authoritative run).
fn score_routing<R: RouteEvaluator>(
    case: &EvalCase,
    router: &R,
    expectation: &RoutingExpectation,
) -> Verdict {
    let Input::Routing { role, complexity } = &case.input else {
        return Verdict::Fail("routing case requires a routing input".into());
    };
    let actual = router.evaluate(AgentRole::from(*role), *complexity);
    ExactMatch.score(&Expectation::Routing(expectation.clone()), &actual)
}

/// Scores a deterministic agent case across its repetitions.
fn score_deterministic<D: LoopDriver>(case: &EvalCase, driver: &D) -> Verdict {
    let Input::Agent { scenario } = &case.input else {
        return Verdict::Fail("agent case requires an agent input".into());
    };
    let scorer = Deterministic;
    let runs: Vec<Verdict> = (0..case.repetitions.max(1))
        .map(|_| {
            let actual = driver.drive(scenario);
            scorer.score(&case.expectation, &actual)
        })
        .collect();
    aggregate_runs(&runs, case.pass_threshold_k.max(1))
}

/// Scores a rubric agent case across its repetitions.
fn score_rubric<D: LoopDriver, J: Judge>(case: &EvalCase, driver: &D, judge: &J) -> Verdict {
    let Input::Agent { scenario } = &case.input else {
        return Verdict::Fail("agent case requires an agent input".into());
    };
    let scorer = Rubric::new(judge);
    let runs: Vec<Verdict> = (0..case.repetitions.max(1))
        .map(|_| {
            let actual = driver.drive(scenario);
            scorer.score(&case.expectation, &actual)
        })
        .collect();
    aggregate_runs(&runs, case.pass_threshold_k.max(1))
}

/// Scores one case, honouring the offline switch.
fn score_case<R, D, J>(
    case: &EvalCase,
    router: &R,
    driver: &D,
    judge: &J,
    offline: bool,
) -> CaseStatus
where
    R: RouteEvaluator,
    D: LoopDriver,
    J: Judge,
{
    match (case.kind, &case.expectation) {
        (CaseKind::Routing, Expectation::Routing(expectation)) => {
            CaseStatus::from_verdict(&score_routing(case, router, expectation))
        }
        (CaseKind::Agent, Expectation::Agent(AgentExpectation::Deterministic { .. })) => {
            // Deterministic agent cases run offline only against an offline
            // driver; a live driver is skipped under the offline switch.
            if offline && driver.is_live() {
                CaseStatus::Skip("offline: live driver skipped".into())
            } else {
                CaseStatus::from_verdict(&score_deterministic(case, driver))
            }
        }
        (CaseKind::Agent, Expectation::Agent(AgentExpectation::Rubric(_))) => {
            if offline {
                CaseStatus::Skip("offline: rubric case skipped".into())
            } else {
                CaseStatus::from_verdict(&score_rubric(case, driver, judge))
            }
        }
        _ => CaseStatus::Fail("case kind does not match its expectation".into()),
    }
}

/// Runs every case in `suite` through the injected collaborators and returns an
/// aggregated [`EvalReport`].
///
/// Routing cases are scored deterministically via `ExactMatch` over the
/// [`RouteEvaluator`]. Agent cases are driven through the [`LoopDriver`] and
/// scored with `Deterministic` and/or `Rubric` (the latter via `judge`),
/// honouring per-case `repetitions` / `pass_threshold_k`. When
/// [`OFFLINE_ENV`] is set, rubric and live-driver cases are skipped while
/// deterministic and routing cases still run.
#[must_use = "the report carries the verdicts and threshold result"]
pub fn run_suite<R, D, J>(suite: &Suite, router: &R, driver: &D, judge: &J) -> EvalReport
where
    R: RouteEvaluator,
    D: LoopDriver,
    J: Judge,
{
    run_suite_with(suite, router, driver, judge, offline_from_env())
}

/// Runs a suite with an explicit `offline` flag.
///
/// [`run_suite`] reads the flag from the environment; this inner form takes it
/// directly so the offline/online behaviour is testable without mutating the
/// process-global environment.
#[must_use = "the report carries the verdicts and threshold result"]
pub fn run_suite_with<R, D, J>(
    suite: &Suite,
    router: &R,
    driver: &D,
    judge: &J,
    offline: bool,
) -> EvalReport
where
    R: RouteEvaluator,
    D: LoopDriver,
    J: Judge,
{
    let verdicts: Vec<CaseVerdict> = suite
        .cases
        .iter()
        .map(|case| CaseVerdict {
            id: case.id.clone(),
            status: score_case(case, router, driver, judge, offline),
        })
        .collect();
    EvalReport::new(
        suite.config.name.clone(),
        suite.config.pass_threshold,
        verdicts,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{EvalRole, SuiteConfig};
    use smedja_types::{Runner, Tier};

    fn routing_case(
        id: &str,
        role: EvalRole,
        complexity: Complexity,
        exp: RoutingExpectation,
    ) -> EvalCase {
        EvalCase {
            version: 1,
            id: id.to_owned(),
            description: String::new(),
            kind: CaseKind::Routing,
            input: Input::Routing { role, complexity },
            expectation: Expectation::Routing(exp),
            repetitions: 1,
            pass_threshold_k: 1,
        }
    }

    fn agent_case(id: &str, expectation: AgentExpectation, reps: u32, k: u32) -> EvalCase {
        EvalCase {
            version: 1,
            id: id.to_owned(),
            description: String::new(),
            kind: CaseKind::Agent,
            input: Input::Agent {
                scenario: "scenario".to_owned(),
            },
            expectation: Expectation::Agent(expectation),
            repetitions: reps,
            pass_threshold_k: k,
        }
    }

    fn suite(name: &str, threshold: f64, cases: Vec<EvalCase>) -> Suite {
        Suite {
            config: SuiteConfig {
                name: name.to_owned(),
                pass_threshold: threshold,
            },
            cases,
        }
    }

    struct OfflineDriver {
        outcome: Outcome,
    }

    impl LoopDriver for OfflineDriver {
        fn drive(&self, _scenario: &str) -> Outcome {
            self.outcome.clone()
        }
        fn is_live(&self) -> bool {
            false
        }
    }

    struct FlakyDriver {
        // A cell of remaining "pass" runs; counts down each drive.
        passes_left: std::cell::Cell<u32>,
    }

    impl LoopDriver for FlakyDriver {
        fn drive(&self, _scenario: &str) -> Outcome {
            let left = self.passes_left.get();
            if left > 0 {
                self.passes_left.set(left - 1);
                Outcome::Loop {
                    final_state: "complete".to_owned(),
                    slices_completed: 1,
                }
            } else {
                Outcome::Loop {
                    final_state: "failed".to_owned(),
                    slices_completed: 0,
                }
            }
        }
        fn is_live(&self) -> bool {
            false
        }
    }

    struct PanicJudge;
    impl Judge for PanicJudge {
        fn judge(&self, _rubric: &str, _output: &str) -> Verdict {
            panic!("judge must not be called");
        }
    }

    struct FixedJudge(Verdict);
    impl Judge for FixedJudge {
        fn judge(&self, _rubric: &str, _output: &str) -> Verdict {
            self.0.clone()
        }
    }

    #[test]
    fn assayer_route_evaluator_scores_review_coding() {
        let router = Assayer::default_rules();
        let pass_case = routing_case(
            "review-coding",
            EvalRole::Review,
            Complexity::Coding,
            RoutingExpectation {
                runner: Runner::Claude,
                tier: Tier::Deep,
            },
        );
        let verdict = score_routing(
            &pass_case,
            &router,
            &RoutingExpectation {
                runner: Runner::Claude,
                tier: Tier::Deep,
            },
        );
        assert_eq!(verdict, Verdict::Pass);

        let wrong = RoutingExpectation {
            runner: Runner::Local,
            tier: Tier::Local,
        };
        let fail_verdict = score_routing(&pass_case, &router, &wrong);
        assert!(!fail_verdict.is_pass());
    }

    #[test]
    fn run_suite_scores_every_routing_case_and_counts() {
        let cases = vec![
            routing_case(
                "review-coding",
                EvalRole::Review,
                Complexity::Coding,
                RoutingExpectation {
                    runner: Runner::Claude,
                    tier: Tier::Deep,
                },
            ),
            routing_case(
                "impl-simple-wrong",
                EvalRole::Impl,
                Complexity::Simple,
                RoutingExpectation {
                    runner: Runner::Claude,
                    tier: Tier::Deep,
                },
            ),
        ];
        let s = suite("routing", 1.0, cases);
        let router = Assayer::default_rules();
        let driver = OfflineDriver {
            outcome: Outcome::Output {
                text: String::new(),
            },
        };
        let report = run_suite(&s, &router, &driver, &PanicJudge);
        assert_eq!(report.total(), 2);
        assert_eq!(report.passed(), 1);
        assert!(report.verdicts[0].is_pass());
        assert!(!report.verdicts[1].is_pass());
    }

    #[test]
    fn k_of_n_repetition_decides_a_flaky_case() {
        let expectation = AgentExpectation::Deterministic {
            final_state: Some("complete".to_owned()),
            min_slices_completed: Some(1),
        };
        // 2 of 3 runs pass.
        let driver = FlakyDriver {
            passes_left: std::cell::Cell::new(2),
        };
        let s = suite(
            "agent",
            1.0,
            vec![agent_case("flaky", expectation.clone(), 3, 2)],
        );
        let router = Assayer::default_rules();
        let report = run_suite(&s, &router, &driver, &PanicJudge);
        assert!(report.verdicts[0].is_pass(), "2-of-3 with k=2 must pass");

        let driver = FlakyDriver {
            passes_left: std::cell::Cell::new(2),
        };
        let s = suite("agent", 1.0, vec![agent_case("flaky", expectation, 3, 3)]);
        let report = run_suite(&s, &router, &driver, &PanicJudge);
        assert!(!report.verdicts[0].is_pass(), "2-of-3 with k=3 must fail");
    }

    #[test]
    fn offline_skips_rubric_and_runs_deterministic() {
        let rubric = agent_case(
            "rubric",
            AgentExpectation::Rubric("coherent?".to_owned()),
            1,
            1,
        );
        let deterministic = agent_case(
            "deterministic",
            AgentExpectation::Deterministic {
                final_state: Some("complete".to_owned()),
                min_slices_completed: Some(1),
            },
            1,
            1,
        );
        let s = suite("agent", 1.0, vec![rubric, deterministic]);
        let router = Assayer::default_rules();
        let driver = OfflineDriver {
            outcome: Outcome::Loop {
                final_state: "complete".to_owned(),
                slices_completed: 1,
            },
        };
        // PanicJudge proves the rubric case is skipped before judging.
        let report = run_suite_with(&s, &router, &driver, &PanicJudge, true);

        assert!(
            report.verdicts[0].is_skip(),
            "rubric case must be skipped offline"
        );
        assert!(
            report.verdicts[1].is_pass(),
            "deterministic case must still run"
        );
        assert_eq!(report.scored(), 1);
    }

    #[test]
    fn rubric_case_scored_when_online() {
        let s = suite(
            "agent",
            1.0,
            vec![agent_case(
                "rubric",
                AgentExpectation::Rubric("coherent?".to_owned()),
                1,
                1,
            )],
        );
        let router = Assayer::default_rules();
        let driver = OfflineDriver {
            outcome: Outcome::Output {
                text: "a diff".to_owned(),
            },
        };
        let report = run_suite_with(
            &s,
            &router,
            &driver,
            &FixedJudge(Verdict::Fail("no".into())),
            false,
        );
        assert!(!report.verdicts[0].is_pass());
        assert_eq!(report.scored(), 1);
    }

    #[test]
    fn online_deterministic_case_runs() {
        // With offline = false a deterministic case is scored via the driver
        // regardless of whether the driver reports as live.
        let s = suite(
            "agent",
            1.0,
            vec![agent_case(
                "deterministic",
                AgentExpectation::Deterministic {
                    final_state: Some("complete".to_owned()),
                    min_slices_completed: Some(1),
                },
                1,
                1,
            )],
        );
        let router = Assayer::default_rules();
        let driver = OfflineDriver {
            outcome: Outcome::Loop {
                final_state: "complete".to_owned(),
                slices_completed: 1,
            },
        };
        let report = run_suite_with(&s, &router, &driver, &PanicJudge, false);
        assert!(report.verdicts[0].is_pass());
    }

    #[test]
    fn offline_from_env_reads_the_switch() {
        // The only test that mutates the process-global env var; it restores
        // the prior value to avoid leaking state to other tests.
        let prior = std::env::var(OFFLINE_ENV).ok();
        std::env::set_var(OFFLINE_ENV, "1");
        assert!(offline_from_env());
        std::env::set_var(OFFLINE_ENV, "0");
        assert!(!offline_from_env());
        match prior {
            Some(value) => std::env::set_var(OFFLINE_ENV, value),
            None => std::env::remove_var(OFFLINE_ENV),
        }
    }
}
