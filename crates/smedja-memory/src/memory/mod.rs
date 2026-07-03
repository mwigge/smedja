//! Working memory for a single agent session.
//!
//! Splits into cohesive submodules:
//! - [`working_memory`] — the [`WorkingMemory`] message store, its stable-prefix
//!   KV-cache guard, strata-aware prompt assembly, and cold recall.
//! - [`strata`] — the [`StrataConfig`] hot/warm/cold boundary presets.
//! - [`loaders`] — workspace skill/context/role loaders and injection helpers.

mod loaders;
mod strata;
mod working_memory;

pub use loaders::{
    detect_agents_md, inject_workspace_skills, load_context_files, load_role_skills,
    load_workspace_skills,
};
pub use strata::StrataConfig;
pub use working_memory::{ColdQuery, WorkingMemory};

/// Hot window size: the last `HOT_WINDOW` turns are always included verbatim.
pub const HOT_WINDOW: usize = 5;

/// Warm window size: turns within `WARM_WINDOW` positions from the end are
/// included in context when the token budget allows, after the hot window.
pub const WARM_WINDOW: usize = 30;
