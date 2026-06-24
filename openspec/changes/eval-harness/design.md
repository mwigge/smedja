## Context

The two surfaces this harness targets already expose clean, callable contracts:

- **Routing** — `smedja_assayer::Assayer::route_decision(role: AgentRole, complexity: Complexity) -> RoutingDecision` (`assayer.rs`) is pure and deterministic. `RoutingDecision` carries `runner()`, `tier()`, `model()`, `complexity()`, and `rationale()`. `Assayer::default_rules()` is the production table. The inputs are the closed enums `AgentRole {Impl, Test, Review, Sre, Orchestrator}` and `Complexity {Simple, Coding, Complex}`; the outputs are `Runner` and `Tier` from `smedja-types`. This is the ideal exact-match regression target.
- **Loop / agent** — `smedja_loop::engine::drive(...) -> LoopOutcome { final_state: LoopState, slices_completed: u64 }` (`engine.rs`) is the end-to-end pipeline. It already delegates side effects through the `RoleRunner` and `StatusSink` traits, which makes the pipeline unit-testable with fakes — the harness reuses that exact pattern. The deterministic verification gate (`verify::run_verification` → `VerifyResult::passed`) is the loop's own pass/fail signal.

Telemetry conventions are set by `smedja-loop/src/telemetry.rs`: counters and histograms are created from `opentelemetry::global::meter("smedja.loop")` with `smedja_loop_*_total` / `smedja_loop_duration_seconds` names. The eval harness mirrors this under a `smedja.eval` meter.

The dev-task surfaces are `xtask` (currently only `gen-rpc-types`, a clap `Subcommand`) and `bin/smj` (a clap CLI whose `Cmd` enum dispatches to per-area subcommands). Both are natural homes for an eval runner; the design hosts the runner logic in the crate and adds thin command shims to each.

The CI gate (`.github/workflows/ci.yml`) is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings -W clippy::pedantic`, `cargo test --workspace --all-targets`, and `cargo audit`. The harness must pass all four; the deterministic routing suite runs inside `cargo test`, the graded suite does not.

## Goals / Non-Goals

Goals:
- A Rust-native eval-case format that lives outside test source, loadable from a directory of files.
- Pluggable scoring: exact-match, arbitrary deterministic checks, and rubric / LLM-judge — selected per case.
- Run the corpus against routing decisions (deterministic) and against the loop/agent pipeline (graded).
- Aggregate to a pass rate, gate on a configured threshold, and emit OTel metrics for trend tracking.
- CI-runnable and free by default: the deterministic suite gates every build; graded suites are opt-in.
- Handle non-determinism explicitly (repetition + k-of-n threshold, offline skip).

Non-Goals:
- Changing the routing rules, loop pipeline, or prompt assembly — the harness only observes existing contracts.
- A new provider integration. The LLM-judge calls through an injected trait; the harness never constructs a provider.
- Persisting eval history to a database or rendering a dashboard — reporting is CLI + OTel; trend storage is the collector's job.
- Benchmarking latency/cost as a pass criterion (a later change may add cost-budget assertions).

## Decisions

**Decision: a single `EvalCase` envelope with a typed `expectation`, deserialised from a directory of files.**
An `EvalCase` carries an `id`, a free-text `description`, a `kind` discriminant (`Routing` | `Agent`), an `input`, and an `expectation`. Suites are directories of case files (one or many cases per file) loaded by `case::load_suite(dir)`; a sibling `suite.toml` supplies the scoring config and threshold. The envelope is `serde`-derived so cases are authored as data, not Rust.
- Rationale: cases-as-data is the whole point of an eval harness — non-engineers and CI can extend the corpus without touching crate source; the loader is the only deserialisation seam.
- Format choice: TOML for the suite config (matches `.smedja/agents.toml` and `loop.json`-style config already in the repo) and JSON or TOML for case files (the loader accepts both via extension). Rejected: a bespoke DSL — unnecessary; `serde` covers it.
- Alternative considered: `#[test]`-embedded cases. Rejected — that is what exists today and what this change replaces; embedded cases cannot be extended without a recompile and cannot be run selectively.

**Decision: scoring is a `Scorer` trait with three concrete strategies, chosen per case.**
```
trait Scorer { fn score(&self, expected: &Expectation, actual: &Outcome) -> Verdict; }
```
- `ExactMatch` — structural equality (used by routing: `(runner, tier)` must match exactly). Deterministic, no I/O.
- `Deterministic` — a named predicate over the actual outcome (e.g. "terminal state is `Complete`", "slices_completed >= N"). Deterministic, no I/O. Used by agent evals that have a crisp programmatic success condition.
- `Rubric` — an LLM-judge: renders a rubric prompt over the captured output and asks a judge model for a pass/fail + score. Behind a `Judge` trait so the crate never constructs a provider and the scorer is unit-testable with a fake judge.
- Rationale: separates the deterministic, always-runnable scorers from the costly/non-deterministic one; the trait keeps the engine agnostic to which scorer a case uses.
- Alternative considered: a single hard-coded scorer with mode flags. Rejected — violates open/closed; new scoring strategies should be additive, not edits to a god-function.

**Decision: routing evals are deterministic exact-match; agent evals are graded — and they read as separate capabilities.**
Routing evals call `Assayer::route_decision` directly and compare destinations; they need no driver, no model, and no repetition — a single pass is authoritative, so the whole routing suite runs inside `cargo test` on every build. Agent evals drive the loop (or a captured agent transcript) and score with `Deterministic` and/or `Rubric`; they are non-deterministic, may cost money, and run only when explicitly invoked. The two surfaces have materially different runtime, determinism, and CI semantics, so they are documented as `routing-evals` and `agent-evals` capabilities over the shared `eval-harness` core.
- Rationale: collapsing them into one capability would hide the determinism/CI distinction that is the most important operational fact about each.

**Decision: the run engine mirrors the loop's injected-trait split.**
`engine::run_suite(suite, &router, &driver, &judge) -> EvalReport` takes the side-effecting collaborators behind traits: a `RouteEvaluator` (default: a real `Assayer`), a `LoopDriver` (default: wraps `smedja_loop::engine::drive` with the daemon's real `RoleRunner`; a fake in tests), and a `Judge` (LLM-judge; a fake in tests). The aggregation, threshold check, and report assembly are pure and live in the crate.
- Rationale: matches the established `RoleRunner`/`StatusSink` pattern in `smedja-loop`, keeping provider/daemon coupling out of the eval crate and making `run_suite` unit-testable with fakes — the same property that made the loop engine testable.

**Decision: non-determinism is handled by repetition + k-of-n threshold and an offline switch.**
A graded case may declare `repetitions: N` and `pass_threshold_k: K`; the engine runs the case N times and the case passes when ≥ K runs pass. Where a grader exposes a seed, the suite config may pin it. `SMEDJA_EVAL_OFFLINE=1` skips every `Rubric` case and any case requiring a live driver, running only `ExactMatch`/`Deterministic` scorers — this is the default CI mode.
- Rationale: a single graded run is not a reliable signal under model non-determinism; k-of-n converts a flaky boolean into a stable threshold. Offline mode keeps the default gate free and fast while still catching deterministic regressions.
- Alternative considered: snapshot/golden-file scoring of raw model output. Rejected as the *primary* strategy — raw-output snapshots are brittle under non-determinism; rubric judgement over the output is the robust form. (A deterministic checker may still assert on stable substrings.)

**Decision: the runner lives in `smedja-eval`; `smj eval` and `cargo xtask eval` are thin shims.**
Both entry points parse a suite path + flags, call `run_suite`, print the human report, write the JSON summary, and exit non-zero if the pass rate is below threshold. `smj eval run` is the operator-facing path (against a running corpus); `cargo xtask eval` is the CI/dev path. Neither contains scoring logic.
- Rationale: one implementation, two front doors; matches the existing `xtask` (build-time helper) vs `smj` (operator CLI) division already in the repo.

**Decision: eval metrics follow the `smedja-loop` telemetry shape.**
`telemetry::record_eval_metrics(suite, cases, passed)` emits `smedja_eval_cases_total`, `smedja_eval_pass_total`, and a `smedja_eval_pass_rate` gauge/derived value, plus `smedja_eval_duration_seconds`, all from `global::meter("smedja.eval")` with a `suite` attribute.
- Rationale: consistency with `smedja_loop_*` naming so the same collector/dashboard conventions apply and pass-rate trends are queryable over time.

## Risks / Trade-offs

- [Risk] Graded LLM-judge evals are non-deterministic and could flake the gate → Mitigation: graded suites are excluded from the default CI gate; they use k-of-n repetition; `SMEDJA_EVAL_OFFLINE=1` is the default mode and runs only deterministic scorers.
- [Risk] LLM-judge cases incur model cost on every run → Mitigation: never run on the default gate; offline-skip by default; the operator opts in explicitly via `smj eval run --online`.
- [Risk] The eval corpus drifts out of sync with routing-rule changes, giving false confidence → Mitigation: the routing suite runs inside `cargo test` on every build, so a rule change that the corpus does not expect fails CI immediately, forcing the corpus to be updated alongside the rule.
- [Risk] A `LoopDriver` that really drives the loop pulls daemon/provider coupling into the eval crate → Mitigation: the driver is an injected trait; the crate ships only a fake for tests and the trait definition. The real driver is wired at the `smj`/daemon boundary, keeping the crate dependency-light.
- [Risk] Case-file format churn breaks existing corpora → Mitigation: the `EvalCase` envelope is versioned (a `version` field, mirroring `LoopConfig.version`); the loader rejects unknown versions with a clear error rather than silently mis-scoring.
- [Risk] Scope creep into latency/cost benchmarking → Mitigation: explicit non-goal; the report records counts and pass rate only. Cost-budget assertions are deferred to a later change.
