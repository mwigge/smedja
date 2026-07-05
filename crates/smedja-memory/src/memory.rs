//! Working memory for a single agent session: the ordered [`WorkingMemory`]
//! message store with hot/warm/cold retention strata, plus the filesystem
//! loaders that inject workspace skills, role packs, and project context.
//!
//! The module is split by concern — the [`WorkingMemory`] strata/retrieval
//! logic (`working`), its configuration types (`config`), and the workspace
//! loaders (`loaders`) — and re-exported here so all `crate::memory::*` paths
//! stay unchanged.

mod config;
mod loaders;
mod working;

#[cfg(test)]
mod tests;

pub use config::{ColdQuery, StrataConfig, HOT_WINDOW, WARM_WINDOW};
pub use loaders::{
    detect_agents_md, inject_workspace_skills, load_context_files, load_role_skills,
    load_workspace_skills, strip_managed_agents_section, AGENTS_MANAGED_BEGIN, AGENTS_MANAGED_END,
};
pub use working::WorkingMemory;
