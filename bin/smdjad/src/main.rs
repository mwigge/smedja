pub mod acp;
pub mod agent_server;
pub mod alert;
pub mod common;
pub mod cowork;
pub mod embedder;
pub mod embedder_config;
pub mod embedder_port;
pub mod executor;
pub mod filters;
pub mod fragments;
pub mod handlers;
pub mod lean_spec;
pub mod local_provider;
pub mod loop_runner;
pub mod mcp_http;
pub mod mcp_oauth;
pub mod mcp_server;
pub mod mcp_stdio;
pub mod methodology_config;
pub mod methodology_gate;
pub mod orchestrator;
pub mod price_table;
pub mod provider_pool;
pub mod quality_hook;
pub mod quality_runner;
pub mod sandbox;
pub mod security;
pub mod stream_server;

mod bootstrap;
mod compaction;
mod exec;
mod mcp_refresh;
mod net_guard;
mod paths;
mod router;
mod servers;
mod turn_wait;
mod worker;

// Crate-root re-exports: these helpers were previously defined in `main.rs` and
// are referenced as `crate::<name>` (or `super::<name>` from crate-root child
// modules) across the crate. Re-exporting keeps those paths stable after the
// split into cohesive modules.
pub(crate) use compaction::assemble_compaction_transcript;
pub(crate) use exec::{exec_bash, exec_bash_ext};
pub(crate) use net_guard::{is_blocked_ip, is_safe_mcp_url};
pub(crate) use paths::{dirs_home, ingot_err, missing_param};
pub(crate) use turn_wait::await_turn_terminal;

use std::collections::HashMap;
use std::sync::Arc;

use smedja_ingot::IngotHandle;
use smedja_rpc::server::Server;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::cowork::CoworkGate;
use crate::paths::SocketGuard;
use crate::price_table::PriceTable;
use crate::provider_pool::build_provider_pool;

#[allow(clippy::too_many_lines)] // startup sequence: bind, migrate, orphan sweep, spawn workers
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // smdjad has no clap parser; honour `--version`/`-V` so it can report its
    // own build like the other binaries (CARGO_PKG_VERSION = workspace version).
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("smdjad {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    paths::init_tracing();

    // Validate SMEDJA_COMPACT_THRESHOLD at startup — reject invalid values early.
    bootstrap::validate_compact_threshold()?;

    // Install the W3C trace-context propagator and (optionally) the OTLP exporter.
    bootstrap::install_telemetry();

    let path = paths::socket_path();

    // Remove stale socket if it exists.
    let _ = std::fs::remove_file(&path);

    // Bind BEFORE spawning so a port-conflict error exits cleanly.
    let listener = UnixListener::bind(&path)?;
    // Guard removes the socket on any exit path (clean shutdown or error propagation).
    let _socket_guard = SocketGuard { path: path.clone() };

    // Set Unix socket permissions to 0o600 immediately after binding.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("failed to set socket permissions: {e}"))?;
    }

    // Write PID file so `smj daemon stop` can send SIGTERM.
    let pid_path = bootstrap::write_pid_file();

    info!(path = %path.display(), "smdjad listening");

    let ingot = paths::open_ingot()?;
    let ingot = IngotHandle::new(ingot);

    // Detect sessions left in_flight by a prior crash.
    bootstrap::sweep_orphaned_sessions(&ingot).await;

    // Dispatcher capacity 1024: lifecycle events (Started/Completed/Failed) must
    // never be dropped during streaming delta bursts. 1024 provides 4× the
    // previous headroom while remaining negligible in memory cost.
    let dispatcher = Arc::new(smedja_bellows::Dispatcher::new(1024));

    // Refresh stale MCP server tool lists in the background so startup is not
    // delayed by N×network_latency when multiple servers are registered.
    mcp_refresh::spawn_mcp_refresh(ingot.clone());

    let gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>> = Arc::new(Mutex::new(HashMap::new()));

    // Build the provider pool and assayer once at startup; thread through Arc.
    let pool = build_provider_pool().await;
    if pool.is_empty() {
        // Loud degraded state: the daemon stays up (so config can be fixed
        // without a restart loop) but every turn will fail until a provider
        // is configured. build_provider_pool already logged the details.
        error!("starting in a DEGRADED state: no LLM provider configured — turns will fail");
    }
    let startup_runner: Arc<str> = Arc::from(pool.default_runner_name());
    let startup_model: Arc<str> = Arc::from(pool.default_model());
    let pool = Arc::new(pool);

    // Load workspace-local routing overrides if .smedja/agents.toml exists.
    // Default to the absolute current directory (deterministic) rather than the
    // relative "." whose meaning depends on the launcher's cwd under a supervisor.
    let workspace_root = paths::resolve_workspace_root();
    let active_change: Option<Arc<str>> =
        quality_hook::detect_active_change(&workspace_root).map(|s| Arc::from(s.as_str()));
    let assayer = Arc::new(bootstrap::load_assayer(&workspace_root));

    // Advisory security plane: read the [security] config and run the workspace
    // posture scan once. Findings are recorded as advisory AuditEvents; the scan
    // is non-blocking by design and never aborts startup.
    {
        let security_config = security::load_security_config(&workspace_root);
        let n = security::run_startup_posture_scan(&ingot, &workspace_root, &security_config).await;
        if n > 0 {
            info!(
                count = n,
                enforce = security_config.enforce,
                "recorded advisory security posture findings"
            );
        }
    }

    let price_table = Arc::new(PriceTable::embedded());

    let vault = Arc::new(Mutex::new(paths::open_vault()));

    // Resolve the embedding backend from `[embedder]` config + runtime
    // availability. An absent/unparseable config or unreachable learned endpoint
    // resolves to the FNV default; this never blocks startup.
    let embedder_config = embedder_config::load_embedder_config(&workspace_root);
    let embedder = embedder_config::resolve_embedder(&embedder_config).await;
    // Record the active model as the vault's coarse identity marker so a future
    // open knows what the database mostly holds.
    {
        let identity = smedja_vault::EmbedderIdentity {
            model: embedder.model_id().to_owned(),
            dimensions: embedder.dim(),
        };
        if let Err(e) = vault.lock().await.set_embedder_identity(&identity) {
            tracing::warn!(error = %e, "failed to set vault embedder identity; continuing");
        }
    }

    // Single provider-session map, constructed once and threaded to every
    // handler and the orchestrator (replaces the former OnceLock singleton).
    let provider_sessions: orchestrator::ProviderSessions = Arc::new(Mutex::new(HashMap::new()));

    // Background GC: cap the provider_sessions map at 10 000 entries.
    bootstrap::spawn_session_gc(Arc::clone(&provider_sessions));

    // Single cross-turn cache-aligner map, keyed by `(session_id, runner)` and
    // threaded exactly like `provider_sessions`, so each persisted aligner
    // outlives a turn and reports real `Grown`/`Mutated` drift.
    let cache_aligners: orchestrator::CacheAligners = Arc::new(Mutex::new(HashMap::new()));

    // Shared set tracking in-flight turn tasks and loop.run background tasks, so
    // both are drained together at shutdown and completed tasks are reaped.
    let task_set: Arc<Mutex<tokio::task::JoinSet<()>>> =
        Arc::new(Mutex::new(tokio::task::JoinSet::new()));

    // Start LSP manager for the daemon workspace. Servers are auto-detected
    // from PATH; missing servers fail silently.
    let lsp_manager = {
        let mgr = smedja_lsp::LspManager::new();
        let ws = common::workspace_root();
        mgr.start(ws);
        Arc::new(mgr)
    };

    // Dedicated mpsc channel for turn start events — bypasses the broadcast so
    // Started is never dropped under high delta/diagnostic load (capacity 256
    // is a hard upper bound on concurrent in-flight turns).
    let (work_tx, work_rx) = tokio::sync::mpsc::channel::<(String, String)>(256);
    // Retain one sender clone so we can close the channel explicitly at shutdown
    // by dropping it — at that point every handler-held clone has also been
    // dropped (server dropped on select! exit), which closes work_rx.
    let work_tx_shutdown = work_tx.clone();

    // Registry of in-flight turns for `turn.cancel` (ESC interrupt). Shared
    // between the RPC router (cancel handler) and the worker (which registers
    // each turn's abort handle).
    let turn_registry: handlers::TurnRegistry =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    let router = router::build_router(
        &ingot,
        &dispatcher,
        &gates,
        &pool,
        &assayer,
        &startup_runner,
        &startup_model,
        &price_table,
        &vault,
        &embedder,
        &provider_sessions,
        &cache_aligners,
        &task_set,
        &lsp_manager,
        work_tx,
        Arc::clone(&turn_registry),
        active_change.clone(),
    );

    let worker_handle = worker::spawn_worker(
        ingot.clone(),
        Arc::clone(&dispatcher),
        Arc::clone(&gates),
        Arc::clone(&pool),
        Arc::clone(&assayer),
        Arc::clone(&price_table),
        Arc::clone(&vault),
        Arc::clone(&embedder),
        Arc::clone(&provider_sessions),
        Arc::clone(&cache_aligners),
        work_rx,
        Arc::clone(&turn_registry),
        active_change,
        Arc::clone(&lsp_manager),
    );

    // Background daily maintenance: prune old sessions and VACUUM both databases.
    bootstrap::spawn_daily_maintenance(ingot.clone());

    // Post-turn quality gate subscriber: reacts to every TurnEvent::Completed by
    // running the four Tier-1 deterministic gates and dispatching QualitySnapshot.
    bootstrap::spawn_quality_gate(Arc::clone(&dispatcher), workspace_root.clone());

    // ACP HTTP server — activated by SMEDJA_ACP_PORT.
    servers::spawn_acp_server(
        ingot.clone(),
        Arc::clone(&dispatcher),
        workspace_root.clone(),
        Arc::clone(&vault),
        Arc::clone(&embedder),
    )
    .await?;

    // Streaming NDJSON server — sibling socket for live turn events.
    let _stream_sock_guard = servers::spawn_stream_server(&path, &dispatcher);

    // Agent-event push server — sibling socket for live pane telemetry.
    let _agent_sock_guard = servers::spawn_agent_server(&path, &dispatcher, &ingot);

    let server = Server::new(router);

    // Socket is bound and the database is open: signal readiness to systemd.
    paths::sd_notify_ready();

    // Install signal handlers before entering select! so registration failures
    // surface at startup rather than panicking inside the async block.
    let mut sig_term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| anyhow::anyhow!("failed to install SIGTERM handler: {e}"))?;
    let mut sig_hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|e| anyhow::anyhow!("failed to install SIGHUP handler: {e}"))?;

    tokio::select! {
        result = server.serve(listener) => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received SIGINT; shutting down");
        }
        _ = sig_term.recv() => {
            info!("received SIGTERM; shutting down");
        }
        _ = sig_hup.recv() => {
            // Treat SIGHUP as a graceful shutdown so the drain and SocketGuard
            // cleanup run, rather than the default terminate-without-cleanup.
            info!("received SIGHUP; shutting down");
        }
    }

    lsp_manager.shutdown();

    // Drain in-flight turn tasks and loop.run background tasks before cleaning
    // up, so mid-stream work can complete (or fail cleanly) rather than being
    // silently abandoned. A 30 s deadline prevents indefinite blocking; tasks
    // still running at the deadline are dropped (aborted) when the set is.
    //
    // Turn tasks: close the work channel (drop our retained sender clone; the
    // server's handler clones were already dropped when the serve() future was
    // cancelled above). Awaiting the worker handle gives back its private JoinSet
    // containing any turns that were spawned but haven't finished yet.
    drop(work_tx_shutdown);
    let mut turn_set = worker_handle.await.unwrap_or_default();
    // Loop tasks: still tracked in the shared task_set.
    let mut loop_set = std::mem::take(&mut *task_set.lock().await);
    let total = turn_set.len() + loop_set.len();
    if total > 0 {
        info!(
            turns = turn_set.len(),
            loops = loop_set.len(),
            "waiting for in-flight turns and loops to finish (up to 30 s)"
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while turn_set.join_next().await.is_some() {}
            while loop_set.join_next().await.is_some() {}
        })
        .await;
    }

    // Allow stream connections to flush their final `done` lines after turns complete.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    info!("smdjad stopped");
    if let Some(ref p) = pid_path {
        let _ = std::fs::remove_file(p);
    }
    // Socket is removed by _socket_guard's Drop impl on function exit.

    Ok(())
}
