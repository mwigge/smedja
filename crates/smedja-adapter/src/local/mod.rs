//! Local rs-llmctl adapter — OpenAI-compatible endpoint, health-checked at startup.
//!
//! Reads `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`) and performs
//! a capability pre-flight against `GET /v1/models` before the first turn runs.
//!
//! smedja **orchestrates** the external local-serving tools (rs-llmctl for
//! install/inventory, a llama-swap-compatible proxy for hot-swap); it does not
//! reimplement an inference server, download weights, or place models on GPUs.

mod inventory;
mod metrics;
mod provider;
mod types;

pub use inventory::{fetch_inventory, install_model, parse_model_inventory};
pub use metrics::record_local_swap;
pub use provider::{issue_swap_request, LocalProvider};
pub use types::{InstallOutcome, LocalCapability, LocalModel, SwapOutcome};
