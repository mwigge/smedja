//! [`LoopRoleRunner`]: runs each loop role as a real agent turn on the role's
//! configured runner, and records the per-slice lean-spec saving.

use std::path::PathBuf;
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::Dispatcher;
use smedja_ingot::{IngotHandle, Session, Task};
use smedja_loop::{LoopRole, RoleRunner};
use smedja_types::Timestamp;
use smedja_vault::Vault;
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use crate::cowork::CoworkGate;
use crate::orchestrator::TurnOrchestrator;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

/// Runs each loop role as a real agent turn on the role's configured runner.
pub(crate) struct LoopRoleRunner {
    pub(crate) ingot: IngotHandle,
    pub(crate) dispatcher: Arc<Dispatcher>,
    pub(crate) gates: Arc<Mutex<std::collections::HashMap<String, Arc<CoworkGate>>>>,
    pub(crate) pool: Arc<ProviderPool>,
    pub(crate) assayer: Arc<Assayer>,
    pub(crate) price_table: Arc<PriceTable>,
    pub(crate) vault: Arc<Mutex<Vault>>,
    pub(crate) embedder: Arc<dyn crate::embedder_port::Embedder>,
    pub(crate) provider_sessions: crate::orchestrator::ProviderSessions,
    pub(crate) cache_aligners: crate::orchestrator::CacheAligners,
    pub(crate) lsp_manager: Arc<smedja_lsp::LspManager>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) agent_timeout_s: u64,
    /// Per-turn tool-call cap forwarded from `LoopConfig.limits.max_tool_turns`.
    pub(crate) max_tool_turns: u32,
    /// Umbrella change name — the namespace key (`umbrella:<change_name>`) the
    /// slice resolves its umbrella detail from, and the umbrella whose paste this
    /// slice avoids restating.
    pub(crate) change_name: String,
    /// The umbrella intent (`proposal.md`) — sealed into the cached prefix when
    /// assembling each slice's hybrid context.
    pub(crate) umbrella_intent: String,
    /// The umbrella intent (`proposal.md`) + design detail (`design.md`) the
    /// slice would otherwise paste in full; the lean-spec saving is measured
    /// against this paste.
    pub(crate) umbrella_paste: String,
}

impl RoleRunner for LoopRoleRunner {
    async fn run_role(
        &self,
        role: &LoopRole,
        slice_index: usize,
        slice: &str,
    ) -> anyhow::Result<()> {
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
