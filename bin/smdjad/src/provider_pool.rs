//! Runtime pool of available LLM providers, indexed by (Runner, Tier).

mod build;
mod local_control;
mod model;
pub(crate) mod pool;
mod tier;

pub use build::build_provider_pool;
pub use local_control::LocalControl;
pub use model::model_default;
pub use pool::{ProviderEntry, ProviderPool};
pub use tier::tier_compatible;
