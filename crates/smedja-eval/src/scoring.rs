//! Pluggable scoring strategies.
//!
//! A [`Scorer`] turns an [`Expectation`] and the captured [`Outcome`] into a
//! [`Verdict`]. Three strategies are provided:
//!
//! - [`ExactMatch`] — structural equality, used by routing.
//! - [`Deterministic`] — a named predicate over the outcome, used by agent
//!   evals with a crisp programmatic success condition.
//! - [`Rubric`] — an LLM-judge behind the injected [`Judge`] trait; the crate
//!   never constructs a provider.

use smedja_types::{Runner, Tier};

use crate::case::{AgentExpectation, Expectation, RoutingExpectation};

/// The verdict for a single scored case or run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The outcome met the expectation.
    Pass,
    /// The outcome did not meet the expectation, carrying a short reason.
    Fail(String),
}

impl Verdict {
    /// Returns `true` only for [`Verdict::Pass`].
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// The captured outcome of running a case, scored against its expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A routing destination produced by the route evaluator.
    Routing {
        /// The chosen runner backend.
        runner: Runner,
        /// The chosen execution tier.
        tier: Tier,
    },
    /// A loop outcome produced by the driver.
    Loop {
        /// The terminal `LoopState` label (lowercase).
        final_state: String,
        /// The number of slices that completed.
        slices_completed: u64,
    },
    /// Produced output to be judged by a rubric.
    Output {
        /// The captured agent output.
        text: String,
    },
}

/// An LLM-judge that scores produced output against a rubric.
///
/// Injected so the eval crate never constructs a model provider; the [`Rubric`]
/// scorer calls through this trait and tests supply a fake.
pub trait Judge {
    /// Judges `output` against `rubric`, returning a [`Verdict`].
    fn judge(&self, rubric: &str, output: &str) -> Verdict;
}

/// Scores an expectation against a captured outcome.
pub trait Scorer {
    /// Returns the [`Verdict`] for `actual` against `expected`.
    fn score(&self, expected: &Expectation, actual: &Outcome) -> Verdict;
}

/// Structural equality scorer, used by routing cases.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExactMatch;

impl Scorer for ExactMatch {
    fn score(&self, expected: &Expectation, actual: &Outcome) -> Verdict {
        match (expected, actual) {
            (
                Expectation::Routing(RoutingExpectation { runner, tier }),
                Outcome::Routing {
                    runner: got_runner,
                    tier: got_tier,
                },
            ) => {
                if runner == got_runner && tier == got_tier {
                    Verdict::Pass
                } else {
                    Verdict::Fail(format!(
                        "expected ({runner:?}, {tier:?}), got ({got_runner:?}, {got_tier:?})"
                    ))
                }
            }
            _ => Verdict::Fail("exact-match scorer requires a routing expectation/outcome".into()),
        }
    }
}

/// Named-predicate scorer over a loop outcome.
#[derive(Debug, Clone, Copy, Default)]
pub struct Deterministic;

impl Scorer for Deterministic {
    fn score(&self, expected: &Expectation, actual: &Outcome) -> Verdict {
        let Expectation::Agent(AgentExpectation::Deterministic {
            final_state,
            min_slices_completed,
        }) = expected
        else {
            return Verdict::Fail(
                "deterministic scorer requires a deterministic agent expectation".into(),
            );
        };
        let Outcome::Loop {
            final_state: got_state,
            slices_completed,
        } = actual
        else {
            return Verdict::Fail("deterministic scorer requires a loop outcome".into());
        };

        if let Some(want_state) = final_state {
            if want_state != got_state {
                return Verdict::Fail(format!(
                    "expected final_state {want_state}, got {got_state}"
                ));
            }
        }
        if let Some(min) = min_slices_completed {
            if slices_completed < min {
                return Verdict::Fail(format!(
                    "expected slices_completed >= {min}, got {slices_completed}"
                ));
            }
        }
        Verdict::Pass
    }
}

/// Rubric scorer that delegates judgement to an injected [`Judge`].
pub struct Rubric<'judge> {
    judge: &'judge dyn Judge,
}

impl<'judge> Rubric<'judge> {
    /// Creates a rubric scorer over `judge`.
    #[must_use]
    pub fn new(judge: &'judge dyn Judge) -> Self {
        Self { judge }
    }
}

impl Scorer for Rubric<'_> {
    fn score(&self, expected: &Expectation, actual: &Outcome) -> Verdict {
        let Expectation::Agent(AgentExpectation::Rubric(rubric)) = expected else {
            return Verdict::Fail("rubric scorer requires a rubric expectation".into());
        };
        let Outcome::Output { text } = actual else {
            return Verdict::Fail("rubric scorer requires an output outcome".into());
        };
        self.judge.judge(rubric, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::RoutingExpectation;

    fn routing_expectation(runner: Runner, tier: Tier) -> Expectation {
        Expectation::Routing(RoutingExpectation { runner, tier })
    }

    #[test]
    fn exact_match_passes_on_equal_destination() {
        let scorer = ExactMatch;
        let expected = routing_expectation(Runner::Claude, Tier::Deep);
        let actual = Outcome::Routing {
            runner: Runner::Claude,
            tier: Tier::Deep,
        };
        assert_eq!(scorer.score(&expected, &actual), Verdict::Pass);
    }

    #[test]
    fn exact_match_fails_on_mismatch() {
        let scorer = ExactMatch;
        let expected = routing_expectation(Runner::Claude, Tier::Deep);
        let actual = Outcome::Routing {
            runner: Runner::Local,
            tier: Tier::Local,
        };
        assert!(!scorer.score(&expected, &actual).is_pass());
    }

    #[test]
    fn deterministic_passes_when_predicate_holds() {
        let scorer = Deterministic;
        let expected = Expectation::Agent(AgentExpectation::Deterministic {
            final_state: Some("complete".to_owned()),
            min_slices_completed: Some(1),
        });
        let actual = Outcome::Loop {
            final_state: "complete".to_owned(),
            slices_completed: 1,
        };
        assert_eq!(scorer.score(&expected, &actual), Verdict::Pass);
    }

    #[test]
    fn deterministic_fails_when_state_differs() {
        let scorer = Deterministic;
        let expected = Expectation::Agent(AgentExpectation::Deterministic {
            final_state: Some("complete".to_owned()),
            min_slices_completed: None,
        });
        let actual = Outcome::Loop {
            final_state: "failed".to_owned(),
            slices_completed: 0,
        };
        assert!(!scorer.score(&expected, &actual).is_pass());
    }

    #[test]
    fn deterministic_fails_when_slices_below_minimum() {
        let scorer = Deterministic;
        let expected = Expectation::Agent(AgentExpectation::Deterministic {
            final_state: None,
            min_slices_completed: Some(3),
        });
        let actual = Outcome::Loop {
            final_state: "complete".to_owned(),
            slices_completed: 2,
        };
        assert!(!scorer.score(&expected, &actual).is_pass());
    }

    struct FakeJudge {
        verdict: Verdict,
    }

    impl Judge for FakeJudge {
        fn judge(&self, _rubric: &str, _output: &str) -> Verdict {
            self.verdict.clone()
        }
    }

    #[test]
    fn rubric_propagates_judge_pass() {
        let judge = FakeJudge {
            verdict: Verdict::Pass,
        };
        let scorer = Rubric::new(&judge);
        let expected =
            Expectation::Agent(AgentExpectation::Rubric("is the diff coherent?".to_owned()));
        let actual = Outcome::Output {
            text: "a diff".to_owned(),
        };
        assert_eq!(scorer.score(&expected, &actual), Verdict::Pass);
    }

    #[test]
    fn rubric_propagates_judge_fail() {
        let judge = FakeJudge {
            verdict: Verdict::Fail("incoherent".to_owned()),
        };
        let scorer = Rubric::new(&judge);
        let expected =
            Expectation::Agent(AgentExpectation::Rubric("is the diff coherent?".to_owned()));
        let actual = Outcome::Output {
            text: "noise".to_owned(),
        };
        assert!(!scorer.score(&expected, &actual).is_pass());
    }
}
