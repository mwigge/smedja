//! Daemon-side glue between the `smedja-loop` engine and smdjad's turn machinery.
//!
//! [`run`] loads `.smedja/loop.json`, reads the pending slices from the change's
//! `tasks.md`, and drives [`smedja_loop::drive`] with two daemon-backed callbacks:
//! [`role_runner::LoopRoleRunner`] (spawns a real role session per slice via the
//! turn orchestrator) and [`status_sink::LoopStatusSink`] (persists loop status
//! through the ingot). The deterministic pipeline lives in the engine crate;
//! this module only supplies the side effects.
//!
//! The module is split into cohesive submodules:
//! - [`status_sink`]: the `LoopStatusSink` status callback.
//! - [`role_runner`]: the `LoopRoleRunner` per-slice turn callback.
//! - [`tasks`]: workspace-bounded `tasks.md` resolution and pending-slice reads.
//! - [`run`]: the [`run`] entry point that drives a fresh loop.
//! - [`resume`]: the [`resume`] entry point that re-enters from a checkpoint.

mod resume;
mod role_runner;
mod run;
mod status_sink;
mod tasks;

pub(crate) use resume::resume;
pub(crate) use run::run;
