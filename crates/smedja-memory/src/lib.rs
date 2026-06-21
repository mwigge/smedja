//! Working memory for smedja agent sessions.
//!
//! Provides [`WorkingMemory`] — the ordered message store for a single session —
//! together with a stable-prefix KV-cache guard, hot/warm/cold retention strata,
//! and a naive token-budget estimator.

pub mod budget;
pub mod error;
pub mod memory;
pub mod types;

pub use budget::{estimate_messages_tokens, estimate_tokens};
pub use error::MemoryError;
pub use memory::{
    detect_agents_md, load_workspace_skills, StrataConfig, WorkingMemory, HOT_WINDOW, WARM_WINDOW,
};
pub use types::{Message, Role, Stratum};
