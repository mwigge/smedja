//! Runtime pool of available LLM providers, indexed by (Runner, Tier).

mod detection;
mod pool;
#[cfg(test)]
mod tests;
mod types;

pub use detection::build_provider_pool;
pub use pool::{tier_compatible, ProviderPool};
pub use types::{model_default, LocalControl, ProviderEntry};

#[cfg(test)]
pub(crate) use detection::{
    claude_preferred_runner, codex_preferred_runner, gemini_preferred_runner, kimi_preferred_runner,
};
