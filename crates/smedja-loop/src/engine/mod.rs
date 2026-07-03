//! Engine — drives the bounded multi-role loop pipeline.
//!
//! The engine owns the deterministic control flow (state machine, retry bound,
//! verification gate, policy/evaluator integrity checks, failure mining) and
//! delegates the side-effecting work — running a role's agent session and
//! persisting loop status — to caller-supplied implementations of [`RoleRunner`]
//! and [`StatusSink`]. This keeps the daemon's provider/session/DB coupling out
//! of the engine crate and makes the pipeline unit-testable with fakes.
//!
//! The implementation is split across sibling submodules:
//! - [`types`] — the [`RoleRunner`]/[`StatusSink`] traits and the
//!   [`LoopOutcome`]/[`LoopCheckpoint`] value types.
//! - [`roles`] — role resolution and per-role traced execution.
//! - [`slice`] — the per-slice implement→verify→review pipeline.
//! - [`drive`] — the [`drive`] entry point and terminal telemetry.

mod drive;
mod roles;
mod slice;
mod types;

pub use drive::drive;
pub use types::{LoopCheckpoint, LoopOutcome, RoleRunner, StatusSink};
