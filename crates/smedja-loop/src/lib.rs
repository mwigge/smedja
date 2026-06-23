//! The smedja loop engine — a bounded, multi-role pipeline over a work envelope.
//!
//! [`engine::drive`] owns the deterministic control flow (state machine, retry
//! bound, verification gate, policy/evaluator integrity checks, failure mining)
//! and delegates running a role's agent session and persisting loop status to
//! the caller via the [`engine::RoleRunner`] and [`engine::StatusSink`] traits.
//! `smdjad`'s `loop.run` handler supplies those implementations; the deterministic
//! core stays here so it is unit-testable without the daemon.

pub mod config;
pub mod engine;
pub mod mining;
pub mod role;
pub mod state;
pub mod telemetry;
pub mod verify;

pub use config::LoopConfig;
pub use engine::{drive, LoopOutcome, RoleRunner, StatusSink};
pub use role::{DataAccess, LoopRole, Runner, Tier};
pub use state::LoopState;
