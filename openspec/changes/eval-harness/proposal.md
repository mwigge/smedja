## Why

smedja has no evaluation framework. Routing quality (the `smedja-assayer` role × complexity → runner/tier table), loop outcomes (the `smedja-loop` `drive` pipeline and its deterministic verification gate), and agent behaviour are all asserted only by hand-written unit tests against fixed inputs. There is no way to:

- define a corpus of eval cases (input → expected or graded output) outside the test source,
- run that corpus against the routing decision surface or the loop/agent pipeline,
- score the results with more than one strategy (exact-match, deterministic check, rubric / LLM-judge), or
- track pass rates over time to catch regressions when a routing rule, prompt, or model default changes.

This is felt most acutely on the two surfaces that already have a clean, callable contract. `Assayer::route_decision(role, complexity)` is pure and deterministic — the ideal regression target — yet a change to `default_rules()` could silently alter routing for an entire role class with no corpus to catch it. The loop's `LoopOutcome { final_state, slices_completed }` is the natural graded target for end-to-end agent behaviour, but nothing exercises it against a labelled set of scenarios.

This change adds a Rust-native, CI-runnable eval harness: a `smedja-eval` crate that defines the eval-case format and scoring strategies, plus a `smj eval` subcommand and a `cargo xtask eval` entry point that load a case corpus, run it against routing and/or the loop pipeline, score it, and report pass rates — emitting the result as OTel metrics so trends are observable.

## What Changes

- **New `smedja-eval` crate** holding the eval-case format (`EvalCase`), the case-suite loader, the scoring strategies (`Scorer` trait with `ExactMatch`, `Deterministic`, and `Rubric`/LLM-judge implementations), the run engine (`run_suite`), and the report type (`EvalReport` with per-case verdicts and an aggregate pass rate). The crate follows the loop's split-trait pattern: deterministic scoring and aggregation live in the crate and are unit-testable with fakes; the side-effecting graders (LLM-judge, loop driver) are injected behind traits.
- **Routing evals (deterministic, exact-match)**: an `EvalCase` whose input is a `(role, complexity)` pair and whose expectation is a `(runner, tier)` destination, scored by exact comparison against `Assayer::route_decision`. No model calls; fully deterministic; runs on every CI build.
- **Agent / loop evals (graded)**: an `EvalCase` whose input is a change/slice scenario and whose expectation is a graded outcome (terminal `LoopState`, slice pass count, or a rubric judgement of produced output), scored by a `Deterministic` checker over `LoopOutcome` and/or a `Rubric` LLM-judge over captured output. These are gated off the default CI run because they are non-deterministic and may incur model cost.
- **`smj eval run` subcommand** and **`cargo xtask eval` entry point**: both load a suite from a directory of case files and a scoring config, run it, print a human report and a machine-readable JSON summary, and exit non-zero when the pass rate falls below a configured threshold (the regression gate).
- **OTel eval metrics**: the harness emits `smedja_eval_cases_total`, `smedja_eval_pass_total`, and `smedja_eval_pass_rate` (plus a per-suite duration histogram) via the global meter, following the existing `smedja-loop` telemetry naming so pass rates are trackable over time.
- **Non-determinism handling**: graded suites support N repetitions per case with a pass-threshold (k-of-n), a fixed seed surface where the grader supports it, and a `SMEDJA_EVAL_OFFLINE=1` switch that skips LLM-judge cases and runs only deterministic scorers — so CI stays fast and free by default.

Out of scope (referenced only): no changes to the routing rules, loop pipeline, or prompt assembly themselves — the harness observes existing contracts. No new provider integration; the LLM-judge reuses the existing adapter surface through an injected trait rather than constructing providers itself. No web UI or dashboard; reporting is CLI + OTel.

## Capabilities

### New Capabilities

- `eval-harness`: the `smedja-eval` crate defines the eval-case format, the suite loader, the pluggable scoring strategies, the run engine, the report and threshold gate, the `smj eval` / `cargo xtask eval` runners, and the eval OTel metrics.
- `routing-evals`: a deterministic, exact-match eval surface that scores `Assayer::route_decision` outputs against a labelled `(role, complexity) → (runner, tier)` corpus, runnable on every CI build with no model calls.
- `agent-evals`: a graded eval surface that scores loop/agent outcomes — terminal `LoopState`, slice pass count, and rubric / LLM-judge verdicts over produced output — with repetition-based non-determinism handling and an offline-skip switch.

## Impact

- `Cargo.toml`: add `crates/smedja-eval` to the workspace members.
- `crates/smedja-eval/` (new): `Cargo.toml`, `src/lib.rs`, `src/case.rs` (`EvalCase`, suite loader), `src/scoring.rs` (`Scorer` trait + `ExactMatch`/`Deterministic`/`Rubric`), `src/engine.rs` (`run_suite`, repetition handling), `src/report.rs` (`EvalReport`, threshold gate), `src/telemetry.rs` (eval metrics).
- `bin/smj/Cargo.toml`, `bin/smj/src/main.rs`: add `smedja-eval` dependency; add the `Eval` subcommand (`smj eval run`).
- `xtask/Cargo.toml`, `xtask/src/main.rs`: add `smedja-eval` dependency; add the `Eval` xtask command.
- `crates/smedja-eval` depends on `smedja-assayer` (routing target), `smedja-types`, and `smedja-loop` (graded target via injected driver trait); the LLM-judge grader is behind a trait, so no direct provider dependency.
- `evals/` (new corpus directory): example routing and agent case files plus a suite config, so the gate has a starting corpus and the format is documented by example.
- CI: the deterministic routing suite is wired into the default `test` job; the graded suite runs only when explicitly invoked (not on the default gate) to keep CI fast and free.
