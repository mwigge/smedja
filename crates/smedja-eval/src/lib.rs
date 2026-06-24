//! `smedja-eval` — a Rust-native, CI-runnable eval harness for smedja.
//!
//! The harness defines an [`case::EvalCase`] format authored as data and loaded
//! from a suite directory, a pluggable [`scoring::Scorer`] trait with three
//! concrete strategies (`ExactMatch`, `Deterministic`, `Rubric`), a run
//! [`engine`] that aggregates verdicts into an [`report::EvalReport`], and eval
//! [`telemetry`] mirroring the `smedja-loop` metric conventions.
//!
//! Two surfaces are evaluated:
//!
//! - **Routing** — labelled `(role, complexity)` inputs scored by exact match
//!   against [`smedja_assayer::Assayer::route_decision`]. Deterministic and
//!   model-free; runs inside `cargo test`.
//! - **Agent / loop** — change scenarios driven through an injected loop driver
//!   and scored with deterministic predicates and/or an injected LLM judge.
//!   Non-deterministic and opt-in.
//!
//! Side-effecting collaborators (route evaluator, loop driver, judge) are
//! injected behind traits so the crate carries no daemon or provider coupling
//! and the aggregation logic is unit-testable with fakes.

pub mod case;
pub mod engine;
pub mod report;
pub mod scoring;
pub mod telemetry;
