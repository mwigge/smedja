//! [`run`]: drives a fresh loop to a terminal state.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_loop::{LoopConfig, LoopState};
use smedja_types::Timestamp;
use smedja_vault::Vault;
use tokio::sync::Mutex;
use tracing::warn;

use crate::cowork::CoworkGate;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

use super::role_runner::LoopRoleRunner;
use super::status_sink::LoopStatusSink;
use super::tasks::{read_pending_slices, safe_tasks_path};

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
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, LoopRecord};
    use tempfile::TempDir;

    // ── group 4: loop consumes umbrella-once + slice-each ───────────────────

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
        // Tier/Runner enums are referenced to keep the import surface honest.
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
