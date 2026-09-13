//! Runtime pool of available LLM providers, indexed by (Runner, Tier).

mod detection;
mod pool;
#[cfg(test)]
mod tests;
mod types;

pub use detection::{build_provider_pool, build_provider_pool_with_keys};
pub use pool::{tier_compatible, ProviderPool};
pub use types::{model_default, LocalControl, ProviderEntry};

/// Replacing the pool swaps future requests to new providers while running turns
/// keep the immutable snapshot they started with.
pub type PoolHandle = std::sync::Arc<std::sync::RwLock<std::sync::Arc<ProviderPool>>>;

pub fn pool_snapshot(handle: &PoolHandle) -> std::sync::Arc<ProviderPool> {
    handle.read().expect("provider pool lock poisoned").clone()
}

#[cfg(test)]
pub(crate) use detection::{
    claude_preferred_runner, codex_preferred_runner, gemini_preferred_runner, kimi_preferred_runner,
};
