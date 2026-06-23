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
use smedja_loop::{LoopConfig, LoopRole, LoopState, RoleRunner, StatusSink};
use smedja_vault::Vault;
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use crate::cowork::CoworkGate;
use crate::orchestrator::TurnOrchestrator;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

/// Persists loop progression through the ingot `loops` table.
pub(crate) struct LoopStatusSink {
    ingot: IngotHandle,
    loop_id: String,
}

impl StatusSink for LoopStatusSink {
    async fn set_status(&self, state: &LoopState) {
        if let Err(e) = self
            .ingot
            .update_loop_status(&self.loop_id, state.as_str(), crate::now_epoch())
            .await
        {
            warn!(loop_id = %self.loop_id, error = %e, "failed to persist loop status");
        }
    }

    async fn set_slice(&self, slice: i64) {
        if let Err(e) = self
            .ingot
            .update_loop_slice(&self.loop_id, slice, crate::now_epoch())
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
    workspace_root: PathBuf,
    agent_timeout_s: u64,
}

impl RoleRunner for LoopRoleRunner {
    async fn run_role(
        &self,
        role: &LoopRole,
        _slice_index: usize,
        slice: &str,
    ) -> anyhow::Result<()> {
        let now = crate::now_epoch();
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
            runner_override: Some(crate::runner_session_key(role.runner).to_owned()),
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
        );
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
        Ok(())
    }
}

/// Reads the pending slices (`- [ ] ` lines) from the change's `tasks.md`.
///
/// Returns an empty vector when the file is absent — a loop with no pending work
/// completes immediately.
async fn read_pending_slices(workspace_root: &Path, change_name: &str) -> Vec<String> {
    let tasks_path = workspace_root
        .join("openspec")
        .join("changes")
        .join(change_name)
        .join("tasks.md");
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
pub(crate) async fn run(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<std::collections::HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
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
                .update_loop_status(&loop_id, LoopState::Failed.as_str(), crate::now_epoch())
                .await;
            return;
        }
    };

    let slices = read_pending_slices(&workspace_root, &change_name).await;

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
        workspace_root: workspace_root.clone(),
        agent_timeout_s: config.limits.agent_timeout_s,
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
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, LoopRecord};
    use tempfile::TempDir;

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
                created_at: 1_000.0,
                updated_at: 1_000.0,
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
            "loop-empty".to_owned(),
            "demo".to_owned(),
            ws.path().to_path_buf(),
        )
        .await;

        let rec = ingot.get_loop("loop-empty").await.unwrap().unwrap();
        assert_eq!(rec.status, "complete");
    }
}
