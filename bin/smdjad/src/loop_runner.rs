//! Daemon-side glue between the `smedja-loop` engine and smdjad's turn machinery.
//!
//! [`run`] loads `.smedja/loop.json`, reads the pending slices from the change's
//! `tasks.md`, and drives [`smedja_loop::drive`] with two daemon-backed callbacks:
//! [`LoopRoleRunner`] (spawns a real role session per slice via the turn
//! orchestrator) and [`LoopStatusSink`] (persists loop status through the ingot).
//! The deterministic pipeline lives in the engine crate; this module only
//! supplies the side effects.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::Dispatcher;
use smedja_ingot::{IngotHandle, Session, Task};
use smedja_loop::{LoopConfig, LoopRole, LoopState, RoleRunner, StatusSink, Tier};
use smedja_types::Timestamp;
use smedja_vault::Vault;
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use crate::cowork::CoworkGate;
use crate::orchestrator::TurnOrchestrator;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

/// Live model ids for the owner's loop×tier mapping (confirmed against the
/// current catalog): planner=Fable 5, implementer=Sonnet 5 (complex) /
/// Haiku 4.5 (simple), reviewer=Opus 4.8.
const MODEL_PLAN: &str = "claude-fable-5";
const MODEL_IMPL_COMPLEX: &str = "claude-sonnet-5";
const MODEL_IMPL_SIMPLE: &str = "claude-haiku-4-5";
const MODEL_REVIEW: &str = "claude-opus-4-8";

/// The directive the planner turn works from when no `tasks.md` exists yet.
const PLAN_DIRECTIVE: &str =
    "Decompose the umbrella intent into a coarse slice list and write it to \
     tasks.md, one `- [ ] ` line per slice.";

/// Built-in model for a loop phase, keyed by role name and routed tier.
///
/// This is the 4-slot tier→model binding of the owner's loop×tier vision:
/// planner→Fable, implementer/fix→Sonnet (Deep/complex) or Haiku (cheaper/simple)
/// keyed by the role's routed tier, reviewer→Opus. Roles outside the pipeline
/// (orchestrator, proposer, tester) return `None` and keep the pool default.
#[must_use]
fn builtin_model_for(role_name: &str, tier: Tier) -> Option<&'static str> {
    match role_name {
        "plan" => Some(MODEL_PLAN),
        "reviewer" => Some(MODEL_REVIEW),
        "implementer" | "fix" => Some(match tier {
            // Deep tier ⇒ a complex slice ⇒ the stronger Sonnet; a cheaper tier
            // ⇒ a simple slice ⇒ Haiku. This is also the axis the failure-escalation
            // ladder climbs, so an escalated fix turn upgrades Haiku → Sonnet.
            Tier::Deep => MODEL_IMPL_COMPLEX,
            _ => MODEL_IMPL_SIMPLE,
        }),
        _ => None,
    }
}

/// Binds a phase role to the owner's model, returning a role whose `model` field
/// carries the resolved id.
///
/// An explicit `model` in `loop.json` always wins. Otherwise the built-in
/// mapping is resolved through the existing `SMEDJA_MODEL_<RUNNER>_<TIER>` env
/// override ([`crate::provider_pool::model_default`]) so a newly released model
/// can be swapped in without a recompile. The binding is via `LoopRole::model`
/// and leaves `runner`/`tier` untouched, so evaluator separation (a runner-level
/// check) is unaffected.
#[must_use]
fn bind_role_model(role: &LoopRole) -> LoopRole {
    if role.model.is_some() {
        return role.clone();
    }
    let Some(builtin) = builtin_model_for(&role.name, role.tier) else {
        return role.clone();
    };
    let runner_name = crate::common::runner_session_key(role.runner);
    let mut bound = role.clone();
    bound.model = Some(crate::provider_pool::model_default(
        runner_name,
        role.tier,
        builtin,
    ));
    bound
}

/// Persists loop progression through the ingot `loops` table.
pub(crate) struct LoopStatusSink {
    ingot: IngotHandle,
    loop_id: String,
}

impl StatusSink for LoopStatusSink {
    async fn set_status(&self, state: &LoopState) {
        if let Err(e) = self
            .ingot
            .update_loop_status(&self.loop_id, state.as_str(), Timestamp::now())
            .await
        {
            warn!(loop_id = %self.loop_id, error = %e, "failed to persist loop status");
        }
    }

    async fn set_slice(&self, slice: i64) {
        if let Err(e) = self
            .ingot
            .update_loop_slice(&self.loop_id, slice, Timestamp::now())
            .await
        {
            warn!(loop_id = %self.loop_id, error = %e, "failed to persist loop slice");
        }
    }
}

/// Runs each loop role as a real agent turn on the role's configured runner.
pub(crate) struct LoopRoleRunner {
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<std::collections::HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn crate::embedder_port::Embedder>,
    provider_sessions: crate::orchestrator::ProviderSessions,
    cache_aligners: crate::orchestrator::CacheAligners,
    lsp_manager: Arc<smedja_lsp::LspManager>,
    workspace_root: PathBuf,
    agent_timeout_s: u64,
    /// Per-turn tool-call cap forwarded from `LoopConfig.limits.max_tool_turns`.
    max_tool_turns: u32,
    /// Umbrella change name — the namespace key (`umbrella:<change_name>`) the
    /// slice resolves its umbrella detail from, and the umbrella whose paste this
    /// slice avoids restating.
    change_name: String,
    /// The umbrella intent (`proposal.md`) — sealed into the cached prefix when
    /// assembling each slice's hybrid context.
    umbrella_intent: String,
    /// The umbrella intent (`proposal.md`) + design detail (`design.md`) the
    /// slice would otherwise paste in full; the lean-spec saving is measured
    /// against this paste.
    umbrella_paste: String,
}

impl RoleRunner for LoopRoleRunner {
    async fn run_role(
        &self,
        role: &LoopRole,
        slice_index: usize,
        slice: &str,
    ) -> anyhow::Result<()> {
        // Bind the phase to the owner's model (env-overridable) before the turn
        // runs, so the routed session carries the right model id (fable-plan /
        // sonnet-or-haiku-implement / opus-review).
        let bound = bind_role_model(role);
        let role = &bound;
        let now = Timestamp::now();
        let session_id = Uuid::new_v4();

        // The role's configured runner becomes the session runner override so the
        // turn orchestrator routes this role to that backend. read_only is carried
        // on the role; its enforcement is owned by the methodology gates.
        let session = Session {
            id: session_id,
            created_at: now,
            updated_at: now,
            status: "active".to_owned(),
            task_id: None,
            mode: Some(role.name.clone()),
            title: String::new(),
            cowork_mode: false,
            workspace_root: Some(self.workspace_root.display().to_string()),
            model_override: role.model.clone(),
            runner_override: Some(crate::common::runner_session_key(role.runner).to_owned()),
        };
        if let Err(e) = self.ingot.create_session(session).await {
            anyhow::bail!("role '{}': failed to create session: {e}", role.name);
        }

        let task_id = Uuid::new_v4();
        let task = Task {
            id: task_id,
            title: slice.to_owned(),
            description: String::new(),
            status: "planned".to_owned(),
            created_at: now,
            session_id: Some(session_id.to_string()),
            response: None,
        };
        if let Err(e) = self.ingot.create_task(task).await {
            anyhow::bail!("role '{}': failed to create task: {e}", role.name);
        }

        // Drive the turn directly and await it, bounded by the role timeout.
        let orchestrator = TurnOrchestrator::new(
            self.ingot.clone(),
            Arc::clone(&self.dispatcher),
            Arc::clone(&self.gates),
            Arc::clone(&self.pool),
            Arc::clone(&self.assayer),
            Arc::clone(&self.price_table),
            Arc::clone(&self.vault),
            Arc::clone(&self.embedder),
            Arc::clone(&self.provider_sessions),
            Arc::clone(&self.cache_aligners),
            Some(self.change_name.clone()),
            Arc::clone(&self.lsp_manager),
        )
        .cap_tool_turns(self.max_tool_turns);
        // Safety: `orchestrator.run()` uses structured concurrency — `drain_stream`
        // internally awaits without spawning background tasks, so dropping the
        // future when the outer `timeout` fires cancels all inner awaits cleanly.
        // No explicit `abort()` is required.
        let run = orchestrator.run(session_id.to_string(), task_id.to_string());
        if tokio::time::timeout(std::time::Duration::from_secs(self.agent_timeout_s), run)
            .await
            .is_err()
        {
            anyhow::bail!(
                "role '{}' timed out after {}s",
                role.name,
                self.agent_timeout_s
            );
        }

        // The turn marks the task "failed" on any error; treat that as role failure.
        let status = self
            .ingot
            .get_task(&task_id.to_string())
            .await
            .ok()
            .flatten()
            .map_or_else(|| "unknown".to_owned(), |t| t.status);
        if status == "failed" {
            anyhow::bail!("role '{}' turn failed", role.name);
        }

        // Lean-spec self-measurement: this slice referenced its umbrella rather
        // than pasting it in full. Best-effort; never fails the role.
        self.record_slice_saving(slice_index, slice, session_id.to_string())
            .await;
        Ok(())
    }

    async fn run_plan(&self, role: &LoopRole, existing: &[String]) -> anyhow::Result<Vec<String>> {
        // Behavior-compatible refresh: a tasks.md that already lists pending
        // slices is kept as-is — the plan phase never overwrites an authored
        // slice list. Only a fresh, empty tasks.md triggers a planner turn.
        if !existing.is_empty() {
            return Ok(existing.to_vec());
        }
        // Run the planner turn at the plan tier (bound to fable in run_role) to
        // decompose the umbrella into tasks.md, then re-read the pending slices.
        self.run_role(role, 0, PLAN_DIRECTIVE).await?;
        Ok(read_pending_slices(&self.workspace_root, &self.change_name).await)
    }
}

impl LoopRoleRunner {
    /// Records this slice's lean-spec saving (umbrella referenced, not pasted).
    ///
    /// Assembles the hybrid slice context — umbrella intent sealed into the
    /// cached prefix, umbrella detail recalled per slice from the
    /// `umbrella:<change_name>` vault namespace — and records
    /// `paste − retrieved` on the tokens-saved ledger tagged `source=lean-spec`.
    /// A dangling umbrella yields an empty recall (saving = full paste) rather
    /// than an error. A no-op when no umbrella was preloaded.
    async fn record_slice_saving(&self, slice_index: usize, slice: &str, session_id: String) {
        if self.umbrella_paste.trim().is_empty() {
            return;
        }

        // Flag (best-effort) a slice that copies the umbrella's Why/design
        // instead of carrying only its own delta — the saving regresses then.
        if crate::lean_spec::slice_restates_intent(&self.umbrella_intent, slice) {
            warn!(
                change = %self.change_name,
                "lean-spec: slice restates umbrella intent; the saving regresses"
            );
        }

        #[allow(clippy::cast_possible_truncation)] // slice ordinals never exceed u32::MAX
        let pointer =
            crate::lean_spec::SlicePointer::new(self.change_name.clone(), (slice_index + 1) as u32);

        // Persist the slice→umbrella pointer (vault payload convention) and warn
        // if the pointer dangles — no umbrella chunks resolve for it.
        let _ = crate::lean_spec::store_slice_pointer(&self.vault, &self.embedder, &pointer).await;
        let resolved = crate::lean_spec::resolve_umbrella(
            &self.vault,
            &self.embedder,
            &pointer.umbrella_id,
            slice,
            crate::lean_spec::default_slice_recall_k(),
        )
        .await;
        if resolved.is_empty() {
            warn!(
                change = %self.change_name,
                slice_n = pointer.slice_n,
                "lean-spec: umbrella pointer resolves to no chunks; using full paste as the baseline"
            );
        }

        let assembled = crate::lean_spec::assemble_slice_context(
            &self.vault,
            &self.embedder,
            &pointer,
            &self.umbrella_intent,
            slice,
            crate::lean_spec::default_slice_recall_k(),
        )
        .await;
        tracing::debug!(
            change = %self.change_name,
            slice_n = pointer.slice_n,
            detail_chunks = assembled.detail_chunks,
            "lean-spec: assembled hybrid slice context"
        );

        // The retrieved detail is the mutable-window content beyond the slice
        // delta — what cold recall re-sent in lieu of the full umbrella paste.
        let retrieved_text = assembled
            .memory
            .mutable_window()
            .iter()
            .filter(|m| m.content != slice)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        crate::lean_spec::record_lean_spec_saving(
            &self.ingot,
            &session_id,
            &self.umbrella_paste,
            &retrieved_text,
        )
        .await;
    }
}

/// Resolves the change's `tasks.md` path, returning it only when it canonically
/// resolves inside `workspace_root`.
///
/// Canonicalising the change directory catches symlink escapes and a
/// non-canonical `SMEDJA_WORKSPACE` that the `..`/`/` name check alone would
/// miss. Returns `None` when the change directory is absent (no work to do) or
/// the resolved path would escape the workspace.
fn safe_tasks_path(workspace_root: &Path, change_name: &str) -> Option<PathBuf> {
    let ws_canon = workspace_root.canonicalize().ok()?;
    let change_dir = ws_canon.join("openspec").join("changes").join(change_name);
    // Canonicalise the change directory (the file itself may be absent) and
    // assert it stays within the workspace root.
    let dir_canon = change_dir.canonicalize().ok()?;
    if !dir_canon.starts_with(&ws_canon) {
        tracing::warn!(
            change = change_name,
            "loop.run: tasks path escapes the workspace root; refusing to read it"
        );
        return None;
    }
    Some(dir_canon.join("tasks.md"))
}

/// Reads the pending slices (`- [ ] ` lines) from the change's `tasks.md`.
///
/// Returns an empty vector when the file is absent or the path would escape the
/// workspace — a loop with no readable pending work completes immediately.
async fn read_pending_slices(workspace_root: &Path, change_name: &str) -> Vec<String> {
    let Some(tasks_path) = safe_tasks_path(workspace_root, change_name) else {
        return Vec::new();
    };
    match tokio::fs::read_to_string(&tasks_path).await {
        Ok(content) => content
            .lines()
            .filter(|l| l.starts_with("- [ ] "))
            .map(|l| l.trim_start_matches("- [ ] ").to_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Drives a loop run to a terminal state.
///
/// Loads `.smedja/loop.json`; a missing or invalid policy marks the loop
/// `failed` and returns. Otherwise the engine drives the pipeline — policy-hash
/// verification, evaluator separation, per-slice role execution with the
/// verification gate and bounded fix retries — persisting status through the
/// ingot as it goes.
#[allow(clippy::too_many_arguments)] // forwards the turn-orchestrator dependencies
#[tracing::instrument(skip(ingot, dispatcher, gates, pool, assayer, price_table, vault, embedder, provider_sessions, cache_aligners, lsp_manager, workspace_root), fields(loop_id = %loop_id, change = %change_name))]
pub(crate) async fn run(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<std::collections::HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn crate::embedder_port::Embedder>,
    provider_sessions: crate::orchestrator::ProviderSessions,
    cache_aligners: crate::orchestrator::CacheAligners,
    lsp_manager: Arc<smedja_lsp::LspManager>,
    loop_id: String,
    change_name: String,
    workspace_root: PathBuf,
) {
    let policy_path = workspace_root.join(".smedja").join("loop.json");

    let config = match LoopConfig::from_file(&policy_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                loop_id = %loop_id,
                path = %policy_path.display(),
                error = %e,
                "loop.run: .smedja/loop.json missing or invalid; marking loop failed",
            );
            let _ = ingot
                .update_loop_status(&loop_id, LoopState::Failed.as_str(), Timestamp::now())
                .await;
            return;
        }
    };

    let slices = read_pending_slices(&workspace_root, &change_name).await;

    // Lean-spec umbrella preload (umbrella-once): chunk the change's design
    // detail into the `umbrella:<change_name>` vault namespace so each slice can
    // recall it on demand instead of restating it. The intent + detail paste is
    // captured so per-slice savings can be measured. Best-effort: a missing or
    // unreadable umbrella degrades to "no umbrella context" and zero savings.
    let (umbrella_intent, umbrella_paste) = if let Some(change_dir) =
        safe_tasks_path(&workspace_root, &change_name)
            .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        let (intent, detail) = crate::lean_spec::read_umbrella_sources(&change_dir).await;
        if let Err(e) =
            crate::lean_spec::preload_umbrella(&vault, &embedder, &change_name, &detail).await
        {
            warn!(change = %change_name, error = %e, "lean-spec umbrella preload failed; continuing");
        }
        let paste = format!("{intent}\n{detail}");
        (intent, paste)
    } else {
        (String::new(), String::new())
    };

    let sink = LoopStatusSink {
        ingot: ingot.clone(),
        loop_id,
    };
    let runner = LoopRoleRunner {
        ingot,
        dispatcher,
        gates,
        pool,
        assayer,
        price_table,
        vault,
        embedder,
        provider_sessions,
        cache_aligners,
        lsp_manager,
        workspace_root: workspace_root.clone(),
        agent_timeout_s: config.limits.agent_timeout_s,
        max_tool_turns: config.limits.max_tool_turns.unwrap_or(50),
        change_name: change_name.clone(),
        umbrella_intent,
        umbrella_paste,
    };

    // The engine persists every transition (including the terminal state) through
    // the sink, so there is nothing more to record here.
    let _ = smedja_loop::drive(
        &config,
        &workspace_root,
        &policy_path,
        &change_name,
        &slices,
        &runner,
        &sink,
        0,
        // All slices share this one workspace tree (no per-slice worktree), so the
        // engine must run them serially and checkpoint per slice.
        true,
    )
    .await;
}

/// Re-enters the loop engine from the last checkpointed slice.
///
/// Reads `.smedja/loop-state.json`, verifies the policy hash has not changed,
/// then calls `drive()` starting at the checkpoint's `slice_index`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn resume(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<std::collections::HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn crate::embedder_port::Embedder>,
    provider_sessions: crate::orchestrator::ProviderSessions,
    cache_aligners: crate::orchestrator::CacheAligners,
    lsp_manager: Arc<smedja_lsp::LspManager>,
    loop_id: String,
    change_name: String,
    workspace_root: PathBuf,
) {
    let policy_path = workspace_root.join(".smedja").join("loop.json");
    let checkpoint_path = workspace_root.join(".smedja").join("loop-state.json");

    let checkpoint_raw = match std::fs::read_to_string(&checkpoint_path) {
        Ok(s) => s,
        Err(e) => {
            warn!(loop_id = %loop_id, error = %e, "loop.resume: no checkpoint found; marking failed");
            let _ = ingot
                .update_loop_status(
                    &loop_id,
                    smedja_loop::LoopState::Failed.as_str(),
                    Timestamp::now(),
                )
                .await;
            return;
        }
    };
    let checkpoint: smedja_loop::LoopCheckpoint = match serde_json::from_str(&checkpoint_raw) {
        Ok(c) => c,
        Err(e) => {
            warn!(loop_id = %loop_id, error = %e, "loop.resume: checkpoint parse error; marking failed");
            let _ = ingot
                .update_loop_status(
                    &loop_id,
                    smedja_loop::LoopState::Failed.as_str(),
                    Timestamp::now(),
                )
                .await;
            return;
        }
    };

    if checkpoint.change_name != change_name {
        warn!(
            loop_id = %loop_id,
            checkpoint_change = %checkpoint.change_name,
            requested_change = %change_name,
            "loop.resume: checkpoint change_name mismatch; marking failed"
        );
        let _ = ingot
            .update_loop_status(
                &loop_id,
                smedja_loop::LoopState::Failed.as_str(),
                Timestamp::now(),
            )
            .await;
        return;
    }

    let config = match LoopConfig::from_file(&policy_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(loop_id = %loop_id, error = %e, "loop.resume: loop.json invalid; marking failed");
            let _ = ingot
                .update_loop_status(
                    &loop_id,
                    smedja_loop::LoopState::Failed.as_str(),
                    Timestamp::now(),
                )
                .await;
            return;
        }
    };

    if config.policy_hash != checkpoint.policy_hash {
        warn!(
            loop_id = %loop_id,
            "loop.resume: policy hash changed since checkpoint; marking policy_tampered"
        );
        let _ = ingot
            .update_loop_status(
                &loop_id,
                smedja_loop::LoopState::PolicyTampered.as_str(),
                Timestamp::now(),
            )
            .await;
        return;
    }

    let start_slice = checkpoint.slice_index;
    let slices = read_pending_slices(&workspace_root, &change_name).await;

    let (umbrella_intent, umbrella_paste) = if let Some(change_dir) =
        safe_tasks_path(&workspace_root, &change_name)
            .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        let (intent, detail) = crate::lean_spec::read_umbrella_sources(&change_dir).await;
        if let Err(e) =
            crate::lean_spec::preload_umbrella(&vault, &embedder, &change_name, &detail).await
        {
            warn!(change = %change_name, error = %e, "lean-spec umbrella preload failed; continuing");
        }
        let paste = format!("{intent}\n{detail}");
        (intent, paste)
    } else {
        (String::new(), String::new())
    };

    let sink = LoopStatusSink {
        ingot: ingot.clone(),
        loop_id,
    };
    let runner = LoopRoleRunner {
        ingot,
        dispatcher,
        gates,
        pool,
        assayer,
        price_table,
        vault,
        embedder,
        provider_sessions,
        cache_aligners,
        lsp_manager,
        workspace_root: workspace_root.clone(),
        agent_timeout_s: config.limits.agent_timeout_s,
        max_tool_turns: config.limits.max_tool_turns.unwrap_or(50),
        change_name: change_name.clone(),
        umbrella_intent,
        umbrella_paste,
    };

    let _ = smedja_loop::drive(
        &config,
        &workspace_root,
        &policy_path,
        &change_name,
        &slices,
        &runner,
        &sink,
        start_slice,
        // Shared workspace tree — serial, per-slice-checkpointed execution.
        true,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, LoopRecord};
    use tempfile::TempDir;

    #[test]
    fn safe_tasks_path_accepts_in_workspace_and_rejects_symlink_escape() {
        let ws = TempDir::new().unwrap();
        let ws_root = ws.path().canonicalize().unwrap();

        // A normal in-workspace change resolves.
        let good = ws_root.join("openspec").join("changes").join("good");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("tasks.md"), "- [ ] 1.1 do it\n").unwrap();
        assert!(super::safe_tasks_path(&ws_root, "good").is_some());

        // A change dir that symlinks outside the workspace is rejected.
        let outside = TempDir::new().unwrap();
        let outside_canon = outside.path().canonicalize().unwrap();
        std::fs::write(outside_canon.join("tasks.md"), "- [ ] evil\n").unwrap();
        let evil_link = ws_root.join("openspec").join("changes").join("evil");
        std::os::unix::fs::symlink(&outside_canon, &evil_link).unwrap();
        assert!(
            super::safe_tasks_path(&ws_root, "evil").is_none(),
            "a symlinked change dir escaping the workspace must be refused"
        );
    }

    // ── group 4: loop consumes umbrella-once + slice-each ───────────────────

    #[tokio::test]
    async fn umbrella_tasks_md_coarse_lines_are_read_as_slice_list() {
        // Task 4.1/4.2: the umbrella's coarse `- [ ]` lines are read as the slice
        // list; each coarse group maps to exactly one slice the engine iterates.
        let ws = TempDir::new().unwrap();
        let ws_root = ws.path().canonicalize().unwrap();
        let change_dir = ws_root.join("openspec").join("changes").join("umbrella");
        std::fs::create_dir_all(&change_dir).unwrap();
        // An umbrella tasks.md lists slices coarsely — one `- [ ]` per slice, no
        // granular per-step decomposition. The `## ` headings and a `[x]` line
        // must NOT be read as slices.
        std::fs::write(
            change_dir.join("tasks.md"),
            "## Slices\n\n\
             - [ ] Slice 1: store the umbrella\n\
             - [ ] Slice 2: resolve the pointer\n\
             - [x] already done — must be skipped\n\
             - [ ] Slice 3: hybrid loading\n",
        )
        .unwrap();

        let slices = super::read_pending_slices(&ws_root, "umbrella").await;
        assert_eq!(
            slices,
            vec![
                "Slice 1: store the umbrella".to_owned(),
                "Slice 2: resolve the pointer".to_owned(),
                "Slice 3: hybrid loading".to_owned(),
            ],
            "each coarse `- [ ]` line must map to exactly one pending slice"
        );
    }

    #[tokio::test]
    async fn drive_iterates_one_role_run_per_slice() {
        // Task 4.2: each slice the engine iterates triggers exactly one
        // implementer run — the umbrella-once + slice-each cadence over the list
        // read from tasks.md.
        use smedja_loop::{drive, LoopRole, LoopState, RoleRunner, Runner, StatusSink, Tier};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingRunner {
            slices_seen: std::sync::Mutex<Vec<String>>,
        }
        impl RoleRunner for CountingRunner {
            async fn run_role(
                &self,
                _role: &LoopRole,
                _slice_index: usize,
                slice: &str,
            ) -> anyhow::Result<()> {
                self.slices_seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(slice.to_owned());
                Ok(())
            }
        }
        struct CountingSink {
            slice_calls: AtomicUsize,
        }
        impl StatusSink for CountingSink {
            async fn set_status(&self, _state: &LoopState) {}
            async fn set_slice(&self, _slice: i64) {
                self.slice_calls.fetch_add(1, Ordering::SeqCst);
            }
        }

        let dir = TempDir::new().unwrap();
        let smedja_dir = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja_dir).unwrap();
        // Empty verification command → the gate passes vacuously, so every slice
        // is driven once with no provider needed.
        let roles = r#"[{"name":"implementer","runner":"claude","tier":"deep","read_only":false,"tools":[]}]"#;
        std::fs::write(
            smedja_dir.join("loop.json"),
            format!(
                r#"{{
                    "version": 1,
                    "limits": {{"max_attempts": 3, "agent_timeout_s": 60}},
                    "roles": {roles},
                    "verification": {{"command": ""}},
                    "review": {{"per_slice": false, "required": false}},
                    "publication": {{"max_pr_lines": 400}}
                }}"#
            ),
        )
        .unwrap();
        let policy_path = smedja_dir.join("loop.json");
        let config = LoopConfig::from_file(&policy_path).expect("policy loads");

        let slices = vec![
            "slice one".to_owned(),
            "slice two".to_owned(),
            "slice three".to_owned(),
        ];
        let runner = CountingRunner {
            slices_seen: std::sync::Mutex::new(Vec::new()),
        };
        let sink = CountingSink {
            slice_calls: AtomicUsize::new(0),
        };

        let outcome = drive(
            &config,
            dir.path(),
            &policy_path,
            "umbrella",
            &slices,
            &runner,
            &sink,
            0,
            true,
        )
        .await;

        assert_eq!(
            outcome.slices_completed, 3,
            "every slice must complete once"
        );
        assert_eq!(
            *runner.slices_seen.lock().unwrap(),
            slices,
            "the engine must run exactly one role per slice, in order"
        );
        assert_eq!(
            sink.slice_calls.load(Ordering::SeqCst),
            3,
            "each slice maps to exactly one iteration"
        );
        // Tier/Runner enums are referenced to keep the import surface used.
        let _ = (Tier::Deep, Runner::Claude);
    }

    #[tokio::test]
    async fn umbrella_intent_sealed_once_across_the_slice_iteration() {
        // Task 4.3/4.4: the umbrella intent is sealed into the cached prefix
        // exactly once before the slice iteration; the prefix is not re-sealed
        // per slice, and each slice is a thin delta on the already-cached intent.
        use smedja_memory::{Message, WorkingMemory};

        let mut mem = WorkingMemory::new(usize::MAX);
        // Seal the umbrella intent once, before iterating slices.
        mem.push(Message::system("UMBRELLA INTENT (cached once)"));
        mem.seal_prefix();
        let sealed_at = mem.stable_prefix();
        assert_eq!(
            sealed_at, 1,
            "the umbrella intent is the only sealed message"
        );

        // Drive three slices: each replaces the mutable window with its own thin
        // delta. The prefix is never re-sealed.
        for n in 1..=3 {
            mem.replace_mutable(vec![Message::user(format!("slice {n} delta"))]);
            assert_eq!(
                mem.stable_prefix(),
                sealed_at,
                "the prefix must stay sealed at its original boundary, not re-sealed"
            );
            assert!(
                mem.messages()[0].content.contains("UMBRELLA INTENT"),
                "the cached umbrella intent persists across every slice"
            );
            assert_eq!(
                mem.mutable_window().len(),
                1,
                "each slice carries only its own thin delta in the mutable window"
            );
        }
    }

    // ── 4-slot tier→model binding ───────────────────────────────────────────

    #[test]
    fn builtin_model_maps_the_four_phase_slots() {
        // planner=Fable, implementer=Sonnet(complex)/Haiku(simple), reviewer=Opus.
        assert_eq!(
            super::builtin_model_for("plan", Tier::Deep),
            Some("claude-fable-5")
        );
        assert_eq!(
            super::builtin_model_for("reviewer", Tier::Fast),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            super::builtin_model_for("implementer", Tier::Deep),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            super::builtin_model_for("implementer", Tier::Local),
            Some("claude-haiku-4-5")
        );
        assert_eq!(
            super::builtin_model_for("fix", Tier::Deep),
            Some("claude-sonnet-5")
        );
        // Non-pipeline roles keep the pool default.
        assert_eq!(super::builtin_model_for("orchestrator", Tier::Deep), None);
    }

    #[test]
    fn bind_role_model_resolves_the_owner_mapping() {
        let roles = LoopRole::defaults();
        let plan = roles.iter().find(|r| r.name == "plan").unwrap();
        assert_eq!(
            super::bind_role_model(plan).model.as_deref(),
            Some("claude-fable-5"),
            "the plan phase binds to fable"
        );
        let reviewer = roles.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(
            super::bind_role_model(reviewer).model.as_deref(),
            Some("claude-opus-4-8"),
            "the review phase binds to opus"
        );
    }

    #[test]
    fn bind_role_model_respects_an_explicit_config_model() {
        let mut plan = LoopRole::defaults()
            .into_iter()
            .find(|r| r.name == "plan")
            .unwrap();
        plan.model = Some("operator-pinned-model".to_owned());
        assert_eq!(
            super::bind_role_model(&plan).model.as_deref(),
            Some("operator-pinned-model"),
            "an explicit loop.json model always wins over the builtin binding"
        );
    }

    #[test]
    fn model_binding_preserves_evaluator_separation() {
        // opus-review over haiku/sonnet-implement: the binding only touches
        // `model`, so the runner-level reviewer≠implementer separation still holds.
        let roles = LoopRole::defaults();
        let implementer =
            super::bind_role_model(roles.iter().find(|r| r.name == "implementer").unwrap());
        let reviewer = super::bind_role_model(roles.iter().find(|r| r.name == "reviewer").unwrap());
        assert!(
            reviewer.runner_differs_from(&implementer),
            "evaluator separation must survive model binding"
        );
        assert_eq!(reviewer.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(implementer.model.as_deref(), Some("claude-haiku-4-5"));
    }

    async fn deps() -> (
        IngotHandle,
        Arc<Dispatcher>,
        Arc<Mutex<std::collections::HashMap<String, Arc<CoworkGate>>>>,
        Arc<ProviderPool>,
        Arc<Assayer>,
        Arc<PriceTable>,
        Arc<Mutex<Vault>>,
    ) {
        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory ingot"));
        let dispatcher = Arc::new(Dispatcher::new(16));
        let gates = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pool = Arc::new(crate::provider_pool::build_provider_pool().await);
        let assayer = Arc::new(Assayer::default_rules());
        let price_table = Arc::new(PriceTable::embedded());
        let vault = Arc::new(Mutex::new(
            Vault::open_in_memory().expect("in-memory vault"),
        ));
        (ingot, dispatcher, gates, pool, assayer, price_table, vault)
    }

    async fn seed_loop(ingot: &IngotHandle, id: &str, change: &str) {
        ingot
            .create_loop(LoopRecord {
                id: id.to_owned(),
                change_name: change.to_owned(),
                status: "planning".to_owned(),
                current_slice: 0,
                attempt: 1,
                created_at: Timestamp::from_secs_f64(1_000.0),
                updated_at: Timestamp::from_secs_f64(1_000.0),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_loop_json_marks_loop_failed() {
        let (ingot, dispatcher, gates, pool, assayer, price_table, vault) = deps().await;
        let ws = TempDir::new().unwrap();
        seed_loop(&ingot, "loop-missing", "demo").await;

        run(
            ingot.clone(),
            dispatcher,
            gates,
            pool,
            assayer,
            price_table,
            vault,
            Arc::new(crate::embedder_port::FnvEmbedder::new()),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(smedja_lsp::LspManager::new()),
            "loop-missing".to_owned(),
            "demo".to_owned(),
            ws.path().to_path_buf(),
        )
        .await;

        let rec = ingot.get_loop("loop-missing").await.unwrap().unwrap();
        assert_eq!(rec.status, "failed");
    }

    #[tokio::test]
    async fn valid_policy_with_no_pending_slices_completes() {
        let (ingot, dispatcher, gates, pool, assayer, price_table, vault) = deps().await;
        let ws = TempDir::new().unwrap();
        // Valid loop.json with no roles → evaluator separation vacuously holds.
        let smedja_dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&smedja_dir).unwrap();
        std::fs::write(
            smedja_dir.join("loop.json"),
            r#"{
                "version": 1,
                "limits": {"max_attempts": 3, "agent_timeout_s": 60},
                "roles": [],
                "verification": {"command": "true"},
                "review": {"per_slice": false, "required": false},
                "publication": {"max_pr_lines": 400}
            }"#,
        )
        .unwrap();
        // tasks.md with no pending lines → zero slices → completes without a provider.
        let change_dir = ws.path().join("openspec").join("changes").join("demo");
        std::fs::create_dir_all(&change_dir).unwrap();
        std::fs::write(
            change_dir.join("tasks.md"),
            "## 1. Group\n\n- [x] 1.1 done\n",
        )
        .unwrap();
        seed_loop(&ingot, "loop-empty", "demo").await;

        run(
            ingot.clone(),
            dispatcher,
            gates,
            pool,
            assayer,
            price_table,
            vault,
            Arc::new(crate::embedder_port::FnvEmbedder::new()),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(smedja_lsp::LspManager::new()),
            "loop-empty".to_owned(),
            "demo".to_owned(),
            ws.path().to_path_buf(),
        )
        .await;

        let rec = ingot.get_loop("loop-empty").await.unwrap().unwrap();
        assert_eq!(rec.status, "complete");
    }
}
