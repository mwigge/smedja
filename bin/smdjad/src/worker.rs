//! Turn execution worker: spawns a [`crate::orchestrator::TurnOrchestrator`] per
//! `TurnEvent::Started` and reaps finished tasks.

use std::collections::HashMap;
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::cowork::CoworkGate;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;
use crate::{embedder_port, handlers, orchestrator};

/// Executes a single turn: loads the task, calls the LLM, handles tool calls,
/// stores the final response.
#[allow(clippy::too_many_arguments)] // forwarded directly to TurnOrchestrator
async fn run_turn(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    session_id: String,
    turn_id: String,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn embedder_port::Embedder>,
    provider_sessions: orchestrator::ProviderSessions,
    cache_aligners: orchestrator::CacheAligners,
    turn_registry: handlers::TurnRegistry,
    active_change: Option<Arc<str>>,
    lsp_manager: Arc<smedja_lsp::LspManager>,
) {
    // Deregister-on-drop: removes this turn's abort handle from the registry
    // whether the turn completes normally *or* is aborted by `turn.cancel`
    // (aborting drops this future, which runs the guard's destructor). This is
    // race-free vs. the worker's insert — the guard only removes on drop, which
    // can only happen after the turn has started running.
    struct Deregister {
        registry: handlers::TurnRegistry,
        turn_id: String,
    }
    impl Drop for Deregister {
        fn drop(&mut self) {
            if let Ok(mut reg) = self.registry.lock() {
                reg.remove(&self.turn_id);
            }
        }
    }
    let _dereg = Deregister {
        registry: turn_registry,
        turn_id: turn_id.clone(),
    };

    orchestrator::TurnOrchestrator::new(
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
        active_change.as_deref().map(str::to_owned),
        lsp_manager,
    )
    .run(session_id, turn_id)
    .await;
}

/// Subscribes to [`smedja_bellows::TurnEvent::Started`] and spawns a
/// [`run_turn`] task for each.
///
/// Owns its `JoinSet` exclusively — no shared mutex. Finished tasks are reaped
/// via `try_join_next` so the set size tracks only *in-flight* work. When
/// `work_rx` closes (all senders dropped), the worker exits its loop and
/// returns the set so the caller can drain any remaining tasks at shutdown.
///
/// Started events arrive via a dedicated `work_rx` mpsc channel (sent by the
/// `turn.submit` handler) rather than the broadcast, so they cannot be dropped
/// even when the broadcast is temporarily full from delta events.
#[allow(clippy::too_many_arguments)] // forwarded directly to TurnOrchestrator
pub(crate) fn spawn_worker(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn embedder_port::Embedder>,
    provider_sessions: orchestrator::ProviderSessions,
    cache_aligners: orchestrator::CacheAligners,
    mut work_rx: tokio::sync::mpsc::Receiver<(String, String)>,
    turn_registry: handlers::TurnRegistry,
    active_change: Option<Arc<str>>,
    lsp_manager: Arc<smedja_lsp::LspManager>,
) -> tokio::task::JoinHandle<tokio::task::JoinSet<()>> {
    tokio::spawn(async move {
        let mut set = tokio::task::JoinSet::new();
        loop {
            let Some((session_id, turn_id)) = work_rx.recv().await else {
                break; // all senders dropped — daemon shutting down
            };
            let ig = ingot.clone();
            let dp = Arc::clone(&dispatcher);
            let g = Arc::clone(&gates);
            let pl = Arc::clone(&pool);
            let as_ = Arc::clone(&assayer);
            let pt = Arc::clone(&price_table);
            let vt = Arc::clone(&vault);
            let em = Arc::clone(&embedder);
            let ps = Arc::clone(&provider_sessions);
            let ca = Arc::clone(&cache_aligners);
            let reg = Arc::clone(&turn_registry);
            let ac = active_change.clone();
            let lsp = Arc::clone(&lsp_manager);
            let handle = set.spawn(run_turn(
                ig,
                dp,
                session_id,
                turn_id.clone(),
                g,
                pl,
                as_,
                pt,
                vt,
                em,
                ps,
                ca,
                Arc::clone(&turn_registry),
                ac,
                lsp,
            ));
            // Register the abort handle so `turn.cancel` can interrupt this turn.
            if let Ok(mut map) = reg.lock() {
                map.insert(turn_id, handle);
            }
            // Reap finished tasks so the set tracks only in-flight work.
            while set.try_join_next().is_some() {}
            tracing::debug!(in_flight = set.len(), "turn spawned");
        }
        set
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smedja_types::Timestamp;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn session_set_runner_stores_canonical_key() {
        use smedja_ingot::{Ingot, IngotHandle, Session};
        use uuid::Uuid;

        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let session_id = Uuid::new_v4().to_string();
        let now = Timestamp::from_secs_f64(1_700_000_000.0);
        ig.create_session(Session {
            id: Uuid::parse_str(&session_id).unwrap(),
            created_at: now,
            updated_at: now,
            status: "active".into(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        })
        .await
        .unwrap();
        ig.update_session_runner_override(&session_id, "codex-cli")
            .await
            .unwrap();
        let fetched = ig.get_session(&session_id).await.unwrap().unwrap();
        assert_eq!(fetched.runner_override.as_deref(), Some("codex-cli"));
    }

    #[tokio::test]
    async fn session_takeover_forks_with_runner_override() {
        use smedja_ingot::{Ingot, IngotHandle, Session};
        use uuid::Uuid;

        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let parent_id = Uuid::new_v4().to_string();
        let now = Timestamp::from_secs_f64(1_700_000_000.0);
        ig.create_session(Session {
            id: Uuid::parse_str(&parent_id).unwrap(),
            created_at: now,
            updated_at: now,
            status: "active".into(),
            task_id: None,
            mode: Some("impl".into()),
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        })
        .await
        .unwrap();

        // Simulate takeover: fork then set runner_override.
        let new_id = Uuid::new_v4().to_string();
        let parent = ig.get_session(&parent_id).await.unwrap().unwrap();
        ig.create_session(Session {
            id: Uuid::parse_str(&new_id).unwrap(),
            created_at: Timestamp::from_secs_f64(1_700_000_001.0),
            updated_at: Timestamp::from_secs_f64(1_700_000_001.0),
            status: "active".into(),
            task_id: None,
            mode: parent.mode.clone(),
            title: parent.title.clone(),
            cowork_mode: parent.cowork_mode,
            workspace_root: parent.workspace_root.clone(),
            model_override: parent.model_override.clone(),
            runner_override: Some("codex-cli".into()),
        })
        .await
        .unwrap();

        let new_sess = ig.get_session(&new_id).await.unwrap().unwrap();
        assert_eq!(new_sess.runner_override.as_deref(), Some("codex-cli"));
        assert_eq!(new_sess.mode.as_deref(), Some("impl"));
    }

    #[tokio::test]
    async fn warm_snapshot_writes_vault_entries_for_session_checkpoints() {
        use smedja_ingot::Checkpoint;
        use smedja_vault::Vault;
        use uuid::Uuid;

        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let session_id = "sess-warm-test".to_owned();
        let fan_out_id = "fan-01".to_owned();

        let messages = r#"[{"role":"user","content":"what is async rust"}]"#.to_owned();
        let checkpoints = vec![Checkpoint {
            id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            session_id: session_id.clone(),
            turn_n: 0,
            messages_json: messages.clone(),
            created_at: Timestamp::from_secs_f64(1_700_000_000.0),
            compaction_id: None,
        }];

        // Simulate the warm snapshot logic from task.parallel.
        let fid = fan_out_id.clone();
        let parent_sid = session_id.clone();
        let vt = Arc::clone(&vault);
        tokio::task::spawn_blocking(move || {
            let mut guard = vt.blocking_lock();
            for cp in &checkpoints {
                let entry = smedja_vault::VaultEntry {
                    id: format!("warm:{}:{}", fid, cp.id),
                    embedding: crate::embedder::embed(&cp.messages_json),
                    payload: serde_json::json!({
                        "fan_out_id": fid,
                        "session_id": parent_sid,
                        "turn_n": cp.turn_n,
                    }),
                    namespace: "warm".to_owned(),
                    content: cp.messages_json.clone(),
                    source_file: None,
                    added_by: Some("task.parallel".to_owned()),
                    chunk_index: None,
                    parent_id: None,
                    created_at: 0.0,
                    embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
                    dim: crate::embedder::DIM,
                };
                guard.upsert(&entry).unwrap();
            }
        })
        .await
        .unwrap();

        let count = vault.lock().await.count_by_namespace("warm").unwrap();
        assert_eq!(count, 1, "one warm entry must be written per checkpoint");

        let results = {
            let guard = vault.lock().await;
            let qv = crate::embedder::embed("async rust");
            guard
                .search(
                    &qv,
                    "async rust",
                    "warm",
                    5,
                    smedja_vault::LEGACY_MODEL_ID,
                    crate::embedder::DIM,
                )
                .unwrap()
        };
        assert!(
            !results.is_empty(),
            "warm snapshot must be retrievable by content similarity"
        );
    }

    #[tokio::test]
    async fn takeover_handoff_writes_vault_entry_with_handoff_namespace() {
        use smedja_vault::Vault;

        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let from_sid = "sess-from".to_owned();
        let to_sid = "sess-to".to_owned();
        let messages =
            r#"[{"role":"user","content":"implement auth"},{"role":"assistant","content":"ok"}]"#
                .to_owned();
        let hid = format!("handoff:{from_sid}:{to_sid}");

        let hid2 = hid.clone();
        let from2 = from_sid.clone();
        let to2 = to_sid.clone();
        let msgs = messages.clone();
        let vt = Arc::clone(&vault);
        tokio::task::spawn_blocking(move || {
            let entry = smedja_vault::VaultEntry {
                id: hid2,
                embedding: crate::embedder::embed(&msgs),
                payload: serde_json::json!({
                    "from_session_id": from2,
                    "to_session_id": to2,
                    "runner": "codex-cli",
                }),
                namespace: "handoff".to_owned(),
                content: msgs,
                source_file: None,
                added_by: Some("session.takeover".to_owned()),
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
                embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
                dim: crate::embedder::DIM,
            };
            let mut guard = vt.blocking_lock();
            guard.upsert(&entry).unwrap();
        })
        .await
        .unwrap();

        let count = vault.lock().await.count_by_namespace("handoff").unwrap();
        assert_eq!(count, 1, "one handoff entry must be written on takeover");
    }
}
