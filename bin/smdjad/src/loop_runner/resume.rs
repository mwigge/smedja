//! [`resume`]: re-enters the loop engine from the last checkpointed slice.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_loop::LoopConfig;
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
    )
    .await;
}
