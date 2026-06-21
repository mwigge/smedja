//! `smedja-loop` — bounded multi-role pipeline over an `OpenSpec` work envelope.
//!
//! Provides the core types, state machine states, `OTel` telemetry helpers,
//! verification gate, and failure-mining utilities for the smedja loop engine.

pub mod config;
pub mod mining;
pub mod role;
pub mod state;
pub mod telemetry;
pub mod verify;

pub use config::LoopConfig;
pub use role::{DataAccess, LoopRole, Runner, Tier};
pub use state::LoopState;
