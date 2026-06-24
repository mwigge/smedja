## ADDED Requirements

### Requirement: Eval cases are defined as data and loaded from a suite directory

The `smedja-eval` crate SHALL define an `EvalCase` envelope (`version`, `id`, `description`, `kind`, `input`, `expectation`) that is deserialised from case files rather than embedded in test source. A suite SHALL be a directory of case files accompanied by a `suite.toml` carrying the scoring configuration and pass threshold. The loader MUST reject an unknown case `version` with a typed error rather than silently mis-scoring.

#### Scenario: a routing case is loaded from a file

- **WHEN** a suite directory contains a case file with `kind = "routing"`, an input `(role, complexity)`, and an expected `(runner, tier)` destination
- **THEN** `load_suite` SHALL return a suite containing that `EvalCase` with its expectation parsed
- **AND** the case SHALL be addressable by its `id`

#### Scenario: an unknown case version is rejected

- **WHEN** a case file declares a `version` the crate does not support
- **THEN** `load_suite` SHALL return an error identifying the offending file and version
- **AND** no partial suite SHALL be returned

### Requirement: Scoring is pluggable via a Scorer trait

The crate SHALL expose a `Scorer` trait and at least three implementations — `ExactMatch` (structural equality), `Deterministic` (a named predicate over the outcome), and `Rubric` (an LLM-judge behind an injected `Judge` trait) — and each case SHALL select which scorer applies. The crate MUST NOT construct a model provider; the `Rubric` scorer MUST call through the injected `Judge`.

#### Scenario: exact-match scorer compares destinations

- **WHEN** an `ExactMatch` scorer compares an expected destination to an equal actual destination
- **THEN** the verdict SHALL be pass
- **AND** an unequal actual destination SHALL yield a fail verdict

#### Scenario: rubric scorer uses the injected judge

- **WHEN** a `Rubric` case is scored with a fake `Judge` that returns a pass verdict
- **THEN** the case verdict SHALL be pass
- **AND** the crate SHALL NOT construct any provider to obtain that verdict

### Requirement: The run engine aggregates verdicts into a report and gates on a threshold

The crate SHALL expose `run_suite` that takes the side-effecting collaborators (route evaluator, loop driver, judge) behind traits and returns an `EvalReport` carrying per-case verdicts, the case and pass counts, and the aggregate pass rate. The report SHALL expose a threshold check, and the runner SHALL exit non-zero when the pass rate is below the configured threshold.

#### Scenario: report aggregates a routing suite

- **WHEN** `run_suite` runs an in-memory suite of routing cases
- **THEN** the returned `EvalReport` SHALL record a verdict for every case
- **AND** the aggregate pass rate SHALL equal passed cases divided by total cases

#### Scenario: threshold gate fails below the configured rate

- **WHEN** a suite's pass rate is below the suite's configured threshold
- **THEN** `EvalReport::meets_threshold` SHALL return false
- **AND** the `smj eval` and `cargo xtask eval` runners SHALL exit non-zero

### Requirement: The eval runner is reachable from smj and xtask

The harness SHALL be runnable via `smj eval run` (operator-facing) and `cargo xtask eval` (CI/dev-facing). Both entry points SHALL load a suite, run it through `run_suite`, print a human-readable report, optionally emit a machine-readable JSON summary, and share the crate's scoring logic rather than reimplementing it.

#### Scenario: smj eval run reports and gates

- **WHEN** `smj eval run --suite <dir>` is invoked on a suite that meets its threshold
- **THEN** a human-readable report SHALL be printed
- **AND** the process SHALL exit zero
- **AND** the same invocation on a suite below threshold SHALL exit non-zero

### Requirement: The harness emits eval metrics via OpenTelemetry

The harness SHALL emit eval metrics through the global meter `smedja.eval`, recording `smedja_eval_cases_total`, `smedja_eval_pass_total`, `smedja_eval_pass_rate`, and a `smedja_eval_duration_seconds` histogram, each carrying a `suite` attribute, so pass rates are observable over time.

#### Scenario: running a suite records metrics

- **WHEN** a suite completes a run
- **THEN** the harness SHALL record the total case count and pass count under the `smedja.eval` meter
- **AND** each metric SHALL carry the suite identifier as an attribute
