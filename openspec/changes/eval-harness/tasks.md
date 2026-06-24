## 1. Scaffold the smedja-eval crate

- [x] 1.1 Create `crates/smedja-eval/Cargo.toml` (workspace deps: `serde`, `serde_json`, `toml`, `anyhow`, `thiserror`, `opentelemetry`; path deps: `smedja-types`, `smedja-assayer`, `smedja-loop`) and add `crates/smedja-eval` to the workspace `members` list in the root `Cargo.toml`
- [x] 1.2 Create `crates/smedja-eval/src/lib.rs` with module declarations (`case`, `scoring`, `engine`, `report`, `telemetry`) and crate-level docs; confirm `cargo build -p smedja-eval` succeeds with empty modules

## 2. Eval-case format and suite loader (case.rs)

- [x] 2.1 Write failing tests for `EvalCase` deserialisation: a routing case `(role, complexity) → (runner, tier)` and an agent case `(scenario) → expected outcome`, plus a `version` mismatch rejection test
- [x] 2.2 Define the `EvalCase` envelope (`version`, `id`, `description`, `kind`, `input`, `expectation`) and the `Expectation` enum (`Routing`, `Agent`) with `serde` derives; make the deserialisation tests pass
- [x] 2.3 Write a failing test for `load_suite(dir)` that reads a directory of `.json`/`.toml` case files plus a `suite.toml` (scoring config + `pass_threshold`)
- [x] 2.4 Implement `load_suite`; reject unknown `version` and malformed files with a typed error; make the loader test pass

## 3. Scoring strategies (scoring.rs)

- [x] 3.1 Write failing tests for the `Scorer` trait: `ExactMatch` passes on equal destinations and fails on a mismatch
- [x] 3.2 Define `trait Scorer`, the `Outcome`/`Verdict` types, and implement `ExactMatch`; make the test pass
- [x] 3.3 Write a failing test for `Deterministic` (a named predicate over an outcome, e.g. terminal state `Complete`, `slices_completed >= N`)
- [x] 3.4 Implement `Deterministic`; make the test pass
- [x] 3.5 Write a failing test for `Rubric` using a fake `Judge` (asserts pass/fail propagates from the judge verdict and that an offline run skips the case)
- [x] 3.6 Define `trait Judge` and implement `Rubric` over it; make the test pass

## 4. Routing eval scoring against the assayer

- [x] 4.1 Write a failing test asserting a routing case for `(Review, Coding)` scores pass against `Assayer::default_rules().route_decision(...)` and a wrong-expectation case scores fail
- [x] 4.2 Define the `RouteEvaluator` trait with a real `Assayer`-backed impl; wire routing cases through `ExactMatch` over `route_decision`; make the test pass

## 5. Run engine and repetition handling (engine.rs)

- [x] 5.1 Write a failing test for `run_suite` over a small in-memory routing suite: every case scored, aggregate counts correct
- [x] 5.2 Implement `run_suite(suite, &router, &driver, &judge) -> EvalReport` with the injected-trait collaborators (`RouteEvaluator`, `LoopDriver`, `Judge`); ship a fake `LoopDriver`/`Judge` for tests; make the test pass
- [x] 5.3 Write a failing test for k-of-n repetition: a flaky fake judge passing 2 of 3 runs yields case-pass at `pass_threshold_k = 2` and case-fail at `k = 3`
- [x] 5.4 Implement `repetitions` / `pass_threshold_k` in the engine; make the test pass
- [x] 5.5 Write a failing test for `SMEDJA_EVAL_OFFLINE=1`: graded/rubric and live-driver cases are skipped, deterministic cases still run and score
- [x] 5.6 Implement the offline switch in the engine; make the test pass

## 6. Report and threshold gate (report.rs)

- [x] 6.1 Write failing tests for `EvalReport`: per-case verdicts, aggregate `pass_rate`, `meets_threshold(threshold)` true/false, and a stable JSON serialisation
- [x] 6.2 Implement `EvalReport` (counts, pass rate, threshold check, `to_json`); make the tests pass

## 7. Eval telemetry (telemetry.rs)

- [x] 7.1 Write a test that `record_eval_metrics` runs without panicking under a no-op meter provider (mirroring the `smedja-loop` telemetry tests)
- [x] 7.2 Implement `record_eval_metrics(suite, cases, passed)` and `record_eval_duration(suite, secs)` from `global::meter("smedja.eval")` with names `smedja_eval_cases_total`, `smedja_eval_pass_total`, `smedja_eval_pass_rate`, `smedja_eval_duration_seconds`; make the test pass

## 8. smj eval subcommand

- [x] 8.1 Add `smedja-eval` as a dependency in `bin/smj/Cargo.toml`
- [x] 8.2 Add an `Eval { action: EvalCmd }` variant to the `Cmd` enum and an `EvalCmd::Run { suite, online, json, threshold }` subcommand in `bin/smj/src/main.rs`
- [x] 8.3 Implement the `eval run` handler: load the suite, call `run_suite`, print the human report, write JSON when `--json`, emit metrics, and exit non-zero when the pass rate is below threshold

## 9. cargo xtask eval entry point

- [x] 9.1 Add `smedja-eval` as a dependency in `xtask/Cargo.toml`
- [x] 9.2 Add an `Eval { suite, threshold }` command to the xtask `Command` enum and implement it as a thin shim over `run_suite` with a non-zero exit below threshold

## 10. Starter corpus and CI wiring

- [x] 10.1 Create `evals/routing/` with example routing case files covering each `AgentRole` and a `suite.toml`, matching the current `default_rules()` table
- [x] 10.2 Create `evals/agent/` with an example agent case (deterministic outcome) and a `suite.toml`, documenting the graded format by example
- [x] 10.3 Add an integration test in `smedja-eval` that loads `evals/routing/` and asserts the suite passes at 100% against `Assayer::default_rules()` (this is the routing regression gate that runs inside `cargo test`)

## 11. Verify

- [x] 11.1 Run `cargo test --workspace` — all green
- [x] 11.2 Run `cargo fmt --all -- --check` and `cargo clippy -p smedja-eval --all-targets -- -D warnings -W clippy::pedantic` — clean for the new crate
- [x] 11.3 Run `cargo xtask eval evals/routing` and `smj eval run --suite evals/routing` — both report 100% pass and exit 0
- [x] 11.4 Run `SMEDJA_EVAL_OFFLINE=1 cargo xtask eval evals/agent` — graded cases skipped, deterministic cases scored, exit 0
- [x] 11.5 Run `openspec validate eval-harness --strict` — clean
