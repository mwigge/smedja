//! `smedja-spec` — the native in-daemon OpenSpec engine.
//!
//! This crate replaces the TUI-only shell-out to an external `openspec` binary
//! with a self-contained engine every runner shares. It owns the OpenSpec file
//! model:
//!
//! - `openspec/specs/<capability>/spec.md` — the source of truth.
//! - `openspec/changes/<name>/{proposal,design,tasks}.md` — a change's artifacts.
//! - `openspec/changes/<name>/specs/<capability>/spec.md` — a change's delta.
//! - `openspec/changes/archive/<name>/` — completed changes.
//!
//! The keystone primitive is the delta parser/merger in [`parse`]: it turns the
//! `## ADDED / MODIFIED / REMOVED Requirements` markdown surface into the typed
//! [`Delta`] model and back, and [`SpecEngine::archive`] merges those deltas into
//! the source specs. [`SpecEngine`] exposes the operations `create_change`,
//! `write_delta`, `validate`, `show`, `diff`, `list_changes`, `archive`, and
//! `status`, plus the `tasks.md` slice reader the loop routes through.

pub mod engine;
pub mod model;
pub mod parse;

pub use engine::{ArchiveOutcome, ChangeStatus, Result, SpecEngine, SpecError, ValidationReport};
pub use model::{Delta, DeltaOp, Requirement, Scenario, Spec};
pub use parse::{
    parse_delta, parse_pending_slices, parse_spec, render_delta, render_requirement, render_spec,
    task_counts,
};
