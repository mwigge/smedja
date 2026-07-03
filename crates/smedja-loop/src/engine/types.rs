//! Engine boundary traits and value types.
//!
//! [`RoleRunner`] and [`StatusSink`] are the caller-supplied seams that keep the
//! daemon's provider/session/DB coupling out of the engine; [`LoopOutcome`] and
//! [`LoopCheckpoint`] are the values the engine produces and persists.

use std::future::Future;

/// Executes a single loop role — the daemon spawns an agent session for it.
pub trait RoleRunner {
    /// Runs `role` for the slice at `slice_index` with text `slice`.
    ///
    /// Returns `Ok(())` when the role's session completed and `Err` when it
    /// could not be executed. A review verdict is modelled as execution success;
    /// the deterministic pass/fail signal comes from the verification gate.
    fn run_role(
        &self,
        role: &crate::role::LoopRole,
        slice_index: usize,
        slice: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Persists loop progression — status transitions and the slice counter.
pub trait StatusSink {
    /// Records the current [`LoopState`](crate::state::LoopState).
    fn set_status(&self, state: &crate::state::LoopState) -> impl Future<Output = ()> + Send;
    /// Records the current 1-based slice counter.
    fn set_slice(&self, slice: i64) -> impl Future<Output = ()> + Send;
}

/// Outcome of driving a loop to a terminal state.
#[derive(Debug, Clone)]
pub struct LoopOutcome {
    /// The terminal [`LoopState`](crate::state::LoopState) reached.
    pub final_state: crate::state::LoopState,
    /// Number of slices that passed verification.
    pub slices_completed: u64,
}

/// Checkpoint persisted to `.smedja/loop-state.json` before each slice runs.
///
/// Used by `loop.resume` to re-enter the pipeline at the last started slice.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct LoopCheckpoint {
    pub change_name: String,
    pub policy_hash: String,
    pub slice_index: usize,
    pub slices_completed: u64,
}
