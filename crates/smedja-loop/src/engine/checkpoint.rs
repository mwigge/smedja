//! Checkpoint persistence and role/label resolution helpers for the engine.

use std::path::Path;

use crate::config::LoopConfig;
use crate::role::{DataAccess, LoopRole, Runner, Tier};

use super::LoopCheckpoint;

/// Persists the loop checkpoint to `.smedja/loop-state.json` (best-effort).
///
/// A missing `.smedja` directory or a write error is ignored: the checkpoint is
/// a resume optimisation, not a correctness dependency of the current run.
pub(super) fn write_checkpoint(
    path: &Path,
    change_name: &str,
    policy_hash: &str,
    slice_index: usize,
    slices_completed: u64,
) {
    let ckpt = LoopCheckpoint {
        change_name: change_name.to_owned(),
        policy_hash: policy_hash.to_owned(),
        slice_index,
        slices_completed,
    };
    if let Ok(json) = serde_json::to_string(&ckpt) {
        let _ = std::fs::write(path, json);
    }
}

/// Resolves a role by name from the config, falling back to the default table,
/// then to a deny-all local role.
pub(super) fn resolve_role(config: &LoopConfig, name: &str) -> LoopRole {
    if let Some(r) = config.roles.iter().find(|r| r.name == name) {
        return r.clone();
    }
    LoopRole::defaults()
        .into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| LoopRole {
            name: name.to_owned(),
            runner: Runner::Local,
            tier: Tier::Local,
            model: None,
            read_only: false,
            tools: vec![],
            role_id: uuid::Uuid::nil(),
            data_access: DataAccess::default(),
            resume_session_id: None,
        })
}

/// String label for a runner, for telemetry attributes.
pub(super) fn runner_label(runner: Runner) -> &'static str {
    match runner {
        Runner::Claude => "claude",
        Runner::Codex => "codex",
        Runner::Local => "local",
        Runner::Copilot => "copilot",
        Runner::Minimax => "minimax",
        Runner::Berget => "berget",
        Runner::Pool => "pool",
    }
}

/// String label for a tier, for telemetry attributes.
pub(super) fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Local => "local",
        Tier::Deep => "deep",
    }
}
