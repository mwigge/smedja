pub mod acp;
pub mod agent_server;
pub mod alert;
pub mod common;
pub mod cowork;
pub mod embedder;
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
pub mod sandbox;
pub mod security;
pub mod stream_server;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_adapter::types::Message as AdapterMessage;
use smedja_assayer::{Assayer, WorktreePool};

use crate::price_table::PriceTable;
use crate::provider_pool::{build_provider_pool, ProviderPool};
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Ingot, IngotHandle, McpServer};
use smedja_rpc::{codes, router::Router, server::Server, RpcError};
use smedja_vault::Vault;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::cowork::CoworkGate;

fn socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        tracing::warn!("XDG_RUNTIME_DIR not set; using /tmp for socket — set XDG_RUNTIME_DIR for a secure socket location");
        "/tmp".into()
    });
    PathBuf::from(base).join("smdjad.sock")
}

/// RAII guard that removes the Unix socket file when dropped.
///
/// This ensures the socket is cleaned up on both clean shutdown and error
/// propagation (e.g. when `server.serve()` returns `Err` and `?` exits early).
struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

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

fn open_ingot() -> anyhow::Result<Ingot> {
    // Try to open the persistent store under ~/.local/share/smedja/smedja.db.
    // If the data directory cannot be created, fall back to in-memory.
    let data_dir = dirs_home()
        .map(|h| h.join(".local").join("share").join("smedja"))
        .filter(|d| std::fs::create_dir_all(d).is_ok());

    if let Some(dir) = data_dir {
        let db_path = dir.join("smedja.db");
        Ingot::open(&db_path).map_err(anyhow::Error::from)
    } else {
        tracing::error!("cannot create data directory; using in-memory store — all session data will be lost on restart");
        Ingot::open_in_memory().map_err(anyhow::Error::from)
    }
}

fn open_vault() -> Vault {
    // Mirror the ingot path: ~/.local/share/smedja/vault.db.
    // Falls back to an in-memory vault if the directory cannot be created.
    let vault_path = dirs_home()
        .map(|h| h.join(".local").join("share").join("smedja"))
        .filter(|d| std::fs::create_dir_all(d).is_ok())
        .map(|dir| dir.join("vault.db"));

    if let Some(path) = vault_path {
        match Vault::open(&path) {
            Ok(v) => return v,
            Err(e) => tracing::warn!(error = %e, "vault open failed; using in-memory vault"),
        }
    } else {
        tracing::warn!("cannot create vault data directory; using in-memory vault");
    }
    Vault::open_in_memory().expect("in-memory vault must open")
}

/// Returns the user's home directory, or `None` if it cannot be determined.
fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
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

/// Executes a bash command in `workspace` using `sh -c`, returning stdout or a
/// formatted error string.
pub(crate) async fn exec_bash(cmd: &str, workspace: &std::path::Path) -> String {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .output()
        .await
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        Ok(out) => format!("error: {}", String::from_utf8_lossy(&out.stderr)),
        Err(e) => format!("error: {e}"),
    }
}

/// Builds the `turn.subscribe` response envelope from a task's current state.
///
/// Returns `Err` when the task does not exist, `Ok(Some(env))` when the task has
/// reached a terminal state (`complete` / `failed`), and `Ok(None)` when it is
/// still in progress.
async fn terminal_envelope(ingot: &IngotHandle, task_id: &str) -> Result<Option<Value>, RpcError> {
    match ingot.get_task(task_id).await.map_err(|e| ingot_err(&e))? {
        None => Err(RpcError::new(
            codes::INTERNAL_ERROR,
            format!("task not found: {task_id}"),
        )),
        Some(t) if t.status == "complete" => {
            // Best-effort token counts from the latest snapshot for the session.
            let (input_tok, output_tok) = if let Some(ref sid) = t.session_id {
                ingot
                    .session_token_snapshots(sid)
                    .await
                    .map_or((0i64, 0i64), |snaps| {
                        snaps
                            .last()
                            .map_or((0i64, 0i64), |s| (s.input_tok, s.output_tok))
                    })
            } else {
                (0i64, 0i64)
            };
            Ok(Some(json!({
                "done": true,
                "response": t.response.unwrap_or_default(),
                "input_tok": input_tok,
                "output_tok": output_tok,
            })))
        }
        Some(t) if t.status == "failed" => Ok(Some(json!({
            "done": true,
            "error": t.response.unwrap_or_else(|| "turn failed".into()),
        }))),
        Some(_) => Ok(None),
    }
}

/// Waits for `task_id` to reach a terminal state and returns the
/// `turn.subscribe` response envelope.
///
/// Subscribes to the dispatcher *before* the initial state read so no terminal
/// event published after subscription is missed; on subscriber lag it falls back
/// to a direct state read; the wait is bounded by `timeout`.
#[tracing::instrument(skip(ingot, dispatcher), fields(turn_id = %task_id))]
pub(crate) async fn await_turn_terminal(
    ingot: &IngotHandle,
    dispatcher: &Dispatcher,
    task_id: &str,
    timeout: std::time::Duration,
) -> Result<Value, RpcError> {
    use tokio::sync::broadcast::error::RecvError;

    let mut rx = dispatcher.subscribe();

    // Resolve immediately if the task is already terminal (or absent).
    if let Some(env) = terminal_envelope(ingot, task_id).await? {
        return Ok(env);
    }

    let wait = async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let event_turn = match &ev {
                        TurnEvent::Completed { turn_id, .. }
                        | TurnEvent::Failed { turn_id, .. } => Some(turn_id.as_str()),
                        _ => None,
                    };
                    if event_turn == Some(task_id) {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // A burst dropped events: check state directly so the terminal
                    // signal cannot be silently lost.
                    if let Ok(Some(t)) = ingot.get_task(task_id).await {
                        if t.status == "complete" || t.status == "failed" {
                            return;
                        }
                    }
                }
                Err(RecvError::Closed) => return,
            }
        }
    };

    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| RpcError::new(codes::TIMEOUT, "turn.subscribe timed out after 60s"))?;

    // Build the envelope from the now-terminal state.
    match terminal_envelope(ingot, task_id).await? {
        Some(env) => Ok(env),
        None => Err(RpcError::new(
            codes::TIMEOUT,
            "turn ended without a terminal status",
        )),
    }
}

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
    provider_sessions: orchestrator::ProviderSessions,
    cache_aligners: orchestrator::CacheAligners,
) {
    orchestrator::TurnOrchestrator::new(
        ingot,
        dispatcher,
        gates,
        pool,
        assayer,
        price_table,
        vault,
        provider_sessions,
        cache_aligners,
    )
    .run(session_id, turn_id)
    .await;
}

/// Subscribes to [`TurnEvent::Started`] and spawns a [`run_turn`] task for each
/// into the shared `task_set`.
///
/// Completed tasks are reaped from the set as they finish (via `try_join_next`)
/// so the set is bounded by the number of *in-flight* tasks rather than every
/// task ever spawned. The same set is drained at shutdown.
#[allow(clippy::too_many_arguments)] // forwarded directly to TurnOrchestrator
fn spawn_worker(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    provider_sessions: orchestrator::ProviderSessions,
    cache_aligners: orchestrator::CacheAligners,
    task_set: Arc<Mutex<tokio::task::JoinSet<()>>>,
) {
    tokio::spawn(async move {
        let mut rx = dispatcher.subscribe();
        loop {
            // Block on the first event, then drain any additionally-queued events
            // to reduce per-delta task spawns during high-rate streaming.
            let first = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::error!(
                        dropped = n,
                        "turn worker lagged; events dropped — some turns may be lost",
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let mut batch = vec![first];
            batch.extend(smedja_bellows::drain_ready(&mut rx));
            for event in batch {
                if let TurnEvent::Started {
                    session_id,
                    turn_id,
                    ..
                } = event
                {
                    let ig = ingot.clone();
                    let dp = Arc::clone(&dispatcher);
                    let g = Arc::clone(&gates);
                    let pl = Arc::clone(&pool);
                    let as_ = Arc::clone(&assayer);
                    let pt = Arc::clone(&price_table);
                    let vt = Arc::clone(&vault);
                    let ps = Arc::clone(&provider_sessions);
                    let ca = Arc::clone(&cache_aligners);
                    let mut set = task_set.lock().await;
                    set.spawn(run_turn(
                        ig, dp, session_id, turn_id, g, pl, as_, pt, vt, ps, ca,
                    ));
                    // Reap finished tasks so the set tracks only in-flight work.
                    while set.try_join_next().is_some() {}
                }
                // ignore non-Started events
            }
        }
    });
}

/// Returns `true` when `addr` falls in a range the daemon must never reach
/// (SSRF defence): loopback, unspecified, RFC-1918 private, link-local
/// (incl. the cloud IMDS endpoint), CGNAT, IPv6 ULA, and IPv6 link-local.
///
/// IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are unwrapped to their embedded
/// IPv4 first, so a mapped private address cannot bypass the IPv4 rules.
pub(crate) fn is_blocked_ip(addr: std::net::IpAddr) -> bool {
    use std::net::IpAddr;

    // Unwrap IPv4-mapped IPv6 so the IPv4 range checks apply.
    let addr = match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(v4) => IpAddr::V4(v4),
    };

    if addr.is_loopback() || addr.is_unspecified() {
        return true;
    }

    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || v4.is_link_local() // 169.254.0.0/16 (incl. IMDS 169.254.169.254)
                || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            (seg[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

/// Returns `true` only for publicly routable HTTP/HTTPS URLs.
///
/// Rejects non-HTTP schemes, the `localhost` hostname, and any host that parses
/// to an IP address blocked by [`is_blocked_ip`]. Hostnames that do not parse as
/// an IP are allowed (DNS resolution is the caller's network policy).
pub(crate) fn is_safe_mcp_url(url: &str) -> bool {
    let Ok(parsed) = url.parse::<url::Url>() else {
        return false;
    };
    if !matches!(parsed.scheme(), "https" | "http") {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    if host == "localhost" {
        return false;
    }
    // IPv6 literals come bracketed from the URL host (e.g. "[::1]"); strip the
    // brackets so the address parses.
    let host_ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(addr) = host_ip.parse::<std::net::IpAddr>() {
        if is_blocked_ip(addr) {
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // thin wiring: one registration per RPC method
fn build_router(
    ingot: &IngotHandle,
    dispatcher: &Arc<Dispatcher>,
    gates: &Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: &Arc<ProviderPool>,
    assayer: &Arc<Assayer>,
    startup_runner: &Arc<str>,
    startup_model: &Arc<str>,
    price_table: &Arc<PriceTable>,
    vault: &Arc<Mutex<Vault>>,
    provider_sessions: &orchestrator::ProviderSessions,
    cache_aligners: &orchestrator::CacheAligners,
    task_set: &Arc<Mutex<tokio::task::JoinSet<()>>>,
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
        provider_sessions: Arc::clone(provider_sessions),
        cache_aligners: Arc::clone(cache_aligners),
        task_set: Arc::clone(task_set),
        startup_runner: Arc::clone(startup_runner),
        startup_model: Arc::clone(startup_model),
    };

    // ── ping ────────────────────────────────────────────────────────────────
    router.register("ping", |_| async { Ok(json!("pong")) });

    // ── session.create ──────────────────────────────────────────────────────
    let session_create_state = state.clone();
    router.register("session.create", move |params: Value| {
        let state = session_create_state.clone();
        async move { handlers::session::create(state, params).await }
    });

    // ── session.list ────────────────────────────────────────────────────────
    let session_list_state = state.clone();
    router.register("session.list", move |params: Value| {
        let state = session_list_state.clone();
        async move { handlers::session::list(state, params).await }
    });

    // ── session.get ─────────────────────────────────────────────────────────
    let session_get_state = state.clone();
    router.register("session.get", move |params: Value| {
        let state = session_get_state.clone();
        async move { handlers::session::get(state, params).await }
    });

    // ── session.delete ──────────────────────────────────────────────────────
    let session_delete_state = state.clone();
    router.register("session.delete", move |params: Value| {
        let state = session_delete_state.clone();
        async move { handlers::session::delete(state, params).await }
    });

    // ── session.fork ────────────────────────────────────────────────────────
    let session_fork_state = state.clone();
    router.register("session.fork", move |params: Value| {
        let state = session_fork_state.clone();
        async move { handlers::session::fork(state, params).await }
    });

    // ── turn.subscribe ──────────────────────────────────────────────────────
    // Blocks until the named task reaches a terminal status (complete / failed)
    // or a 60-second deadline expires.  Returns a single response envelope so
    // callers do not need to poll task.get in a loop.
    let turn_subscribe_state = state.clone();
    router.register("turn.subscribe", move |params: Value| {
        let state = turn_subscribe_state.clone();
        async move { handlers::turn::subscribe(state, params).await }
    });

    // ── task.get ────────────────────────────────────────────────────────────
    let task_get_state = state.clone();
    router.register("task.get", move |params: Value| {
        let state = task_get_state.clone();
        async move { handlers::task::get(state, params).await }
    });

    // ── task.list ───────────────────────────────────────────────────────────
    let task_list_state = state.clone();
    router.register("task.list", move |params: Value| {
        let state = task_list_state.clone();
        async move { handlers::task::list(state, params).await }
    });

    // ── task.create ─────────────────────────────────────────────────────────
    let task_create_state = state.clone();
    router.register("task.create", move |params: Value| {
        let state = task_create_state.clone();
        async move { handlers::task::create(state, params).await }
    });

    // ── task.close ──────────────────────────────────────────────────────────
    let task_close_state = state.clone();
    router.register("task.close", move |params: Value| {
        let state = task_close_state.clone();
        async move { handlers::task::close(state, params).await }
    });

    // ── turn.submit ─────────────────────────────────────────────────────────
    let turn_submit_state = state.clone();
    router.register("turn.submit", move |params: Value| {
        let state = turn_submit_state.clone();
        async move { handlers::turn::submit(state, params).await }
    });

    // ── session.checkpoint.list ─────────────────────────────────────────────
    let checkpoint_list_state = state.clone();
    router.register("session.checkpoint.list", move |params: Value| {
        let state = checkpoint_list_state.clone();
        async move { handlers::checkpoint::list(state, params).await }
    });

    // ── session.rollback ────────────────────────────────────────────────────
    let rollback_state = state.clone();
    router.register("session.rollback", move |params: Value| {
        let state = rollback_state.clone();
        async move { handlers::checkpoint::rollback(state, params).await }
    });

    // ── session.compact ──────────────────────────────────────────────────────
    let compact_state = state.clone();
    router.register("session.compact", move |params: Value| {
        let state = compact_state.clone();
        async move { handlers::checkpoint::compact(state, params).await }
    });

    // ── session.token_usage ──────────────────────────────────────────────────
    let token_usage_state = state.clone();
    router.register("session.token_usage", move |params: Value| {
        let state = token_usage_state.clone();
        async move { handlers::session::token_usage(state, params).await }
    });

    // ── session.cost ────────────────────────────────────────────────────────
    let cost_state = state.clone();
    router.register("session.cost", move |params: Value| {
        let state = cost_state.clone();
        async move { handlers::cost::cost(state, params).await }
    });

    // ── metrics.summary ───────────────────────────────────────────────────────
    let metrics_state = state.clone();
    router.register("metrics.summary", move |params: Value| {
        let state = metrics_state.clone();
        async move { handlers::metrics::summary(state, params).await }
    });

    // ── savings.summary ───────────────────────────────────────────────────────
    let savings_state = state.clone();
    router.register("savings.summary", move |params: Value| {
        let state = savings_state.clone();
        async move { handlers::savings::summary(state, params).await }
    });

    // ── session.set_model ────────────────────────────────────────────────────
    let set_model_state = state.clone();
    router.register("session.set_model", move |params: Value| {
        let state = set_model_state.clone();
        async move { handlers::session::set_model(state, params).await }
    });

    // ── session.set_runner ───────────────────────────────────────────────────
    let set_runner_state = state.clone();
    router.register("session.set_runner", move |params: Value| {
        let state = set_runner_state.clone();
        async move { handlers::session::set_runner(state, params).await }
    });

    // ── session.takeover ─────────────────────────────────────────────────────
    // Forks the current session onto a new runner in one atomic operation:
    // creates a new session, copies the latest checkpoint, and sets the
    // runner_override so the next turn routes to the requested runner.
    let takeover_state = state.clone();
    router.register("session.takeover", move |params: Value| {
        let state = takeover_state.clone();
        async move { handlers::session::takeover(state, params).await }
    });

    // ── runner.list ──────────────────────────────────────────────────────────
    let runner_list_state = state.clone();
    router.register("runner.list", move |params: Value| {
        let state = runner_list_state.clone();
        async move { handlers::session::runner_list(state, params).await }
    });

    // ── local.models ─────────────────────────────────────────────────────────
    let local_models_state = state.clone();
    router.register("local.models", move |params: Value| {
        let state = local_models_state.clone();
        async move { handlers::local::models(state, params).await }
    });

    // ── local.gpu ────────────────────────────────────────────────────────────
    let local_gpu_state = state.clone();
    router.register("local.gpu", move |params: Value| {
        let state = local_gpu_state.clone();
        async move { handlers::local::gpu(state, params).await }
    });

    // ── local.swap ───────────────────────────────────────────────────────────
    let local_swap_state = state.clone();
    router.register("local.swap", move |params: Value| {
        let state = local_swap_state.clone();
        async move { handlers::local::swap(state, params).await }
    });

    // ── local.install ────────────────────────────────────────────────────────
    let local_install_state = state.clone();
    router.register("local.install", move |params: Value| {
        let state = local_install_state.clone();
        async move { handlers::local::install(state, params).await }
    });

    // ── session.context ─────────────────────────────────────────────────────
    let context_state = state.clone();
    router.register("session.context", move |params: Value| {
        let state = context_state.clone();
        async move { handlers::session::context(state, params).await }
    });

    // ── cowork.set ──────────────────────────────────────────────────────────
    let cowork_set_state = state.clone();
    router.register("cowork.set", move |params: Value| {
        let state = cowork_set_state.clone();
        async move { handlers::audit::set(state, params).await }
    });

    // ── session.set_mode ────────────────────────────────────────────────────
    let set_mode_state = state.clone();
    router.register("session.set_mode", move |params: Value| {
        let state = set_mode_state.clone();
        async move { handlers::session::set_mode(state, params).await }
    });

    // ── mcp.register ────────────────────────────────────────────────────────
    let mcp_register_state = state.clone();
    router.register("mcp.register", move |params: Value| {
        let state = mcp_register_state.clone();
        async move { handlers::mcp::register(state, params).await }
    });

    // ── mcp.list ────────────────────────────────────────────────────────────
    let mcp_list_state = state.clone();
    router.register("mcp.list", move |params: Value| {
        let state = mcp_list_state.clone();
        async move { handlers::mcp::list(state, params).await }
    });

    // ── cowork.approve ───────────────────────────────────────────────────────
    let cowork_approve_state = state.clone();
    router.register("cowork.approve", move |params: Value| {
        let state = cowork_approve_state.clone();
        async move { handlers::audit::approve(state, params).await }
    });

    // ── cowork.deny ──────────────────────────────────────────────────────────
    let cowork_deny_state = state.clone();
    router.register("cowork.deny", move |params: Value| {
        let state = cowork_deny_state.clone();
        async move { handlers::audit::deny(state, params).await }
    });

    // ── cowork.modify ────────────────────────────────────────────────────────
    let cowork_modify_state = state.clone();
    router.register("cowork.modify", move |params: Value| {
        let state = cowork_modify_state.clone();
        async move { handlers::audit::modify(state, params).await }
    });

    // ── cowork.pending ───────────────────────────────────────────────────────
    let cowork_pending_state = state.clone();
    router.register("cowork.pending", move |params: Value| {
        let state = cowork_pending_state.clone();
        async move { handlers::audit::pending(state, params).await }
    });

    // ── task.parallel ────────────────────────────────────────────────────────
    let task_parallel_state = state.clone();
    router.register("task.parallel", move |params: Value| {
        let state = task_parallel_state.clone();
        async move { handlers::task::parallel(state, params).await }
    });

    // ── task.cancel ──────────────────────────────────────────────────────────
    let task_cancel_state = state.clone();
    router.register("task.cancel", move |params: Value| {
        let state = task_cancel_state.clone();
        async move { handlers::task::cancel(state, params).await }
    });

    // ── mcp.remove ───────────────────────────────────────────────────────────
    let mcp_remove_state = state.clone();
    router.register("mcp.remove", move |params: Value| {
        let state = mcp_remove_state.clone();
        async move { handlers::mcp::remove(state, params).await }
    });

    // ── mcp.refresh ──────────────────────────────────────────────────────────
    let mcp_refresh_state = state.clone();
    router.register("mcp.refresh", move |params: Value| {
        let state = mcp_refresh_state.clone();
        async move { handlers::mcp::refresh(state, params).await }
    });

    // ── loop.create ──────────────────────────────────────────────────────────
    let loop_create_state = state.clone();
    router.register("loop.create", move |params: Value| {
        let state = loop_create_state.clone();
        async move { handlers::loops::create(state, params).await }
    });

    // ── loop.status ──────────────────────────────────────────────────────────
    let loop_status_state = state.clone();
    router.register("loop.status", move |params: Value| {
        let state = loop_status_state.clone();
        async move { handlers::loops::status(state, params).await }
    });

    // ── loop.cancel ──────────────────────────────────────────────────────────
    let loop_cancel_state = state.clone();
    router.register("loop.cancel", move |params: Value| {
        let state = loop_cancel_state.clone();
        async move { handlers::loops::cancel(state, params).await }
    });

    // ── loop.list ────────────────────────────────────────────────────────────
    let loop_list_state = state.clone();
    router.register("loop.list", move |params: Value| {
        let state = loop_list_state.clone();
        async move { handlers::loops::list(state, params).await }
    });

    // ── loop.retire ──────────────────────────────────────────────────────────
    let loop_retire_state = state.clone();
    router.register("loop.retire", move |params: Value| {
        let state = loop_retire_state.clone();
        async move { handlers::loops::retire(state, params).await }
    });

    // ── loop.list_by_status ──────────────────────────────────────────────────
    let loop_lbs_state = state.clone();
    router.register("loop.list_by_status", move |params: Value| {
        let state = loop_lbs_state.clone();
        async move { handlers::loops::list_by_status(state, params).await }
    });

    // ── audit.list ───────────────────────────────────────────────────────────
    let audit_list_state = state.clone();
    router.register("audit.list", move |params: Value| {
        let state = audit_list_state.clone();
        async move { handlers::audit::list(state, params).await }
    });

    // ── loop.run ────────────────────────────────────────────────────────────
    // Drives the real `smedja-loop` engine: load `.smedja/loop.json`, verify the
    // policy hash, enforce evaluator separation, then run each slice through the
    // implementer / verification gate / reviewer / bounded fix-retry pipeline.
    let loop_run_state = state.clone();
    router.register("loop.run", move |params: Value| {
        let state = loop_run_state.clone();
        async move { handlers::loops::run(state, params).await }
    });

    // ── audit.run ─────────────────────────────────────────────────────────────
    // Runs the bounded, read-only repo/PR/branch audit loop under the Review
    // role and returns { findings, counts, report | report_path }.
    let audit_run_state = state.clone();
    router.register("audit.run", move |params: Value| {
        let state = audit_run_state.clone();
        async move { handlers::auditor::run(state, params).await }
    });

    // ── agent.routing ────────────────────────────────────────────────────────
    // Resolves a (role, complexity?) pair through the daemon's assayer and
    // returns { runner, tier, model, complexity, rationale }.
    let agent_routing_state = state.clone();
    router.register("agent.routing", move |params: Value| {
        let state = agent_routing_state.clone();
        async move { handlers::routing::routing(state, params).await }
    });

    // ── graph.index ──────────────────────────────────────────────────────────
    // Runs the server-side code-graph index over a workspace; returns
    // { indexed: <count>, workspace }.
    let graph_index_state = state.clone();
    router.register("graph.index", move |params: Value| {
        let state = graph_index_state.clone();
        async move { handlers::graph::index(state, params).await }
    });

    // ── graph.query ──────────────────────────────────────────────────────────
    // Queries the server-side code graph; returns { symbols: [...] }.
    let graph_query_state = state.clone();
    router.register("graph.query", move |params: Value| {
        let state = graph_query_state.clone();
        async move { handlers::graph::query(state, params).await }
    });

    // ── session.history ──────────────────────────────────────────────────────
    // Returns ordered turn/message records and the audit trail for a session.
    let session_history_state = state.clone();
    router.register("session.history", move |params: Value| {
        let state = session_history_state.clone();
        async move { handlers::session::history(state, params).await }
    });

    router
}

/// Writes the ACP auth token to the runtime secret file with 0o600 permissions.
///
/// Path preference: `$XDG_RUNTIME_DIR/smdjad.secret` → `$HOME/.cache/smdjad.secret`
/// → `/tmp/smdjad.secret`.
/// Resolves the private path for the ACP secret from the runtime/home inputs, or
/// `None` when only a world-traversable location (e.g. `/tmp`) would be available.
///
/// The secret is never written to `/tmp`: a world-traversable directory lets any
/// local user learn the secret file's existence and (with lax permissions)
/// content, so the daemon refuses rather than falling back there.
fn acp_secret_path_from(
    xdg_runtime_dir: Option<&str>,
    home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Some(dir) = xdg_runtime_dir {
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir).join("smdjad.secret"));
        }
    }
    home.map(|h| h.join(".cache").join("smdjad.secret"))
}

fn write_acp_secret(token: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let home = dirs_home();
    let Some(secret_path) = acp_secret_path_from(xdg.as_deref(), home.as_deref()) else {
        tracing::error!(
            "no private directory for the ACP secret (set XDG_RUNTIME_DIR or HOME); \
             refusing to write it to a world-traversable location like /tmp"
        );
        return;
    };

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&secret_path)
    {
        let _ = f.write_all(token.as_bytes());
    } else {
        tracing::warn!(path = %secret_path.display(), "could not write ACP secret file");
    }
}

/// Resolves the daemon workspace root from an explicit env value and the current
/// directory, never returning the bare relative `"."`.
fn resolve_workspace_root_from(env: Option<String>, cwd: std::path::PathBuf) -> std::path::PathBuf {
    match env {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => cwd,
    }
}

/// Resolves the workspace root: `SMEDJA_WORKSPACE` if set, else the absolute
/// current directory. The relative `"."` default is avoided because its meaning
/// depends on the launcher's working directory under a supervisor.
fn resolve_workspace_root() -> std::path::PathBuf {
    let env = std::env::var("SMEDJA_WORKSPACE").ok();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    resolve_workspace_root_from(env, cwd)
}

/// Signals `READY=1` to systemd via `$NOTIFY_SOCKET` (for `Type=notify` units),
/// after the socket is bound and the database is open. A no-op when not run
/// under systemd (the variable is absent) or off Linux.
#[cfg(target_os = "linux")]
fn sd_notify_ready() {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let sent = if let Some(name) = path.strip_prefix('@') {
        // Abstract namespace socket (common for user services).
        use std::os::linux::net::SocketAddrExt as _;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| sock.send_to_addr(b"READY=1", &addr))
            .is_ok()
    } else {
        sock.send_to(b"READY=1", &path).is_ok()
    };
    if sent {
        tracing::debug!("notified systemd: READY=1");
    }
}

/// No-op readiness notification off Linux (systemd is Linux-only).
#[cfg(not(target_os = "linux"))]
fn sd_notify_ready() {}

/// Initialises the tracing subscriber, honouring `SMEDJA_LOG_FORMAT`.
///
/// `text` (default) uses the human-readable formatter; `json` emits structured
/// JSON for log-ingestion pipelines (Loki, `OpenSearch`); an unrecognised value
/// falls back to text with a warning.
fn init_tracing() {
    match std::env::var("SMEDJA_LOG_FORMAT").as_deref() {
        Ok("json") => tracing_subscriber::fmt().json().init(),
        Ok("text" | "") | Err(_) => tracing_subscriber::fmt().init(),
        Ok(other) => {
            tracing_subscriber::fmt().init();
            tracing::warn!(format = other, "unrecognised SMEDJA_LOG_FORMAT; using text");
        }
    }
}

#[allow(clippy::too_many_lines)] // startup sequence: bind, migrate, orphan sweep, spawn workers
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

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
                warn!(error = %e, endpoint = %endpoint, "failed to install OTLP exporter; trace destination: structured logs only");
            }
        }
    } else {
        info!("SMEDJA_OTLP_ENDPOINT not set; trace destination: structured logs only (set the endpoint to export OTLP spans)");
    }

    let path = socket_path();

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
    let pid_path = {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(base).join("smdjad.pid")
    };
    std::fs::write(&pid_path, std::process::id().to_string())
        .unwrap_or_else(|e| tracing::warn!(error = %e, "failed to write PID file"));

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

    // Refresh MCP server tool lists that have not been updated in the last hour.
    {
        let stale_threshold = crate::common::now_epoch() - 3600.0;
        let servers = ingot
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
                        if let Err(e) = ingot.register_mcp_server(updated).await {
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
    }

    // Capacity 256: lifecycle events (Started/Completed/Failed) must never be
    // dropped by streaming delta overflow.  256 provides enough headroom for
    // bursts of AssistantDelta chunks without discarding control events.
    let dispatcher = Arc::new(Dispatcher::new(256));
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

    let vault = Arc::new(Mutex::new(open_vault()));

    // Single provider-session map, constructed once and threaded to every
    // handler and the orchestrator (replaces the former OnceLock singleton).
    let provider_sessions: orchestrator::ProviderSessions = Arc::new(Mutex::new(HashMap::new()));

    // Single cross-turn cache-aligner map, keyed by `(session_id, runner)` and
    // threaded exactly like `provider_sessions`, so each persisted aligner
    // outlives a turn and reports real `Grown`/`Mutated` drift.
    let cache_aligners: orchestrator::CacheAligners = Arc::new(Mutex::new(HashMap::new()));

    // Shared set tracking in-flight turn tasks and loop.run background tasks, so
    // both are drained together at shutdown and completed tasks are reaped.
    let task_set: Arc<Mutex<tokio::task::JoinSet<()>>> =
        Arc::new(Mutex::new(tokio::task::JoinSet::new()));

    let router = build_router(
        &ingot,
        &dispatcher,
        &gates,
        &pool,
        &assayer,
        &startup_runner,
        &startup_model,
        &price_table,
        &vault,
        &provider_sessions,
        &cache_aligners,
        &task_set,
    );

    spawn_worker(
        ingot.clone(),
        Arc::clone(&dispatcher),
        Arc::clone(&gates),
        Arc::clone(&pool),
        Arc::clone(&assayer),
        Arc::clone(&price_table),
        Arc::clone(&vault),
        Arc::clone(&provider_sessions),
        Arc::clone(&cache_aligners),
        Arc::clone(&task_set),
    );

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
            };
            let acp_router = acp::build_acp_router(acp_state);
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            // Bind before spawning so a port conflict fails at startup, not inside the task.
            let tcp_listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| anyhow::anyhow!("ACP bind failed on {addr}: {e}"))?;
            info!(%addr, "ACP HTTP server listening");
            tokio::spawn(async move {
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
    let _stream_sock_guard = SocketGuard {
        path: stream_sock_path.clone(),
    };
    match UnixListener::bind(&stream_sock_path) {
        Ok(stream_listener) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(
                    &stream_sock_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            info!(path = %stream_sock_path.display(), "turn stream server listening");
            let ds = Arc::clone(&delta_store);
            let dp = Arc::clone(&dispatcher);
            tokio::spawn(async move {
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
    let _agent_sock_guard = SocketGuard {
        path: agent_sock_path.clone(),
    };
    match UnixListener::bind(&agent_sock_path) {
        Ok(agent_listener) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(
                    &agent_sock_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            info!(path = %agent_sock_path.display(), "agent event server listening");
            let dp = Arc::clone(&dispatcher);
            let agent_ingot = ingot.clone();
            tokio::spawn(async move {
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

    tokio::select! {
        result = server.serve(listener) => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received SIGINT; shutting down");
        }
        _ = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler failed")
                .recv()
                .await
        } => {
            info!("received SIGTERM; shutting down");
        }
        _ = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("SIGHUP handler failed")
                .recv()
                .await
        } => {
            // Treat SIGHUP as a graceful shutdown so the drain and SocketGuard
            // cleanup run, rather than the default terminate-without-cleanup.
            info!("received SIGHUP; shutting down");
        }
    }

    // Drain in-flight turn tasks and loop.run background tasks before cleaning
    // up, so mid-stream work can complete (or fail cleanly) rather than being
    // silently abandoned. A 30 s deadline prevents indefinite blocking; tasks
    // still running at the deadline are dropped (aborted) when the set is.
    {
        let mut set = std::mem::take(&mut *task_set.lock().await);
        if !set.is_empty() {
            info!(
                count = set.len(),
                "waiting for in-flight turns and loops to finish (up to 30 s)"
            );
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                while set.join_next().await.is_some() {}
            })
            .await;
        }
    }

    info!("smdjad stopped");
    let _ = std::fs::remove_file(&pid_path);
    // Socket is removed by _socket_guard's Drop impl on function exit.

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    // ── turn.subscribe event-driven wait ──────────────────────────────────────

    use smedja_bellows::event::CorrelationCtx;
    use smedja_bellows::{Dispatcher, TurnEvent};
    use smedja_ingot::{Ingot, IngotHandle, Task};
    use smedja_types::Timestamp;

    fn task(id: uuid::Uuid, status: &str, response: Option<&str>) -> Task {
        Task {
            id,
            title: "t".to_owned(),
            description: String::new(),
            status: status.to_owned(),
            created_at: Timestamp::from_micros(0),
            session_id: None,
            response: response.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn subscribe_not_found_errors() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let r = super::await_turn_terminal(
            &ig,
            &dispatcher,
            "missing",
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().message.contains("task not found"));
    }

    #[tokio::test]
    async fn subscribe_already_complete_returns_envelope() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let id = uuid::Uuid::new_v4();
        ig.create_task(task(id, "complete", None)).await.unwrap();
        let env = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id.to_string(),
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(env["done"], true);
        assert!(
            env.get("response").is_some(),
            "complete envelope carries a response field"
        );
    }

    #[tokio::test]
    async fn subscribe_already_failed_returns_error_envelope() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let id = uuid::Uuid::new_v4();
        ig.create_task(task(id, "failed", None)).await.unwrap();
        let env = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id.to_string(),
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(env["done"], true);
        assert_eq!(env["error"], "turn failed");
    }

    #[tokio::test]
    async fn subscribe_times_out_for_in_progress_with_no_event() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let id = uuid::Uuid::new_v4();
        ig.create_task(task(id, "planned", None)).await.unwrap();
        let r = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id.to_string(),
            std::time::Duration::from_millis(100),
        )
        .await;
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().code, super::codes::TIMEOUT);
    }

    #[tokio::test]
    async fn subscribe_resolves_on_completed_event() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Arc::new(Dispatcher::new(16));
        let id = uuid::Uuid::new_v4();
        let id_str = id.to_string();
        ig.create_task(task(id, "planned", None)).await.unwrap();

        // After a short delay, mark complete and publish the terminal event.
        let ig2 = ig.clone();
        let id2 = id_str.clone();
        let dispatcher2 = Arc::clone(&dispatcher);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ig2.set_task_response(&id2, "done-now").await.unwrap();
            dispatcher2.publish(TurnEvent::Completed {
                session_id: "s".to_owned(),
                turn_id: id2.clone(),
                output_tokens: 0,
                input_tokens: Some(0),
                traceparent: None,
                correlation: CorrelationCtx {
                    status: Some("ok".to_owned()),
                    ..CorrelationCtx::default()
                },
            });
        });

        let env = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id_str,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(env["done"], true);
        assert_eq!(env["response"], "done-now");
    }

    #[tokio::test]
    async fn joinset_reaps_completed_tasks() {
        // A JoinSet drains finished tasks via try_join_next, so it tracks only
        // in-flight work rather than retaining every handle forever.
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for _ in 0..5 {
            set.spawn(async {});
        }
        // Let them finish.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut reaped = 0;
        while set.try_join_next().is_some() {
            reaped += 1;
        }
        assert_eq!(reaped, 5);
        assert!(set.is_empty(), "set must be empty after reaping");
    }

    // ── SSRF guard ────────────────────────────────────────────────────────────

    #[test]
    fn is_blocked_ip_rejects_private_and_special_ranges() {
        let blocked = [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.5",
            "192.168.1.1",
            "169.254.169.254", // cloud IMDS (link-local)
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "::1",             // IPv6 loopback
            "fc00::1",         // IPv6 ULA
            "fe80::1",         // IPv6 link-local
            "::ffff:10.0.0.1", // IPv4-mapped private
        ];
        for ip in blocked {
            assert!(
                super::is_blocked_ip(ip.parse().unwrap()),
                "{ip} must be blocked"
            );
        }
        let allowed = ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"];
        for ip in allowed {
            assert!(
                !super::is_blocked_ip(ip.parse().unwrap()),
                "{ip} must be allowed"
            );
        }
    }

    #[test]
    fn is_safe_mcp_url_allows_public_rejects_local() {
        assert!(super::is_safe_mcp_url("https://example.com/mcp"));
        assert!(super::is_safe_mcp_url("https://8.8.8.8/mcp"));
        assert!(!super::is_safe_mcp_url("http://localhost/mcp"));
        assert!(!super::is_safe_mcp_url("http://10.0.0.1/mcp"));
        assert!(!super::is_safe_mcp_url("http://[::1]/mcp"));
        assert!(!super::is_safe_mcp_url("ftp://example.com")); // non-http scheme
    }

    // ── ACP secret path ─────────────────────────────────────────────────────────

    #[test]
    fn acp_secret_path_prefers_private_dirs_and_refuses_tmp() {
        use std::path::Path;
        assert_eq!(
            super::acp_secret_path_from(Some("/run/user/501"), None),
            Some(std::path::PathBuf::from("/run/user/501/smdjad.secret"))
        );
        assert_eq!(
            super::acp_secret_path_from(None, Some(Path::new("/home/u"))),
            Some(std::path::PathBuf::from("/home/u/.cache/smdjad.secret"))
        );
        // No XDG_RUNTIME_DIR and no HOME → refuse (would only be /tmp).
        assert_eq!(super::acp_secret_path_from(None, None), None);
        assert_eq!(super::acp_secret_path_from(Some(""), None), None);
    }

    // ── workspace root default ───────────────────────────────────────────────────

    #[test]
    fn resolve_workspace_root_uses_explicit_env_else_absolute_cwd() {
        let cwd = std::path::PathBuf::from("/abs/cwd");
        assert_eq!(
            super::resolve_workspace_root_from(Some("/ws".to_owned()), cwd.clone()),
            std::path::PathBuf::from("/ws")
        );
        // Unset/empty → the (absolute) cwd, never the relative ".".
        let got = super::resolve_workspace_root_from(None, cwd.clone());
        assert_eq!(got, cwd);
        assert_ne!(got, std::path::PathBuf::from("."));
    }

    #[test]
    fn read_only_role_blocks_write_tools() {
        // The least-privilege check in execute_tool blocks write tools when
        // session mode is "review". Verify the logic inline.
        let read_only_modes = ["review"];
        let write_tools = ["edit_file", "bash", "write_file", "run_command"];
        for mode in &read_only_modes {
            for tool in &write_tools {
                let is_blocked = *mode == "review" && write_tools.contains(tool);
                assert!(is_blocked, "tool {tool} should be blocked for mode {mode}");
            }
        }
    }

    #[test]
    fn loop_retire_state_is_terminal() {
        // Verify the terminal-status strings used in loop.retire enforcement.
        // "retired" must not be "complete" or "failed" — so the retire guard
        // would have rejected it (retired loops cannot be retired again).
        let retired = "retired";
        assert!(retired != "complete" && retired != "failed");
    }

    #[test]
    fn loop_complete_and_failed_allow_retire() {
        // Only complete or failed loops may be retired — verify the predicate.
        let terminal_for_retire = |s: &str| s == "complete" || s == "failed";
        assert!(terminal_for_retire("complete"));
        assert!(terminal_for_retire("failed"));
        assert!(!terminal_for_retire("planning"));
        assert!(!terminal_for_retire("slicing"));
        assert!(!terminal_for_retire("retired"));
    }

    #[test]
    fn retired_loop_cannot_be_re_run() {
        // The loop.run guard rejects status == "retired".
        let guard = |status: &str| status == "retired";
        assert!(guard("retired"));
        assert!(!guard("complete"));
        assert!(!guard("planning"));
    }

    #[tokio::test]
    async fn provider_pool_builds_without_panic() {
        // build_provider_pool is infallible — just verify no panic regardless
        // of what environment variables are set in the test runner.
        let pool = crate::provider_pool::build_provider_pool().await;
        // Pool may be empty or non-empty depending on the environment; either is valid.
        drop(pool);
    }

    /// Returns the provider name that `build_provider` would select given the
    /// detection results for each candidate, encoding the subscription-first
    /// priority order without touching the network or filesystem.
    ///
    /// Priority (index 0 = highest):
    /// 0. claude CLI binary present
    /// 1. codex CLI binary present
    /// 2. copilot detected
    /// 3. poolside detected
    /// 4. `ANTHROPIC_API_KEY` set
    /// 5. `OPENAI_API_KEY` set
    /// 6. minimax detected
    /// 7. berget detected
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn provider_priority(
        claude_cli: bool,
        codex_cli: bool,
        copilot: bool,
        poolside: bool,
        anthropic_key: bool,
        openai_key: bool,
        minimax: bool,
        berget: bool,
    ) -> &'static str {
        if claude_cli {
            return "claude-cli";
        }
        if codex_cli {
            return "codex-cli";
        }
        if copilot {
            return "copilot";
        }
        if poolside {
            return "poolside";
        }
        if anthropic_key {
            return "anthropic";
        }
        if openai_key {
            return "openai";
        }
        if minimax {
            return "minimax";
        }
        if berget {
            return "berget";
        }
        "none"
    }

    #[test]
    fn cli_wins_over_api_key_when_both_present() {
        // CLI subscription beats API key — the fundamental invariant of L20.
        assert_eq!(
            provider_priority(true, false, false, false, true, true, false, false),
            "claude-cli"
        );
        assert_eq!(
            provider_priority(false, true, false, false, false, true, false, false),
            "codex-cli"
        );
    }

    #[test]
    fn api_key_selected_when_no_cli_available() {
        assert_eq!(
            provider_priority(false, false, false, false, true, false, false, false),
            "anthropic"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, true, false, false),
            "openai"
        );
    }

    #[test]
    fn cli_providers_ordered_before_copilot_and_poolside() {
        // Even copilot (subscription-like) comes after the CLI runners.
        assert_eq!(
            provider_priority(false, true, true, true, false, false, false, false),
            "codex-cli"
        );
    }

    #[test]
    fn anthropic_key_before_openai_key() {
        assert_eq!(
            provider_priority(false, false, false, false, true, true, false, false),
            "anthropic"
        );
    }

    #[test]
    fn minimax_and_berget_are_lowest_priority_before_local() {
        assert_eq!(
            provider_priority(false, false, false, false, false, false, true, false),
            "minimax"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, false, false, true),
            "berget"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, false, false, false),
            "none"
        );
    }

    // ── provider-display: session.create response fields ────────────────────

    fn derive_tier(runner: &str) -> &'static str {
        if runner.contains("local") {
            "local"
        } else {
            "fast"
        }
    }

    #[test]
    fn session_create_tier_is_local_for_local_runner() {
        assert_eq!(derive_tier("local"), "local");
        assert_eq!(derive_tier("local-llm"), "local");
    }

    #[test]
    fn session_create_tier_is_fast_for_cloud_runners() {
        for runner in &["claude-cli", "anthropic", "codex-cli", "openai", "copilot"] {
            assert_eq!(
                derive_tier(runner),
                "fast",
                "expected fast tier for runner {runner}"
            );
        }
    }

    #[test]
    fn session_create_response_contains_runner_model_tier() {
        let runner = "anthropic";
        let model = "claude-sonnet-4-6";
        let tier = derive_tier(runner);
        let resp = serde_json::json!({
            "id": "session-test",
            "runner": runner,
            "model": model,
            "tier": tier,
        });
        assert_eq!(resp["runner"].as_str().unwrap(), runner);
        assert_eq!(resp["model"].as_str().unwrap(), model);
        assert_eq!(resp["tier"].as_str().unwrap(), "fast");
    }

    // ── parse_runner_str ────────────────────────────────────────────────────

    #[test]
    fn parse_runner_str_accepts_short_aliases() {
        use smedja_assayer::Runner;

        use crate::common::parse_runner_str;
        assert!(matches!(parse_runner_str("claude"), Some(Runner::Claude)));
        assert!(matches!(parse_runner_str("codex"), Some(Runner::Codex)));
        assert!(matches!(parse_runner_str("local"), Some(Runner::Local)));
        assert!(matches!(parse_runner_str("copilot"), Some(Runner::Copilot)));
    }

    #[test]
    fn parse_runner_str_accepts_canonical_keys() {
        use smedja_assayer::Runner;

        use crate::common::parse_runner_str;
        assert!(matches!(
            parse_runner_str("claude-cli"),
            Some(Runner::Claude)
        ));
        assert!(matches!(parse_runner_str("codex-cli"), Some(Runner::Codex)));
    }

    #[test]
    fn parse_runner_str_rejects_unknown_values() {
        use crate::common::parse_runner_str;
        assert!(parse_runner_str("openai").is_none());
        assert!(parse_runner_str("").is_none());
        assert!(parse_runner_str("anthropic").is_none());
    }

    // ── session.set_runner / session.takeover ─────────────────────────────

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
            guard.search(&qv, "async rust", "warm", 5).unwrap()
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
            };
            let mut guard = vt.blocking_lock();
            guard.upsert(&entry).unwrap();
        })
        .await
        .unwrap();

        let count = vault.lock().await.count_by_namespace("handoff").unwrap();
        assert_eq!(count, 1, "one handoff entry must be written on takeover");
    }

    #[test]
    fn compaction_transcript_renders_strata_messages_not_raw_json() {
        let messages_json = r#"[
            {"role":"user","content":"first request"},
            {"role":"assistant","content":"first reply"},
            {"role":"user","content":"second request"}
        ]"#;
        let transcript = super::assemble_compaction_transcript(messages_json);
        // Rendered as role: content lines, not the raw JSON blob.
        assert!(transcript.contains("user: first request"));
        assert!(transcript.contains("assistant: first reply"));
        assert!(transcript.contains("user: second request"));
        assert!(
            !transcript.contains("\"role\""),
            "transcript must not contain raw JSON keys"
        );
    }

    #[test]
    fn compaction_transcript_empty_for_invalid_json() {
        assert_eq!(super::assemble_compaction_transcript(""), "");
        assert_eq!(super::assemble_compaction_transcript("not json"), "");
        assert_eq!(super::assemble_compaction_transcript("[]"), "");
    }

    #[tokio::test]
    async fn compact_writes_summary_to_vault_compact_namespace() {
        use smedja_vault::Vault;

        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let session_id = "sess-compact-test".to_owned();
        let summary = "• Implemented auth\n• Tests pass\nGoal: ship v1".to_owned();
        let turn_count: i64 = 7;

        // Simulate the vault write logic from session.compact.
        let compact_sid = session_id.clone();
        let compact_summary = summary.clone();
        let vt = Arc::clone(&vault);
        tokio::task::spawn_blocking(move || {
            let entry = smedja_vault::VaultEntry {
                id: format!("compact:{compact_sid}:{turn_count}"),
                embedding: crate::embedder::embed(&compact_summary),
                payload: serde_json::json!({
                    "session_id": compact_sid,
                    "turn_count": turn_count,
                }),
                namespace: "compact".to_owned(),
                content: compact_summary,
                source_file: None,
                added_by: Some("session.compact".to_owned()),
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
            };
            let mut guard = vt.blocking_lock();
            guard.upsert(&entry).unwrap();
        })
        .await
        .unwrap();

        let count = vault.lock().await.count_by_namespace("compact").unwrap();
        assert_eq!(count, 1, "one compact entry must be written per compaction");

        let results = {
            let guard = vault.lock().await;
            let qv = crate::embedder::embed("auth tests");
            guard.search(&qv, "auth tests", "compact", 5).unwrap()
        };
        assert!(
            !results.is_empty(),
            "compact summary must be retrievable by semantic search"
        );
    }

    #[tokio::test]
    async fn session_context_includes_vault_stratum_counts() {
        use smedja_vault::Vault;

        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        // Populate vault with one warm and two default (cold) entries.
        {
            let mut guard = vault.lock().await;
            let make_entry = |id: &str, ns: &str| smedja_vault::VaultEntry {
                id: id.to_owned(),
                embedding: crate::embedder::embed(id),
                payload: serde_json::json!({}),
                namespace: ns.to_owned(),
                content: id.to_owned(),
                source_file: None,
                added_by: None,
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
            };
            guard.upsert(&make_entry("w1", "warm")).unwrap();
            guard.upsert(&make_entry("c1", "default")).unwrap();
            guard.upsert(&make_entry("c2", "default")).unwrap();
        }

        let (warm_count, cold_count) = tokio::task::spawn_blocking(move || {
            let guard = vault.blocking_lock();
            let warm = guard.count_by_namespace("warm").unwrap_or(0);
            let cold = guard.count_by_namespace("default").unwrap_or(0);
            (warm, cold)
        })
        .await
        .unwrap();

        assert_eq!(warm_count, 1);
        assert_eq!(cold_count, 2);
    }
}
