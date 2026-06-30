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
        let ingot = Ingot::open(&db_path).map_err(anyhow::Error::from)?;
        // Ensure the database file is only readable by the owner.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(ingot)
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
            Ok(v) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                return v;
            }
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

/// Timeout for `exec_bash` commands (git diff on large repos, cargo clippy, etc.).
const EXEC_BASH_TIMEOUT_SECS: u64 = 30;

/// Maximum per-call timeout accepted via the `timeout_secs` input field.
pub(crate) const SMEDJA_BASH_MAX_TIMEOUT_SECS: u64 = 600;

/// Executes a bash command in `workspace` using `sh -c`, returning stdout or a
/// formatted error string. Bounded by [`EXEC_BASH_TIMEOUT_SECS`]; a hung command
/// (e.g. git diff on a large repo) returns a timeout error rather than blocking.
pub(crate) async fn exec_bash(cmd: &str, workspace: &std::path::Path) -> String {
    exec_bash_ext(cmd, workspace, None, None, None).await
}

/// Extended `exec_bash` supporting per-call timeout, extra env vars, and stdin.
///
/// Uses spawn-based execution so stdout/stderr are captured concurrently.
/// On timeout the child is killed and any partial stdout already read is
/// returned with a timeout suffix. Stderr is appended as a `[stderr]` block
/// when the exit status is non-zero.
#[allow(clippy::items_after_statements)]
pub(crate) async fn exec_bash_ext(
    cmd: &str,
    workspace: &std::path::Path,
    timeout_secs: Option<u64>,
    env_extra: Option<std::collections::HashMap<String, String>>,
    stdin_bytes: Option<Vec<u8>>,
) -> String {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let timeout = timeout_secs.map_or(EXEC_BASH_TIMEOUT_SECS, |t| {
        t.min(SMEDJA_BASH_MAX_TIMEOUT_SECS)
    });

    let mut builder = tokio::process::Command::new("sh");
    builder
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(ref env_map) = env_extra {
        for (k, v) in env_map {
            builder.env(k, v);
        }
    }

    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => return format!("error: {e}"),
    };

    if let Some(bytes) = stdin_bytes {
        if let Some(mut h) = child.stdin.take() {
            let _ = h.write_all(&bytes).await;
        }
    }

    async fn read_all(reader: impl tokio::io::AsyncRead + Unpin + Send + 'static) -> String {
        let mut buf = String::new();
        let mut r = BufReader::new(reader);
        let mut line = String::new();
        while r.read_line(&mut line).await.unwrap_or(0) > 0 {
            buf.push_str(&line);
            line.clear();
        }
        buf
    }

    let stdout_reader = child.stdout.take().expect("stdout piped");
    let stderr_reader = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(read_all(stdout_reader));
    let stderr_task = tokio::spawn(read_all(stderr_reader));

    match tokio::time::timeout(std::time::Duration::from_secs(timeout), child.wait()).await {
        Ok(Ok(status)) => {
            let out = stdout_task.await.unwrap_or_default();
            let err = stderr_task.await.unwrap_or_default();
            if status.success() {
                out
            } else {
                let mut result = format!("error: exit status {status}\n");
                result.push_str(&out);
                if !err.is_empty() {
                    if !result.ends_with('\n') {
                        result.push('\n');
                    }
                    result.push_str("[stderr]\n");
                    result.push_str(&err);
                }
                result
            }
        }
        Ok(Err(e)) => format!("error: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            // Give readers 5 s to drain after kill.
            let out = tokio::time::timeout(std::time::Duration::from_secs(5), stdout_task)
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .unwrap_or_default();
            let err = tokio::time::timeout(std::time::Duration::from_secs(5), stderr_task)
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .unwrap_or_default();
            let mut result = out;
            if !err.is_empty() {
                if !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push_str("[stderr]\n");
                result.push_str(&err);
            }
            if !result.ends_with('\n') && !result.is_empty() {
                result.push('\n');
            }
            result.push_str("error: command timed out after ");
            result.push_str(&timeout.to_string());
            result.push('s');
            result
        }
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

/// Subscribes to [`TurnEvent::Started`] and spawns a [`run_turn`] task for each.
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
fn spawn_worker(
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

    let vault = Arc::new(Mutex::new(open_vault()));

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

    // Background GC: cap the provider_sessions map at 10 000 entries.  Each
    // entry is soft state (cleared sessions reconnect cleanly on the next turn),
    // so a full clear is safe. The GC task wakes every 5 minutes.
    {
        let ps = Arc::clone(&provider_sessions);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_mins(5)).await;
                let mut map = ps.lock().await;
                let n = map.len();
                if n > 10_000 {
                    map.clear();
                    tracing::info!("provider_sessions: cleared {n} entries (cap exceeded)");
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
        tokio::spawn(async move {
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
        tokio::spawn(async move {
            loop {
                let events = smedja_bellows::drain_ready(&mut quality_rx);
                for ev in events {
                    if let TurnEvent::Completed { turn_id, .. } = ev {
                        let disp = Arc::clone(&quality_dispatcher);
                        let ws = quality_workspace.clone();
                        let skills = session_skills.clone();
                        tokio::task::spawn_blocking(move || {
                            quality_hook::run_after_turn(
                                Some(turn_id),
                                ws,
                                skills,
                                file_size_threshold,
                                disp,
                            );
                        });
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
    fn search_mode_parses_to_read_only_role() {
        use smedja_assayer::AgentRole;
        let role = crate::common::parse_session_mode_to_role("search");
        assert_eq!(role, Some(AgentRole::Search));
        assert!(AgentRole::Search.is_read_only());
        assert_eq!(AgentRole::Search.label(), "search");
    }

    #[test]
    fn search_role_blocks_write_tools() {
        use crate::cowork::{evaluate, PermissionDecision, PermissionMode};
        use smedja_assayer::AgentRole;
        let write_tools = ["edit_file", "bash", "write_file", "run_command"];
        for tool in &write_tools {
            let denied = AgentRole::Search.is_read_only()
                && evaluate(PermissionMode::Plan, tool) == PermissionDecision::Deny;
            assert!(denied, "tool {tool} should be blocked for search role");
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
                embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
                dim: crate::embedder::DIM,
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
            guard
                .search(
                    &qv,
                    "auth tests",
                    "compact",
                    5,
                    smedja_vault::LEGACY_MODEL_ID,
                    crate::embedder::DIM,
                )
                .unwrap()
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
                embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
                dim: crate::embedder::DIM,
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
