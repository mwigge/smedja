//! Turn lifecycle events and their shared correlation context.
//!
//! This module is split into two cohesive submodules:
//!
//! * [`correlation`] — the [`CorrelationCtx`] struct embedded (via
//!   `#[serde(flatten)]`) in every event variant.
//! * [`turn_event`] — the [`TurnEvent`] enum and its constructors.
//!
//! Both types are re-exported here so the public path (`crate::event::…`)
//! is byte-for-byte identical to the historical single-file layout.

mod correlation;
mod turn_event;

pub use correlation::CorrelationCtx;
pub use turn_event::TurnEvent;
