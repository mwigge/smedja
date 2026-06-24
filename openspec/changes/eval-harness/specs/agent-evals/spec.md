## ADDED Requirements

### Requirement: Agent evals score loop and agent outcomes with deterministic and rubric scorers

The harness SHALL evaluate agent/loop behaviour by running a change/slice scenario through an injected loop driver and scoring the resulting outcome. A `Deterministic` scorer SHALL assert over the loop outcome — the terminal `LoopState` and the slice pass count — and a `Rubric` scorer SHALL judge produced output via the injected `Judge`. The loop driver MUST be supplied behind a trait so the eval crate carries no daemon or provider coupling.

#### Scenario: deterministic outcome check passes on a complete loop

- **WHEN** an agent case expects terminal state `Complete` and `slices_completed >= 1`, and the driver returns a `LoopOutcome` with `final_state = Complete` and `slices_completed = 1`
- **THEN** the `Deterministic` scorer SHALL yield a pass verdict

#### Scenario: rubric judgement scores produced output

- **WHEN** an agent case carries a rubric expectation and the injected judge returns a fail verdict for the produced output
- **THEN** the case verdict SHALL be fail
- **AND** the failure SHALL be attributable to the rubric scorer in the report

### Requirement: Graded evals handle non-determinism and are excluded from the default gate

Agent evals SHALL support `repetitions` and a `pass_threshold_k`, where a case runs N times and passes when at least K runs pass. `SMEDJA_EVAL_OFFLINE=1` SHALL skip rubric and live-driver cases while still running deterministic scorers. Graded suites SHALL NOT run on the default CI gate, so the gate stays fast and free.

#### Scenario: k-of-n repetition decides a flaky case

- **WHEN** a case sets `repetitions = 3` and `pass_threshold_k = 2`, and the grader passes exactly 2 of the 3 runs
- **THEN** the case verdict SHALL be pass
- **AND** the same case with `pass_threshold_k = 3` SHALL yield a fail verdict

#### Scenario: offline mode skips graded cases

- **WHEN** the suite is run with `SMEDJA_EVAL_OFFLINE=1`
- **THEN** rubric and live-driver cases SHALL be skipped rather than failed
- **AND** deterministic cases SHALL still be run and scored
