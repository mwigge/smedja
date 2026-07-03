//! Control-plane state for the `local` runner.

use std::sync::Mutex;

use smedja_adapter::{GpuSnapshot, LocalModel};

/// Control-plane state for the `local` runner: the swap-proxy endpoint, the full
/// model inventory, the cached GPU snapshot, and the active-model selection.
///
/// The active-model id lives behind a [`Mutex`] so `local.swap` can update it in
/// place — atomically, without rebuilding the pool's `stream_chat` provider —
/// while concurrent turns keep the model they started with.
pub struct LocalControl {
    /// OpenAI-compatible base endpoint (`SMEDJA_LOCAL_ENDPOINT`); re-queried for
    /// `/v1/models` after an install to verify the model is servable.
    pub endpoint: String,
    /// Swap-proxy endpoint (`SMEDJA_LOCAL_SWAP_ENDPOINT`) the hot-swap targets.
    pub swap_endpoint: String,
    /// Full `/v1/models` inventory captured at connect time.
    pub inventory: Vec<LocalModel>,
    /// Advisory GPU snapshot captured at startup; refreshed on demand by `local.gpu`.
    pub gpu: GpuSnapshot,
    /// The active local model id, mutated in place by a hot-swap.
    active_model_id: Mutex<Option<String>>,
}

impl LocalControl {
    /// Builds a control plane from its captured parts.
    #[must_use]
    pub fn new(
        endpoint: String,
        swap_endpoint: String,
        inventory: Vec<LocalModel>,
        gpu: GpuSnapshot,
        active_model_id: Option<String>,
    ) -> Self {
        Self {
            endpoint,
            swap_endpoint,
            inventory,
            gpu,
            active_model_id: Mutex::new(active_model_id),
        }
    }

    /// Returns the currently-active local model id, if any.
    ///
    /// # Panics
    ///
    /// Panics only if the active-model lock was poisoned by a prior panic while
    /// holding it — a non-recoverable invariant violation.
    #[must_use]
    pub fn active_model_id(&self) -> Option<String> {
        self.active_model_id
            .lock()
            .expect("local active-model lock poisoned")
            .clone()
    }

    /// Atomically sets the active local model id, returning the previous value.
    ///
    /// Called by `local.swap` after the swap proxy accepts the request; no
    /// provider object is rebuilt.
    ///
    /// # Panics
    ///
    /// Panics only if the active-model lock was poisoned by a prior panic while
    /// holding it — a non-recoverable invariant violation.
    pub fn set_active_model_id(&self, model_id: &str) -> Option<String> {
        let mut guard = self
            .active_model_id
            .lock()
            .expect("local active-model lock poisoned");
        guard.replace(model_id.to_owned())
    }
}
