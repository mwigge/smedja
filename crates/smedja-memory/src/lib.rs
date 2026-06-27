//! Working memory for smedja agent sessions.
//!
//! Provides [`WorkingMemory`] — the ordered message store for a single session —
//! together with a stable-prefix KV-cache guard, hot/warm/cold retention strata,
//! and a naive token-budget estimator.

pub mod aligner;
pub mod budget;
pub mod cold;
pub mod error;
pub mod guides;
pub mod memory;
pub mod skills;
pub mod types;
pub mod working;

pub use aligner::{CacheAligner, CacheHint, Drift};
pub use budget::{estimate_messages_tokens, estimate_tokens};
pub use cold::{ColdResult, ColdStore};
pub use error::MemoryError;
pub use guides::write_failure_guide;
pub use memory::{
    detect_agents_md, inject_workspace_skills, load_role_skills, load_workspace_skills, ColdQuery,
    StrataConfig,
    WorkingMemory, HOT_WINDOW, WARM_WINDOW,
};
pub use types::{Message, Role, Stratum};
pub use working::inject_conciseness;
