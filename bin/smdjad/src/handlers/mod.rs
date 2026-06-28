//! RPC handler modules for the smdjad daemon.
//!
//! Each handler is a free `async fn` taking a cloned [`HandlerState`] and the
//! request `Value`, returning `Result<Value, RpcError>` — the same contract the
//! inline closures in `build_router` satisfied. `build_router` constructs one
//! [`HandlerState`] and registers each method as a thin closure that clones the
//! state and calls the handler.
//!
//! Moving handlers here is a pure structural refactor: no method name, parameter
//! parsing, or response JSON changes.

use std::collections::HashMap;
use std::sync::Arc;

use smedja_assayer::{Assayer, WorktreePool};
use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_vault::Vault;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::cowork::CoworkGate;
use crate::embedder_port::Embedder;
use crate::orchestrator::ProviderSessions;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

pub(crate) mod audit;
pub(crate) mod auditor;
pub(crate) mod checkpoint;
pub(crate) mod cost;
pub(crate) mod graph;
pub(crate) mod local;
pub(crate) mod loops;
pub(crate) mod lsp;
pub(crate) mod mcp;
pub(crate) mod metrics;
pub(crate) mod routing;
pub(crate) mod savings;
pub(crate) mod session;
pub(crate) mod task;
pub(crate) mod turn;
pub(crate) mod vault;

/// Shared, cheaply-cloneable bundle of the `Arc`s every RPC handler needs.
///
/// Constructed once by `build_router` from the resources `main()` builds at
/// startup; each registered closure clones it before calling the handler.
#[derive(Clone)]
pub(crate) struct HandlerState {
    pub(crate) ingot: IngotHandle,
    pub(crate) dispatcher: Arc<Dispatcher>,
    pub(crate) gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pub(crate) provider_pool: Arc<ProviderPool>,
    pub(crate) worktree_pool: Arc<Mutex<WorktreePool>>,
    pub(crate) assayer: Arc<Assayer>,
    pub(crate) price_table: Arc<PriceTable>,
    pub(crate) vault: Arc<Mutex<Vault>>,
    /// Resolved embedding backend shared by every vault-embedding handler.
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) provider_sessions: ProviderSessions,
    pub(crate) cache_aligners: crate::orchestrator::CacheAligners,
    pub(crate) task_set: Arc<Mutex<JoinSet<()>>>,
    pub(crate) startup_runner: Arc<str>,
    pub(crate) startup_model: Arc<str>,
    /// Shared LSP manager — holds language server processes started at daemon
    /// startup and serves their diagnostic snapshots to `lsp.*` handlers.
    pub(crate) lsp_manager: Arc<smedja_lsp::LspManager>,
    /// Direct channel to the turn worker — bypasses the broadcast so `Started`
    /// events are never dropped even under high diagnostic/delta burst.
    pub(crate) work_tx: tokio::sync::mpsc::Sender<(String, String)>,
    /// In-flight turns keyed by `turn_id` → the `AbortHandle` of their
    /// `run_turn` task, so `turn.cancel` can stop a runaway turn. The worker
    /// inserts on spawn; the turn removes itself when it finishes; `turn.cancel`
    /// removes on abort.
    pub(crate) turn_registry: TurnRegistry,
}

/// Maps an in-flight `turn_id` to the [`tokio::task::AbortHandle`] of its
/// `run_turn` task (see [`HandlerState::turn_registry`]).
pub(crate) type TurnRegistry = Arc<std::sync::Mutex<HashMap<String, tokio::task::AbortHandle>>>;
