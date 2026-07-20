pub mod acp;
pub mod agent_server;
pub mod alert;
pub mod bundle_config;
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
pub mod local_embedder;
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
pub mod subagents;

mod exec;
pub(crate) use exec::{exec_bash, exec_bash_ext};

mod socket;

mod bootstrap;
use bootstrap::{
    init_tracing, sd_notify_ready, spawn_blocking_bounded, spawn_supervised, spawn_worker,
};

mod router;

mod net_guard;
pub(crate) use net_guard::{is_blocked_ip, is_safe_mcp_url};

mod store;
use store::{dirs_home, open_ingot, open_vault};

mod runtime_paths;
use runtime_paths::{resolve_workspace_root, write_acp_secret};

mod turn_wait;
pub(crate) use turn_wait::{await_turn_terminal, run_turn};

use std::collections::HashMap;
use std::sync::Arc;

use smedja_adapter::types::Message as AdapterMessage;
use smedja_assayer::Assayer;

use crate::price_table::PriceTable;
use crate::provider_pool::build_provider_pool;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{IngotHandle, McpServer};
use smedja_rpc::{codes, server::Server, RpcError};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::cowork::CoworkGate;

/// Assembles a conversation transcript for compaction from a checkpoint's
/// `messages_json` blob, routing it through working-memory strata (deep tier) so
/// the same windowing the live prompt path uses is applied. Invalid or empty JSON
/// yields an empty transcript. The raw blob is preserved separately in the
/// pre-compaction checkpoint, so this rendering is not lossy for rollback.
pub(crate) fn assemble_compaction_transcript(messages_json: &str) -> String {
    let parsed: Vec<AdapterMessage> = serde_json::from_str(messages_json).unwrap_or_default();
    let mut mem = smedja_memory::WorkingMemory::new(100_000);
    mem.set_strata(smedja_memory::StrataConfig::deep());
    for m in parsed {
        mem.push(m);
    }
    mem.build_prompt(100_000)
        .iter()
        .map(|m| format!("{}: {}", m.role.as_str(), m.content))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn ingot_err(e: &smedja_ingot::IngotError) -> RpcError {
    RpcError::new(codes::INTERNAL_ERROR, e.to_string())
}

pub(crate) fn missing_param(name: &str) -> RpcError {
    RpcError::new(
        codes::INVALID_PARAMS,
        format!("missing required param: {name}"),
    )
}

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

    init_tracing();

    // Validate SMEDJA_COMPACT_THRESHOLD at startup — reject invalid values early.
    if let Ok(val) = std::env::var("SMEDJA_COMPACT_THRESHOLD") {
        match val.parse::<f64>() {
            Ok(t) if t < 0.5 => {
                anyhow::bail!(
                    "SMEDJA_COMPACT_THRESHOLD={val} is below the minimum of 0.5; \
                     set it to a value in [0.5, 1.0] or unset it to use the default (0.85)"
                );
            }
            Err(_) => {
                tracing::warn!(
                    value = %val,
                    "SMEDJA_COMPACT_THRESHOLD is not a valid float; using default 0.85"
                );
            }
            Ok(_) => {}
        }
    }

    // Install the W3C trace-context propagator process-wide so outbound HTTP
    // calls inject `traceparent`/`tracestate` and inbound contexts are
    // extracted. The adapter only *uses* the global propagator; installing it
    // is the binary's responsibility.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // Install an OTLP exporter when SMEDJA_OTLP_ENDPOINT is set; otherwise fall
    // back to recording spans through the structured-log layer. The trace
    // destination is logged in both branches so operators always know where
    // span data goes (no silent discard).
    if let Ok(endpoint) = std::env::var("SMEDJA_OTLP_ENDPOINT") {
        use opentelemetry_otlp::WithExportConfig as _;
        let build_result = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(&endpoint)
            .build();
        match build_result {
            Ok(exporter) => {
                let provider = opentelemetry_sdk::trace::TracerProvider::builder()
                    .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                    .build();
                opentelemetry::global::set_tracer_provider(provider);
                info!(endpoint = %endpoint, "trace destination: OTLP exporter");
            }
            Err(e) => {
                error!(error = %e, endpoint = %endpoint, "failed to install OTLP exporter; trace destination: structured logs only");
            }
        }
    } else {
        info!("SMEDJA_OTLP_ENDPOINT not set; trace destination: structured logs only (set the endpoint to export OTLP spans)");
    }

    let path = socket::socket_path();

    // Single-instance guard: refuse to hijack a socket a live daemon is already
    // serving. A blind `remove_file` + rebind let a second daemon steal the
    // socket while both processes held the same databases. Probe by connecting;
    // a successful connect means a peer is alive, so abort. Only a stale socket
    // (connect refused / absent) is reclaimed.
    #[cfg(unix)]
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        anyhow::bail!(
            "another smdjad is already listening on {}; refusing to start a second instance",
            path.display()
        );
    }

    // Remove the stale socket if it exists.
    let _ = std::fs::remove_file(&path);

    // Bind under a restrictive umask so the socket node is created 0600 from the
    // start. Otherwise it is briefly world-accessible between `bind` and the
    // `set_permissions` below — and with XDG_RUNTIME_DIR unset the socket lives
    // in world-traversable /tmp, where another local user could connect in that
    // window and issue `exec_bash`. The umask is restored immediately after.
    // Bind BEFORE spawning so a bind error exits cleanly.
    #[cfg(unix)]
    let prev_umask = unsafe { libc::umask(0o077) };
    let bind_result = UnixListener::bind(&path);
    #[cfg(unix)]
    unsafe {
        libc::umask(prev_umask);
    }
    let listener = bind_result?;
    // Guard removes the socket on any exit path (clean shutdown or error propagation).
    let _socket_guard = socket::SocketGuard { path: path.clone() };

    // Re-assert 0o600 as defence in depth (the umask above already created it so).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("failed to set socket permissions: {e}"))?;
    }

    // Write PID file so `smj daemon stop` can send SIGTERM. Stored in
    // XDG_RUNTIME_DIR (per-user tmpfs, 0700 by default on systemd) or
    // ~/.cache as a private fallback; never /tmp which is world-traversable.
    let pid_path = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|d| std::path::PathBuf::from(d).join("smdjad.pid"))
        .or_else(|| dirs_home().map(|h| h.join(".cache").join("smdjad.pid")));
    if let Some(ref p) = pid_path {
        std::fs::write(p, std::process::id().to_string())
            .unwrap_or_else(|e| tracing::warn!(error = %e, "failed to write PID file"));
    } else {
        tracing::warn!("no private directory for PID file (set XDG_RUNTIME_DIR or HOME); smj daemon stop will not work");
    }

    info!(path = %path.display(), "smdjad listening");

    let ingot = open_ingot()?;
    let ingot = IngotHandle::new(ingot);

    // Detect sessions left in_flight by a prior crash.
    {
        // ponytail: linear scan; session counts are small
        match ingot.list_sessions().await {
            Ok(sessions) => {
                let orphaned: Vec<_> = sessions
                    .into_iter()
                    .filter(|s| s.status == "in_flight")
                    .collect();
                if !orphaned.is_empty() {
                    tracing::warn!(
                        count = orphaned.len(),
                        "orphaned in_flight sessions detected at startup; marking as orphaned"
                    );
                    for sess in &orphaned {
                        let sid = sess.id.to_string();
                        let _ = ingot.update_session_status(&sid, "orphaned").await;

                        // Also fail any in_progress tasks owned by this session.
                        match ingot.list_tasks(Some("in_progress".to_owned())).await {
                            Ok(tasks) => {
                                for task in tasks {
                                    if task.session_id.as_deref() == Some(sid.as_str()) {
                                        let _ = ingot
                                            .update_task_status(&task.id.to_string(), "failed")
                                            .await;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "could not list tasks during orphan sweep");
                            }
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not list sessions at startup"),
        }
    }

    // Dispatcher capacity 1024: lifecycle events (Started/Completed/Failed) must
    // never be dropped during streaming delta bursts. 1024 provides 4× the
    // previous headroom while remaining negligible in memory cost.
    let dispatcher = Arc::new(Dispatcher::new(1024));

    // Refresh stale MCP server tool lists in the background so startup is not
    // delayed by N×network_latency when multiple servers are registered.
    {
        let ingot_clone = ingot.clone();
        tokio::spawn(async move {
            let stale_threshold = crate::common::now_epoch() - 3600.0;
            let servers = ingot_clone
                .list_mcp_servers()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.last_refresh < stale_threshold)
                .collect::<Vec<_>>();

            for server in servers {
                match crate::mcp_http::McpHttpClient::new(&server.url, "") {
                    Ok(client) => match client.list_tools().await {
                        Ok(tools) => {
                            let tools_json =
                                serde_json::to_string(&tools).unwrap_or_else(|_| "[]".to_owned());
                            let updated = McpServer {
                                tools_json,
                                last_refresh: crate::common::now_epoch(),
                                ..server.clone()
                            };
                            if let Err(e) = ingot_clone.register_mcp_server(updated).await {
                                tracing::warn!(name = %server.name, error = %e, "failed to update MCP tools at startup");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(name = %server.name, error = %e, "MCP refresh failed at startup");
                        }
                    },
                    Err(e) => {
                        tracing::warn!(name = %server.name, error = %e, "failed to build MCP client at startup");
                    }
                }
            }
        });
    }
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
    let workspace_root = resolve_workspace_root();
    let active_change: Option<Arc<str>> =
        quality_hook::detect_active_change(&workspace_root).map(|s| Arc::from(s.as_str()));
    let mut assayer = Assayer::default_rules();
    match smedja_assayer::load_rules(&workspace_root) {
        Ok(rules) if !rules.is_empty() => {
            let n = rules.len();
            assayer.prepend_rules(rules);
            info!(count = n, path = ?workspace_root.join(".smedja/agents.toml"), "loaded agents.toml overrides");
        }
        Ok(_) => {}
        Err(e) => {
            warn!(error = %e, "failed to load .smedja/agents.toml; using default routing");
        }
    }
    let assayer = Arc::new(assayer);

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

    // Surface models that will report $0.00 cost at startup so missing pricing
    // data is never a silent surprise.
    pool.warn_missing_prices(&price_table);

    let vault = Arc::new(Mutex::new(open_vault()));

    // Resolve the embedding backend from `[embedder]` config + runtime
    // availability. An absent/unparseable config or unreachable learned endpoint
    // resolves to the FNV default; this never blocks startup.
    let embedder_config = embedder_config::load_embedder_config(&workspace_root);
    let embedder = embedder_config::resolve_embedder(&embedder_config).await;
    // Surface recall quality at startup so lexical-only recall is never a silent
    // surprise: a semantic backend logs at INFO, while the FNV default (keyword
    // overlap, not semantic) is called out at WARN with the config that upgrades it.
    {
        let status = embedder.status();
        if status.semantic {
            tracing::info!(
                model = %status.model_id,
                dim = status.dim,
                "vault recall: semantic embedder active"
            );
        } else if status.degraded {
            // A semantic backend was configured but could not be loaded — recall
            // is lexical against intent, not by choice. Never silent.
            tracing::warn!(
                model = %status.model_id,
                "vault recall is DEGRADED to LEXICAL (FNV keyword overlap): a local semantic model was configured but could not be fetched or loaded (offline or not yet cached). Recall stays keyword-only until the model can be downloaded to ~/.local/share/smedja/models/"
            );
        } else {
            tracing::warn!(
                model = %status.model_id,
                "vault recall is LEXICAL (FNV keyword overlap), not semantic — set [embedder] backend = \"local\" (bundled model) or \"learned\" (local /v1/embeddings endpoint) in .smedja/config.toml for semantic recall"
            );
        }
    }
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

    // Background GC: cap the provider_sessions map at 10 000 entries. When the
    // cap is exceeded, evict only entries idle for over 30 minutes — a session
    // whose turn is running right now touched its entry more recently and is
    // retained even over cap, so an in-flight turn never loses its provider-native
    // resume id. The GC task wakes every 5 minutes.
    {
        const SESSION_CAP: usize = 10_000;
        let idle = std::time::Duration::from_secs(30 * 60);
        let ps = Arc::clone(&provider_sessions);
        spawn_supervised("provider_sessions_gc", async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_mins(5)).await;
                let mut map = ps.lock().await;
                let evicted = orchestrator::gc_provider_sessions(&mut map, SESSION_CAP, idle);
                if evicted > 0 {
                    tracing::info!(
                        "provider_sessions: evicted {evicted} idle entries (cap exceeded, {} retained)",
                        map.len()
                    );
                }
            }
        });
    }

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

    let worker_handle = spawn_worker(
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
    {
        let ingot_for_vacuum = ingot.clone();
        spawn_supervised("daily_maintenance", async move {
            // First run after 1 hour so startup I/O is not affected.
            tokio::time::sleep(std::time::Duration::from_hours(1)).await;
            loop {
                match ingot_for_vacuum.prune_old_sessions(30).await {
                    Ok(n) if n > 0 => info!(pruned = n, "pruned old terminated sessions"),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "session prune failed"),
                }
                if let Err(e) = ingot_for_vacuum.vacuum().await {
                    warn!(error = %e, "database vacuum failed");
                }
                tokio::time::sleep(std::time::Duration::from_hours(24)).await;
            }
        });
    }

    // Post-turn quality gate subscriber: reacts to every TurnEvent::Completed by
    // running the four Tier-1 deterministic gates and dispatching QualitySnapshot.
    // All errors in the hook are swallowed — this must never stall the turn loop.
    {
        let mut quality_rx = dispatcher.subscribe();
        let quality_dispatcher = Arc::clone(&dispatcher);
        let quality_workspace = workspace_root.clone();
        let session_skills = quality_hook::discover_session_skills(&workspace_root);
        let file_size_threshold = quality_hook::load_file_size_threshold(&workspace_root);
        // Cap concurrent quality runs so a burst of completions cannot saturate
        // the blocking pool and starve DB writes.
        let quality_limit = Arc::new(tokio::sync::Semaphore::new(4));
        spawn_supervised("quality_hook", async move {
            loop {
                let events = smedja_bellows::drain_ready(&mut quality_rx);
                for ev in events {
                    if let TurnEvent::Completed { turn_id, .. } = ev {
                        let disp = Arc::clone(&quality_dispatcher);
                        let ws = quality_workspace.clone();
                        let skills = session_skills.clone();
                        spawn_blocking_bounded(&quality_limit, move || {
                            quality_hook::run_after_turn(
                                Some(turn_id),
                                ws,
                                skills,
                                file_size_threshold,
                                disp,
                            );
                        })
                        .await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
    }

    // ACP HTTP server — activated by SMEDJA_ACP_PORT.
    if let Ok(port_str) = std::env::var("SMEDJA_ACP_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            // Generate a one-time auth token and write it to the runtime secret file.
            let acp_token = uuid::Uuid::new_v4().to_string();
            write_acp_secret(&acp_token);
            let acp_state = acp::AcpState {
                ingot: ingot.clone(),
                dispatcher: Arc::clone(&dispatcher),
                auth_token: acp_token,
                workspace: workspace_root.clone(),
                vault: Arc::clone(&vault),
                embedder: Arc::clone(&embedder),
                gates: Arc::clone(&gates),
            };
            let acp_router = acp::build_acp_router(acp_state);
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            // Bind before spawning so a port conflict fails at startup, not inside the task.
            let tcp_listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| anyhow::anyhow!("ACP bind failed on {addr}: {e}"))?;
            info!(%addr, "ACP HTTP server listening");
            spawn_supervised("acp_server", async move {
                if let Err(e) = axum::serve(tcp_listener, acp_router).await {
                    tracing::error!(error = %e, "ACP server error");
                }
            });
        }
    }

    // Streaming NDJSON server — sibling socket for live turn events.
    let delta_store = stream_server::spawn_delta_buffer(&dispatcher);
    let stream_sock_path = stream_server::stream_socket_path(&path);
    let _ = std::fs::remove_file(&stream_sock_path);
    let _stream_sock_guard = socket::SocketGuard {
        path: stream_sock_path.clone(),
    };
    // Bind under a 0077 umask so the sibling socket is never world-accessible.
    #[cfg(unix)]
    let prev_umask = unsafe { libc::umask(0o077) };
    let stream_bind = UnixListener::bind(&stream_sock_path);
    #[cfg(unix)]
    unsafe {
        libc::umask(prev_umask);
    }
    match stream_bind {
        Ok(stream_listener) => {
            // A failure to restrict the socket is fatal: an exposed streaming
            // socket lets another local user read live turn events.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&stream_sock_path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| anyhow::anyhow!("failed to set stream socket permissions: {e}"))?;
            }
            info!(path = %stream_sock_path.display(), "turn stream server listening");
            let ds = Arc::clone(&delta_store);
            let dp = Arc::clone(&dispatcher);
            spawn_supervised("stream_server", async move {
                stream_server::serve(stream_listener, ds, dp).await;
            });
        }
        Err(e) => {
            warn!(error = %e, "failed to bind stream socket; live streaming unavailable");
        }
    }

    // Agent-event push server — sibling socket for live pane telemetry.
    let agent_sock_path = agent_server::agent_socket_path(&path);
    let _ = std::fs::remove_file(&agent_sock_path);
    let _agent_sock_guard = socket::SocketGuard {
        path: agent_sock_path.clone(),
    };
    // Bind under a 0077 umask so the sibling socket is never world-accessible.
    #[cfg(unix)]
    let prev_umask = unsafe { libc::umask(0o077) };
    let agent_bind = UnixListener::bind(&agent_sock_path);
    #[cfg(unix)]
    unsafe {
        libc::umask(prev_umask);
    }
    match agent_bind {
        Ok(agent_listener) => {
            // A failure to restrict the socket is fatal: an exposed agent socket
            // lets another local user read live pane telemetry.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&agent_sock_path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| anyhow::anyhow!("failed to set agent socket permissions: {e}"))?;
            }
            info!(path = %agent_sock_path.display(), "agent event server listening");
            let dp = Arc::clone(&dispatcher);
            let agent_ingot = ingot.clone();
            spawn_supervised("agent_server", async move {
                agent_server::serve(agent_listener, dp, agent_ingot).await;
            });
        }
        Err(e) => {
            warn!(error = %e, "failed to bind agent socket");
        }
    }

    let server = Server::new(router);

    // Socket is bound and the database is open: signal readiness to systemd.
    sd_notify_ready();

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

#[cfg(test)]
mod tests;
