//! Types for the loop pipeline — execution logic lives in smdjad's loop.run RPC handler.
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
