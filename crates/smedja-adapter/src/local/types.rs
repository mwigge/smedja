//! Data types for the local adapter: model inventory, capability snapshot, and
//! swap/install outcomes.

/// A single model offered by the local swap proxy's `/v1/models` inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalModel {
    /// The `id` field of the model entry.
    pub id: String,
    /// Estimated VRAM footprint in MiB, when the inventory metadata exposes it.
    pub est_vram_mb: Option<u64>,
}

/// Capability snapshot returned by the health check.
///
/// Holds the full `/v1/models` inventory (not just the first id) plus the
/// currently-active model id, so the picker can list every servable model and
/// the swap path can update the active selection in place.
#[derive(Debug, Clone)]
pub struct LocalCapability {
    /// Every model entry returned by `GET /v1/models`, in response order.
    pub inventory: Vec<LocalModel>,
    /// The currently-active model id (the first inventory entry at connect time),
    /// or `None` when the inventory is empty.
    pub active_model_id: Option<String>,
    /// `true` when the health check succeeded within the 500 ms timeout.
    pub healthy: bool,
}

/// Outcome of a [`crate::LocalProvider::swap_model`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapOutcome {
    /// The model id now active after the swap.
    pub active_model_id: String,
    /// `true` when the explicit swap endpoint accepted the request; `false` when
    /// the label-only fallback path was taken (the proxy routes on the request
    /// `model` field instead of an explicit swap endpoint).
    pub explicit_swap: bool,
}

/// Result of an install orchestration: the installer's exit and the post-install
/// verification against the live inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// `true` only when the installer succeeded **and** the model afterwards
    /// appears in `/v1/models`.
    pub installed: bool,
    /// Whether the installer process exited zero.
    pub installer_ok: bool,
    /// Whether the model is present in the live inventory after the install.
    pub present_in_inventory: bool,
    /// The installer's combined stdout/stderr tail (for surfacing to the operator).
    pub log_tail: String,
}
