//! RPC method router assembly for the daemon.
//!
//! [`build_router`] wires every JSON-RPC method to its `handlers::*` function
//! over a shared [`handlers::HandlerState`] bundle. It is called once by
//! `main()` after the daemon's shared resources are constructed.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_assayer::{Assayer, WorktreePool};
use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_rpc::router::Router;
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::cowork::CoworkGate;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;
use crate::{embedder_port, handlers, orchestrator};

/// Registers an RPC handler with boilerplate-free cloning.
///
/// Expands to: clone `$state`, register a closure that re-clones state per
/// call, and delegates to `$handler(state, params)`. This eliminates the
/// 4-line let+register+move+async pattern that would otherwise repeat for
/// every method.
macro_rules! route {
    ($router:expr, $method:literal, $state:expr, $handler:path) => {{
        let s = $state.clone();
        $router.register($method, move |params: Value| {
            let state = s.clone();
            async move { $handler(state, params).await }
        });
    }};
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn build_router(
    ingot: &IngotHandle,
    dispatcher: &Arc<Dispatcher>,
    gates: &Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: &Arc<ProviderPool>,
    assayer: &Arc<Assayer>,
    startup_runner: &Arc<str>,
    startup_model: &Arc<str>,
    price_table: &Arc<PriceTable>,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn embedder_port::Embedder>,
    provider_sessions: &orchestrator::ProviderSessions,
    cache_aligners: &orchestrator::CacheAligners,
    task_set: &Arc<Mutex<tokio::task::JoinSet<()>>>,
    lsp_manager: &Arc<smedja_lsp::LspManager>,
    work_tx: tokio::sync::mpsc::Sender<(String, String)>,
    turn_registry: handlers::TurnRegistry,
    active_change: Option<Arc<str>>,
) -> Router {
    let mut router = Router::new();

    // The shared handler state bundle: every handler closure clones this and
    // calls the corresponding module function. This is the only construction
    // point; the registrations below are thin wiring over `handlers::*`.
    let state = handlers::HandlerState {
        ingot: ingot.clone(),
        dispatcher: Arc::clone(dispatcher),
        gates: Arc::clone(gates),
        provider_pool: Arc::clone(pool),
        // Worktree pool shared by task.parallel and task.cancel.
        worktree_pool: Arc::new(Mutex::new(WorktreePool::default())),
        assayer: Arc::clone(assayer),
        price_table: Arc::clone(price_table),
        vault: Arc::clone(vault),
        embedder: Arc::clone(embedder),
        provider_sessions: Arc::clone(provider_sessions),
        cache_aligners: Arc::clone(cache_aligners),
        task_set: Arc::clone(task_set),
        startup_runner: Arc::clone(startup_runner),
        startup_model: Arc::clone(startup_model),
        lsp_manager: Arc::clone(lsp_manager),
        active_change,
        work_tx,
        turn_registry,
    };

    router.register("ping", |_| async { Ok(json!("pong")) });

    route!(router, "session.create", state, handlers::session::create);
    route!(router, "session.list", state, handlers::session::list);
    route!(router, "session.search", state, handlers::session::search);
    route!(router, "session.get", state, handlers::session::get);
    route!(router, "session.delete", state, handlers::session::delete);
    route!(router, "session.fork", state, handlers::session::fork);
    route!(
        router,
        "session.takeover",
        state,
        handlers::session::takeover
    );
    route!(
        router,
        "session.set_model",
        state,
        handlers::session::set_model
    );
    route!(
        router,
        "session.set_runner",
        state,
        handlers::session::set_runner
    );
    route!(
        router,
        "session.set_tier",
        state,
        handlers::session::set_tier
    );
    route!(
        router,
        "session.set_mode",
        state,
        handlers::session::set_mode
    );
    route!(
        router,
        "session.set_title",
        state,
        handlers::session::set_title
    );
    route!(router, "session.context", state, handlers::session::context);
    route!(
        router,
        "session.token_usage",
        state,
        handlers::session::token_usage
    );
    route!(router, "session.history", state, handlers::session::history);
    route!(
        router,
        "session.checkpoint.list",
        state,
        handlers::checkpoint::list
    );
    route!(
        router,
        "session.rollback",
        state,
        handlers::checkpoint::rollback
    );
    route!(
        router,
        "session.compact",
        state,
        handlers::checkpoint::compact
    );
    route!(router, "session.cost", state, handlers::cost::cost);
    route!(
        router,
        "cost.active_change",
        state,
        handlers::cost::active_change
    );
    route!(router, "runner.list", state, handlers::session::runner_list);
    route!(router, "turn.submit", state, handlers::turn::submit);
    route!(router, "turn.cancel", state, handlers::turn::cancel);
    // Blocks until terminal status or 60 s deadline; event-driven, no poll.
    route!(router, "turn.subscribe", state, handlers::turn::subscribe);
    route!(router, "task.get", state, handlers::task::get);
    route!(router, "task.list", state, handlers::task::list);
    route!(router, "task.create", state, handlers::task::create);
    route!(router, "task.close", state, handlers::task::close);
    route!(router, "task.parallel", state, handlers::task::parallel);
    route!(router, "task.cancel", state, handlers::task::cancel);
    // Live shared coordination blocks for parallel fan-out roles: additive
    // append, owner rewrite, and full read of a `fan_out_id`-keyed block.
    route!(
        router,
        "task.block_append",
        state,
        handlers::task::block_append
    );
    route!(
        router,
        "task.block_rewrite",
        state,
        handlers::task::block_rewrite
    );
    route!(router, "task.block_read", state, handlers::task::block_read);
    route!(router, "metrics.summary", state, handlers::metrics::summary);
    route!(router, "savings.summary", state, handlers::savings::summary);
    route!(router, "cowork.set", state, handlers::audit::set);
    route!(router, "cowork.set_mode", state, handlers::audit::set_mode);
    route!(
        router,
        "cowork.gate_tool",
        state,
        handlers::audit::gate_tool
    );
    route!(router, "cowork.approve", state, handlers::audit::approve);
    route!(router, "cowork.deny", state, handlers::audit::deny);
    route!(router, "cowork.modify", state, handlers::audit::modify);
    route!(router, "cowork.pending", state, handlers::audit::pending);
    route!(router, "mcp.register", state, handlers::mcp::register);
    route!(router, "mcp.list", state, handlers::mcp::list);
    route!(router, "mcp.remove", state, handlers::mcp::remove);
    route!(router, "mcp.refresh", state, handlers::mcp::refresh);
    route!(router, "local.models", state, handlers::local::models);
    route!(router, "local.gpu", state, handlers::local::gpu);
    route!(router, "local.swap", state, handlers::local::swap);
    route!(router, "local.install", state, handlers::local::install);
    route!(router, "loop.create", state, handlers::loops::create);
    route!(router, "loop.status", state, handlers::loops::status);
    route!(router, "loop.cancel", state, handlers::loops::cancel);
    route!(router, "loop.list", state, handlers::loops::list);
    route!(router, "loop.retire", state, handlers::loops::retire);
    route!(
        router,
        "loop.list_by_status",
        state,
        handlers::loops::list_by_status
    );
    // Drives the smedja-loop engine: policy hash, evaluator separation, slice pipeline.
    route!(router, "loop.run", state, handlers::loops::run);
    // Re-enters drive() from the last checkpointed slice index.
    route!(router, "loop.resume", state, handlers::loops::resume);
    // Native OpenSpec engine: author/validate/inspect/archive changes over RPC.
    route!(router, "spec.create", state, handlers::spec::create);
    route!(router, "spec.validate", state, handlers::spec::validate);
    route!(router, "spec.show", state, handlers::spec::show);
    route!(router, "spec.diff", state, handlers::spec::diff);
    route!(router, "spec.list", state, handlers::spec::list);
    route!(router, "spec.status", state, handlers::spec::status);
    route!(router, "spec.archive", state, handlers::spec::archive);
    route!(router, "audit.list", state, handlers::audit::list);
    // Bounded read-only repo/PR/branch audit; returns findings + report.
    route!(router, "audit.run", state, handlers::auditor::run);
    // Resolves (role, complexity?) through the assayer.
    route!(router, "agent.routing", state, handlers::routing::routing);
    route!(router, "lsp.status", state, handlers::lsp::status);
    route!(router, "lsp.diagnostics", state, handlers::lsp::diagnostics);
    route!(router, "graph.index", state, handlers::graph::index);
    route!(router, "graph.query", state, handlers::graph::query);
    route!(router, "graph.status", state, handlers::graph::status);
    route!(router, "vault.reembed", state, handlers::vault::reembed);
    route!(router, "quality.review", state, handlers::quality::review);

    // quota.limit — reads SMEDJA_DAILY_TOKEN_LIMIT env var; no handler state needed.
    router.register("quota.limit", |_| async {
        let limit = std::env::var("SMEDJA_DAILY_TOKEN_LIMIT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        Ok(serde_json::json!({ "daily_tokens": limit }))
    });

    router
}
