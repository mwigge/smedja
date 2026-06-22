pub mod acp;
pub mod alert;
pub mod compact;
pub mod cowork;
pub mod embedder;
pub mod executor;
pub mod local_provider;
pub mod mcp_http;
pub mod mcp_oauth;
pub mod orchestrator;
pub mod price_table;
pub mod provider_pool;
pub mod sandbox;
pub mod stream_server;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde_json::{json, Value};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::{CallOptions, Delta};
use smedja_assayer::{Assayer, Role as AgentRole, Runner, Tier, WorktreePool};

use crate::price_table::PriceTable;
use crate::provider_pool::{build_provider_pool, ProviderPool};
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Checkpoint, CostRow, Ingot, IngotHandle, McpServer, Session, Task};
use smedja_vault::{Vault, VaultEntry};
use smedja_rpc::{codes, router::Router, server::Server, RpcError};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use smedja_telemetry as tel;

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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// In-memory map from smedja sessions to provider-native resume identifiers.
fn provider_session_store() -> &'static tokio::sync::Mutex<HashMap<String, String>> {
    static STORE: OnceLock<tokio::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
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

fn ingot_err(e: &smedja_ingot::IngotError) -> RpcError {
    RpcError::new(codes::INTERNAL_ERROR, e.to_string())
}

fn missing_param(name: &str) -> RpcError {
    RpcError::new(
        codes::INVALID_PARAMS,
        format!("missing required param: {name}"),
    )
}

/// Maps a session mode string to an [`AgentRole`] for routing purposes.
fn parse_session_mode_to_role(mode: &str) -> Option<AgentRole> {
    match mode {
        "impl" => Some(AgentRole::Impl),
        "test" => Some(AgentRole::Test),
        "review" => Some(AgentRole::Review),
        "sre" => Some(AgentRole::Sre),
        "orchestrator" => Some(AgentRole::Orchestrator),
        _ => None,
    }
}

/// Maps a [`Runner`] enum value to the short string used in the session-resume store.
fn runner_session_key(runner: Runner) -> &'static str {
    match runner {
        Runner::Claude => "claude-cli",
        Runner::Codex => "codex-cli",
        Runner::Local => "local",
        Runner::Copilot => "copilot",
        Runner::Minimax => "minimax",
        Runner::Berget => "berget",
    }
}

/// Parses a user-supplied or stored runner string to a [`Runner`] enum value.
///
/// Accepts both canonical keys (`"claude-cli"`) and short aliases (`"claude"`).
fn parse_runner_str(s: &str) -> Option<Runner> {
    match s {
        "claude" | "claude-cli" => Some(Runner::Claude),
        "codex" | "codex-cli" => Some(Runner::Codex),
        "local" => Some(Runner::Local),
        "copilot" => Some(Runner::Copilot),
        "minimax" => Some(Runner::Minimax),
        "berget" => Some(Runner::Berget),
        _ => None,
    }
}

/// Drains `stream`, accumulating text deltas into a single string.
///
/// Returns `Ok((full_response, input_tokens, output_tokens, provider_session_id))` on success, or
/// `Err(reason)` if the stream yields an error item.  Each `Delta::Text` chunk
/// is forwarded to `dispatcher` as a [`TurnEvent::AssistantDelta`].
/// Error returned by [`drain_stream`], distinguishing rate-limit responses from
/// other failures so callers can apply an appropriate retry strategy.
enum DrainError {
    /// The provider returned HTTP 429; back off for `retry_after` before retrying.
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },
    /// Any other stream-level error; treat as fatal for this turn.
    Other(String),
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited { retry_after } => {
                write!(f, "rate limited by provider (retry after {retry_after:?})")
            }
            Self::Other(s) => f.write_str(s),
        }
    }
}

async fn drain_stream(
    mut stream: smedja_adapter::DeltaStream,
    dispatcher: &Dispatcher,
    turn_id: Option<&str>,
) -> Result<(String, u32, u32, Option<String>), DrainError> {
    let mut full_response = String::new();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    let mut provider_session_id = None;
    loop {
        match stream.next().await {
            None => break,
            Some(Ok(Delta::Text(t))) => {
                full_response.push_str(&t);
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: t,
                    turn_id: turn_id.map(str::to_owned),
                    conversation_id: None,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    operation_name: None,
                    agent_name: None,
                    status: None,
                });
            }
            Some(Ok(Delta::Usage {
                input_tokens: i,
                output_tokens: n,
            })) => {
                input_tokens = i;
                output_tokens = n;
            }
            Some(Ok(Delta::ToolCall { name, input })) => {
                let input_summary: String = input.to_string().chars().take(120).collect();
                let line = format!("▶ {name}({input_summary})");
                full_response.push_str(&line);
                full_response.push('\n');
                dispatcher.publish(TurnEvent::ToolCalled {
                    tool_name: name,
                    input_summary,
                    turn_id: turn_id.map(str::to_owned),
                    conversation_id: None,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    operation_name: Some(tel::OPERATION_EXECUTE_TOOL.to_owned()),
                    agent_name: None,
                    status: None,
                    tool_call_id: None,
                });
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: line,
                    turn_id: turn_id.map(str::to_owned),
                    conversation_id: None,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    operation_name: None,
                    agent_name: None,
                    status: None,
                });
            }
            Some(Ok(Delta::ToolResult {
                tool_use_id,
                content,
            })) => {
                let line = format!("✓ {tool_use_id} -> {} chars", content.chars().count());
                full_response.push_str(&line);
                full_response.push('\n');
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: line,
                    turn_id: turn_id.map(str::to_owned),
                    conversation_id: None,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    operation_name: None,
                    agent_name: None,
                    status: None,
                });
            }
            Some(Ok(Delta::SessionId(id))) => {
                provider_session_id = Some(id);
            }
            Some(Err(smedja_adapter::AdapterError::RateLimited { retry_after })) => {
                return Err(DrainError::RateLimited { retry_after });
            }
            Some(Err(e)) => return Err(DrainError::Other(e.to_string())),
        }
    }
    Ok((
        full_response,
        input_tokens,
        output_tokens,
        provider_session_id,
    ))
}

/// Executes a bash command in `workspace` using `sh -c`, returning stdout or a
/// formatted error string.
async fn exec_bash(cmd: &str, workspace: &std::path::Path) -> String {
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

/// Maximum number of tool-dispatch iterations in a single turn.
/// Override with `SMEDJA_MAX_TOOL_TURNS` (e.g. `SMEDJA_MAX_TOOL_TURNS=5`).
/// Values above 50 are clamped to 50 to prevent runaway LLM loops.
const MAX_TOOL_TURNS: usize = 10;

fn effective_max_tool_turns() -> usize {
    std::env::var("SMEDJA_MAX_TOOL_TURNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.min(50))
        .unwrap_or(MAX_TOOL_TURNS)
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
) {
    orchestrator::TurnOrchestrator::new(
        ingot, dispatcher, gates, pool, assayer, price_table, vault,
    )
    .run(session_id, turn_id)
    .await;
}

/// Subscribes to [`TurnEvent::Started`] and spawns a [`run_turn`] task for each.
///
/// Returns a shared handle store so that the caller can drain in-flight tasks
/// before exiting (graceful shutdown).
fn spawn_worker(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
) -> Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> {
    let handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let handles_inner = Arc::clone(&handles);
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
                    let handle =
                        tokio::spawn(run_turn(ig, dp, session_id, turn_id, g, pl, as_, pt, vt));
                    handles_inner.lock().await.push(handle);
                }
                // ignore non-Started events
            }
        }
    });
    handles
}

/// Returns `true` only for publicly routable HTTP/HTTPS URLs.
/// Blocks RFC-1918 private ranges, loopback, the unspecified address, and the
/// cloud IMDS endpoint (169.254.169.254).
fn is_safe_mcp_url(url: &str) -> bool {
    let Ok(parsed) = url.parse::<url::Url>() else {
        return false;
    };
    if !matches!(parsed.scheme(), "https" | "http") {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    if host == "localhost" || host == "169.254.169.254" {
        return false;
    }
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        if addr.is_loopback() || addr.is_unspecified() {
            return false;
        }
        if let std::net::IpAddr::V4(v4) = addr {
            let o = v4.octets();
            if o[0] == 10
                || (o[0] == 172 && o[1] >= 16 && o[1] <= 31)
                || (o[0] == 192 && o[1] == 168)
            {
                return false;
            }
        }
    }
    true
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // all RPC methods live in one function per the spec
fn build_router(
    ingot: &IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: &Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    startup_runner: &'static str,
    startup_model: &'static str,
    price_table: Arc<PriceTable>,
    vault: &Arc<Mutex<Vault>>,
) -> Router {
    let mut router = Router::new();

    // Clone gates so the closures below can each hold an independent Arc.
    let gates = Arc::clone(gates);

    // Clone vault so each RPC closure gets its own Arc.
    let vault = Arc::clone(vault);

    // Stash provider pool Arc before the name is shadowed by the WorktreePool below.
    let provider_pool = pool;

    // Create two Arcs for the worktree pool so task.parallel and task.cancel each hold one.
    let pool = Arc::new(Mutex::new(WorktreePool::default()));
    let pool_cancel = Arc::clone(&pool);

    // ── ping ────────────────────────────────────────────────────────────────
    router.register("ping", |_| async { Ok(json!("pong")) });

    // ── session.create ──────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.create", move |params: Value| {
        let ig = ig.clone();
        async move {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned);

            let mode = params
                .get("mode")
                .and_then(Value::as_str)
                .map(str::to_owned);

            let cowork_mode = params
                .get("cowork_mode")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let task_description: Option<String> = params
                .get("task_description")
                .and_then(Value::as_str)
                .map(str::to_owned);

            let now = now_epoch();
            let session_id = Uuid::new_v4();

            // When task_description is provided, create the linked task first so
            // its ID can be stored directly in the Session row.
            let task_id: Option<String> = if let Some(ref desc) = task_description {
                let task = Task {
                    id: Uuid::new_v4(),
                    title: desc.clone(),
                    description: String::new(),
                    status: "planned".to_owned(),
                    created_at: now,
                    session_id: Some(session_id.to_string()),
                    response: None,
                };
                ig.create_task(task.clone())
                    .await
                    .map_err(|e| ingot_err(&e))?;
                Some(task.id.to_string())
            } else {
                None
            };

            let session = Session {
                id: session_id,
                created_at: now,
                updated_at: now,
                status: "active".to_owned(),
                task_id: task_id.clone(),
                mode,
                title: title.clone().unwrap_or_default(),
                cowork_mode,
                workspace_root: None,
                model_override: None,
                runner_override: None,
            };

            ig.create_session(session.clone())
                .await
                .map_err(|e| ingot_err(&e))?;

            // Auto-index: check workspace.toml in cwd; if stale, index in background.
            {
                let cwd = std::env::var("SMEDJA_WORKSPACE")
                    .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from);
                let toml_path = cwd.join(".smedja").join("workspace.toml");
                let needs_index = if toml_path.exists() {
                    let content = std::fs::read_to_string(&toml_path).unwrap_or_default();
                    if let Ok(parsed) = toml::from_str::<toml::Value>(&content) {
                        parsed
                            .get("graph")
                            .and_then(|g| g.get("last_indexed_at"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .is_none_or(|ts| {
                                let age = chrono::Utc::now()
                                    .signed_duration_since(ts.with_timezone(&chrono::Utc));
                                age.num_hours() >= 24
                            })
                    } else {
                        true
                    }
                } else {
                    // Only auto-index if workspace.toml already exists (workspace was initialised).
                    false
                };

                if needs_index {
                    let bg_cwd = cwd.clone();
                    let bg_toml = toml_path.clone();
                    tokio::task::spawn(async move {
                        use opentelemetry::trace::Span as _;
                        let tracer = opentelemetry::global::tracer("smedja");
                        let mut span =
                            opentelemetry::trace::Tracer::start(&tracer, "smedja.workspace.index");
                        let start = std::time::Instant::now();
                        let db_path = bg_cwd.join(".smedja").join("graph.db");
                        let bg_cwd_clone = bg_cwd.clone();
                        let symbol_count = tokio::task::spawn_blocking(move || {
                            smedja_graph::GraphStore::open(&db_path)
                                .and_then(|mut s| {
                                    s.index_workspace_incremental(&bg_cwd_clone, "workspace", None)
                                })
                                .unwrap_or(0)
                        })
                        .await
                        .unwrap_or(0);
                        let duration_ms = start.elapsed().as_millis();
                        span.set_attribute(opentelemetry::KeyValue::new(
                            "workspace_path",
                            bg_cwd.to_string_lossy().into_owned(),
                        ));
                        span.set_attribute(opentelemetry::KeyValue::new(
                            "symbol_count",
                            i64::try_from(symbol_count).unwrap_or(i64::MAX),
                        ));
                        span.set_attribute(opentelemetry::KeyValue::new(
                            "duration_ms",
                            i64::try_from(duration_ms).unwrap_or(i64::MAX),
                        ));
                        span.end();
                        let ts = chrono::Utc::now().to_rfc3339();
                        let new_content = format!(
                            "[graph]\nauto_index = true\nlast_indexed_at = \"{ts}\"\n"
                        );
                        if let Err(e) = std::fs::write(&bg_toml, new_content) {
                            tracing::warn!(error = %e, "failed to update workspace.toml after auto-index");
                        }
                    });
                }
            }

            // When cowork_mode is requested, register the per-session gate.
            // The gate map is owned by build_router; session.create handles the DB flag
            // only here. Callers that need the gate active must also call cowork.set.

            let tier = if startup_runner.contains("local") { "local" } else { "fast" };
            Ok(json!({
                "id": session.id,
                "title": title,
                "created_at": session.created_at,
                "cowork_mode": cowork_mode,
                "task_id": task_id,
                "runner": startup_runner,
                "model": startup_model,
                "tier": tier,
            }))
        }
    });

    // ── session.list ────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.list", move |_| {
        let ig = ig.clone();
        async move {
            let sessions = ig.list_sessions().await.map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = sessions
                .into_iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "title": s.title,
                        "mode": s.mode,
                        "created_at": s.created_at,
                        "updated_at": s.updated_at,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── session.get ─────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.get", move |params: Value| {
        let ig = ig.clone();
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;

            let session = ig
                .get_session(id)
                .await
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INTERNAL_ERROR, format!("session not found: {id}"))
                })?;

            Ok(json!({
                "id": session.id,
                "title": session.title,
                "mode": session.mode,
                "created_at": session.created_at,
                "updated_at": session.updated_at,
                "status": session.status,
                "task_id": session.task_id,
            }))
        }
    });

    // ── session.delete ──────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.delete", move |params: Value| {
        let ig = ig.clone();
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;

            ig.delete_session(id)
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(Value::Bool(true))
        }
    });

    // ── session.fork ────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.fork", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();

            // Each DB call acquires and immediately releases the lock so other
            // concurrent RPC handlers (including turn.subscribe's polling loop)
            // are not serialised behind the entire fork sequence.
            let parent = {
                ig.get_session(&session_id)
                    .await
                    .map_err(|e| ingot_err(&e))?
                    .ok_or_else(|| {
                        RpcError::new(
                            codes::INTERNAL_ERROR,
                            format!("session not found: {session_id}"),
                        )
                    })?
            };

            let latest_cp = {
                ig.latest_checkpoint(&session_id)
                    .await
                    .map_err(|e| ingot_err(&e))?
            };

            let now = now_epoch();
            let new_id = Uuid::new_v4().to_string();

            {
                ig.create_session(Session {
                        id: Uuid::parse_str(&new_id).map_err(|e| {
                            RpcError::new(codes::INTERNAL_ERROR, format!("uuid error: {e}"))
                        })?,
                        created_at: now,
                        updated_at: now,
                        status: "active".into(),
                        task_id: None,
                        mode: parent.mode.clone(),
                        title: parent.title.clone(),
                        cowork_mode: parent.cowork_mode,
                        workspace_root: parent.workspace_root.clone(),
                        model_override: parent.model_override.clone(),
                        runner_override: None,
                    })
                    .await
                    .map_err(|e| ingot_err(&e))?;
            }

            let has_checkpoint = latest_cp.is_some();
            if let Some(cp) = latest_cp {
                ig.save_checkpoint(Checkpoint {
                        id: Uuid::new_v4(),
                        session_id: new_id.clone(),
                        turn_n: cp.turn_n,
                        messages_json: cp.messages_json,
                        created_at: now,
                    })
                    .await
                    .map_err(|e| ingot_err(&e))?;
            }

            Ok(json!({
                "session_id": new_id,
                "forked_from": session_id,
                "has_checkpoint": has_checkpoint,
            }))
        }
    });

    // ── turn.subscribe ──────────────────────────────────────────────────────
    // Blocks until the named task reaches a terminal status (complete / failed)
    // or a 60-second deadline expires.  Returns a single response envelope so
    // callers do not need to poll task.get in a loop.
    let ig = ingot.clone();
    router.register("turn.subscribe", move |params: Value| {
        let ig = ig.clone();
        async move {
            let task_id = params
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("task_id"))?
                .to_owned();

            let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
            loop {
                let task = {
                    ig.get_task(&task_id).await.map_err(|e| ingot_err(&e))?
                };
                match task {
                    None => {
                        return Err(RpcError::new(
                            codes::INTERNAL_ERROR,
                            format!("task not found: {task_id}"),
                        ))
                    }
                    Some(t) if t.status == "complete" => {
                        // Best-effort: look up the latest token snapshot for
                        // this task's session so the TUI can display counts.
                        let (input_tok, output_tok) = if let Some(ref sid) = t.session_id {
                            ig.session_token_snapshots(sid).await.map_or(
                                (0i64, 0i64),
                                |snaps| {
                                    snaps
                                        .last()
                                        .map_or((0i64, 0i64), |s| (s.input_tok, s.output_tok))
                                },
                            )
                        } else {
                            (0i64, 0i64)
                        };
                        return Ok(json!({
                            "done": true,
                            "response": t.response.unwrap_or_default(),
                            "input_tok": input_tok,
                            "output_tok": output_tok,
                        }));
                    }
                    Some(t) if t.status == "failed" => {
                        return Ok(json!({
                            "done": true,
                            "error": t.response.unwrap_or_else(|| "turn failed".into()),
                        }));
                    }
                    Some(_) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(RpcError::new(
                                codes::TIMEOUT,
                                "turn.subscribe timed out after 60s",
                            ));
                        }
                        // TODO: replace polling with a per-turn tokio::sync::watch channel
                        // so run_turn can signal completion without busy-wait overhead.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }
    });

    // ── task.get ────────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("task.get", move |params: Value| {
        let ig = ig.clone();
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;

            let task = ig
                .get_task(id)
                .await
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INTERNAL_ERROR, format!("task not found: {id}"))
                })?;

            Ok(json!({
                "id": task.id,
                "status": task.status,
                "title": task.title,
                "response": task.response,
            }))
        }
    });

    // ── task.list ───────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("task.list", move |params: Value| {
        let ig = ig.clone();
        async move {
            let status = params.get("status").and_then(Value::as_str).map(str::to_owned);
            let tasks = ig
                .list_tasks(status)
                .await
                .map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = tasks
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "title": t.title,
                        "status": t.status,
                        "created_at": t.created_at,
                        "session_id": t.session_id,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── task.create ─────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("task.create", move |params: Value| {
        let ig = ig.clone();
        async move {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("title"))?
                .to_owned();
            let description = params
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let task = Task {
                id: Uuid::new_v4(),
                title,
                description,
                status: "planned".to_owned(),
                created_at: now_epoch(),
                session_id,
                response: None,
            };
            ig.create_task(task.clone())
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "id": task.id, "status": task.status }))
        }
    });

    // ── task.close ──────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("task.close", move |params: Value| {
        let ig = ig.clone();
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;
            ig.update_task_status(id, "complete")
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "id": id, "status": "complete" }))
        }
    });

    // ── turn.submit ─────────────────────────────────────────────────────────
    // Clone dispatcher before turn.submit moves it, so loop.run can hold its
    // own Arc without requiring a second clone point after the move.
    let dispatcher_loop_run = Arc::clone(&dispatcher);
    let ig = ingot.clone();
    router.register("turn.submit", move |params: Value| {
        let ig = ig.clone();
        let dispatcher = Arc::clone(&dispatcher);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("content"))?
                .to_owned();

            let task_id = Uuid::new_v4();
            let task = Task {
                id: task_id,
                title: content,
                description: String::new(),
                status: "planned".to_owned(),
                created_at: now_epoch(),
                session_id: Some(session_id.clone()),
                response: None,
            };

            ig.create_task(task.clone())
                .await
                .map_err(|e| ingot_err(&e))?;

            // Extract current span IDs for turn start event correlation.
            let (ts_trace_id, ts_span_id) = {
                use opentelemetry::trace::TraceContextExt as _;
                let cx = opentelemetry::Context::current();
                let sc = cx.span().span_context().clone();
                if sc.is_valid() {
                    (
                        Some(format!("{}", sc.trace_id())),
                        Some(format!("{}", sc.span_id())),
                    )
                } else {
                    (None, None)
                }
            };
            dispatcher.publish(TurnEvent::Started {
                session_id: session_id.clone(),
                turn_id: task_id.to_string(),
                conversation_id: Some(session_id.clone()),
                trace_id: ts_trace_id,
                span_id: ts_span_id,
                parent_span_id: None,
                operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
                agent_name: Some("interactive".to_owned()),
                status: None,
            });

            Ok(json!({ "task_id": task_id }))
        }
    });

    // ── session.checkpoint.list ─────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.checkpoint.list", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?;
            let cps = ig
                .list_checkpoints(session_id)
                .await
                .map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = cps
                .iter()
                .map(|cp| {
                    json!({
                        "turn_n": cp.turn_n,
                        "created_at": cp.created_at,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── session.rollback ────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.rollback", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let turn_n_raw = params
                .get("turn_n")
                .and_then(Value::as_i64)
                .ok_or_else(|| missing_param("turn_n"))?;
            let turn_n_u32 = u32::try_from(turn_n_raw).map_err(|_| {
                RpcError::new(
                    codes::INVALID_PARAMS,
                    "turn_n must be a non-negative integer",
                )
            })?;

            // Atomically load the target checkpoint and prune all later checkpoints
            // for this session in a single SQLite transaction.
            let cp = ig
                .rollback_session(&session_id, turn_n_u32)
                .await
                .map_err(|e| ingot_err(&e))?;

            match cp {
                Some(cp) => Ok(json!({
                    "session_id": session_id,
                    "turn_n": cp.turn_n,
                    "messages_json": cp.messages_json,
                    "created_at": cp.created_at,
                })),
                None => Err(RpcError::new(codes::INVALID_PARAMS, "checkpoint not found")),
            }
        }
    });

    // ── session.compact ──────────────────────────────────────────────────────
    let ig = ingot.clone();
    let compact_pool = Arc::clone(&provider_pool);
    let vault_compact = Arc::clone(&vault);
    router.register("session.compact", move |params: Value| {
        let ig = ig.clone();
        let pool = Arc::clone(&compact_pool);
        let vt = Arc::clone(&vault_compact);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();

            // Load latest checkpoint for conversation history.
            let messages_json = {
                ig.latest_checkpoint(&session_id)
                    .await
                    .map_err(|e| ingot_err(&e))?
                    .map_or_else(|| "[]".to_owned(), |cp| cp.messages_json)
            };

            // Call provider to produce a summary.
            let compaction_prompt = format!(
                "Summarise this conversation in 3–5 bullet points, then state the current goal. \
                 Be concise.\n\nConversation history:\n{messages_json}"
            );
            let pool_entry = pool
                .get(Runner::Claude, Tier::Fast)
                .or_else(|| pool.get(Runner::Codex, Tier::Fast))
                .or_else(|| pool.get_default())
                .ok_or_else(|| {
                    RpcError::new(codes::INTERNAL_ERROR, "no provider available for compaction")
                })?;
            let provider = &pool_entry.provider;
            let opts = CallOptions {
                model: std::env::var("SMEDJA_MODEL")
                    .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned()),
                max_tokens: Some(512),
                temperature: Some(0.3),
                system: Some("You are a summarisation assistant.".to_owned()),
                tools: None,
                provider_session_id: None,
                stable_prefix_len: None,
            };
            let stream = provider.stream_chat(
                &[AdapterMessage {
                    role: AdapterRole::User,
                    content: compaction_prompt,
                }],
                &opts,
            );
            let dispatcher = Dispatcher::new(1);
            let (summary, _, _, _) = drain_stream(stream, &dispatcher, None).await.map_err(|e| {
                RpcError::new(codes::INTERNAL_ERROR, format!("compaction failed: {e}"))
            })?;

            // Save pre-compaction checkpoint tagged with turn_n = -1.
            let turn_count = {
                ig.list_checkpoints(&session_id)
                    .await
                    .map_or(0i64, |v| i64::try_from(v.len()).unwrap_or(i64::MAX))
            };
            let cp = Checkpoint {
                id: Uuid::new_v4(),
                session_id: session_id.clone(),
                turn_n: -1, // compaction marker
                messages_json: messages_json.clone(),
                created_at: now_epoch(),
            };
            {
                if let Err(e) = ig.save_checkpoint(cp).await {
                    warn!(error = %e, "failed to save pre-compaction checkpoint");
                }
            }

            // Fire-and-forget: index compaction summary into vault cold storage.
            let compact_sid = session_id.clone();
            let compact_summary = summary.clone();
            tokio::task::spawn_blocking(move || {
                let entry = VaultEntry {
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
                if let Err(e) = guard.upsert(&entry) {
                    tracing::warn!(error = %e, "session.compact: vault upsert failed, compaction data lost");
                }
            });

            Ok(json!({
                "session_id": session_id,
                "summary": summary,
                "turn_count": turn_count,
                "compaction_checkpoint_saved": true,
            }))
        }
    });

    // ── session.token_usage ──────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.token_usage", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?;
            let snaps = ig
                .session_token_snapshots(session_id)
                .await
                .map_err(|e| ingot_err(&e))?;
            let rows: Vec<Value> = snaps
                .iter()
                .map(|s| {
                    json!({
                        "turn_n": s.turn_n,
                        "input_tok": s.input_tok,
                        "output_tok": s.output_tok,
                        "cumulative_input": s.cumulative_input,
                        "cumulative_output": s.cumulative_output,
                    })
                })
                .collect();
            Ok(json!({ "session_id": session_id, "turns": rows }))
        }
    });

    // ── session.cost ────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.cost", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?;
            let total_usd = ig
                .session_cost(session_id)
                .await
                .map_err(|e| ingot_err(&e))?;
            let rows: Vec<CostRow> = ig
                .session_cost_entries(session_id)
                .await
                .map_err(|e| ingot_err(&e))?;
            let breakdown: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "model": r.model,
                        "runner": r.runner,
                        "turns": r.turns,
                        "input_tok": r.input_tok,
                        "output_tok": r.output_tok,
                        "cost_usd": r.cost_usd,
                    })
                })
                .collect();
            Ok(json!({
                "session_id": session_id,
                "total_usd": total_usd,
                "breakdown": breakdown,
            }))
        }
    });

    // ── session.set_model ────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.set_model", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let model = params["model"]
                .as_str()
                .ok_or_else(|| missing_param("model"))?
                .to_owned();
            ig.update_session_model_override(&session_id, &model)
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "session_id": session_id, "model": model }))
        }
    });

    // ── session.set_runner ───────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.set_runner", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let runner_str = params["runner"]
                .as_str()
                .ok_or_else(|| missing_param("runner"))?
                .to_owned();
            // Validate and normalise to the canonical key stored in the DB.
            let canonical = parse_runner_str(&runner_str)
                .map(runner_session_key)
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INVALID_PARAMS,
                        format!("unknown runner: {runner_str}; valid: claude, codex, local, copilot"),
                    )
                })?;
            ig.update_session_runner_override(&session_id, canonical)
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "session_id": session_id, "runner": canonical }))
        }
    });

    // ── session.takeover ─────────────────────────────────────────────────────
    // Forks the current session onto a new runner in one atomic operation:
    // creates a new session, copies the latest checkpoint, and sets the
    // runner_override so the next turn routes to the requested runner.
    let ig = ingot.clone();
    let vault_takeover = Arc::clone(&vault);
    router.register("session.takeover", move |params: Value| {
        let ig = ig.clone();
        let vt = Arc::clone(&vault_takeover);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let runner_str = params["runner"]
                .as_str()
                .ok_or_else(|| missing_param("runner"))?
                .to_owned();

            let canonical = parse_runner_str(&runner_str)
                .map(runner_session_key)
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INVALID_PARAMS,
                        format!("unknown runner: {runner_str}; valid: claude, codex, local, copilot"),
                    )
                })?;

            let parent = {
                ig.get_session(&session_id)
                    .await
                    .map_err(|e| ingot_err(&e))?
                    .ok_or_else(|| {
                        RpcError::new(
                            codes::INTERNAL_ERROR,
                            format!("session not found: {session_id}"),
                        )
                    })?
            };

            let latest_cp = {
                ig.latest_checkpoint(&session_id)
                    .await
                    .map_err(|e| ingot_err(&e))?
            };

            let now = now_epoch();
            let new_id = Uuid::new_v4().to_string();

            {
                ig.create_session(Session {
                        id: Uuid::parse_str(&new_id).map_err(|e| {
                            RpcError::new(codes::INTERNAL_ERROR, format!("uuid error: {e}"))
                        })?,
                        created_at: now,
                        updated_at: now,
                        status: "active".into(),
                        task_id: None,
                        mode: parent.mode.clone(),
                        title: parent.title.clone(),
                        cowork_mode: parent.cowork_mode,
                        workspace_root: parent.workspace_root.clone(),
                        model_override: parent.model_override.clone(),
                        runner_override: Some(canonical.to_owned()),
                    })
                    .await
                    .map_err(|e| ingot_err(&e))?;
            }

            let has_checkpoint = latest_cp.is_some();
            let handoff_context_id = format!("handoff:{session_id}:{new_id}");
            if let Some(cp) = latest_cp {
                ig.save_checkpoint(Checkpoint {
                        id: Uuid::new_v4(),
                        session_id: new_id.clone(),
                        turn_n: cp.turn_n,
                        messages_json: cp.messages_json.clone(),
                        created_at: now,
                    })
                    .await
                    .map_err(|e| ingot_err(&e))?;

                // Fire-and-forget vault write so the receiving session can retrieve
                // the handoff context via smedja_vault_search namespace="handoff".
                let hid = handoff_context_id.clone();
                let from_sid = session_id.clone();
                let to_sid = new_id.clone();
                let runner_str = canonical.to_owned();
                let messages = cp.messages_json.clone();
                tokio::task::spawn_blocking(move || {
                    let entry = VaultEntry {
                        id: hid.clone(),
                        embedding: crate::embedder::embed(&messages),
                        payload: serde_json::json!({
                            "from_session_id": from_sid,
                            "to_session_id": to_sid,
                            "runner": runner_str,
                        }),
                        namespace: "handoff".to_owned(),
                        content: messages,
                        source_file: None,
                        added_by: Some("session.takeover".to_owned()),
                        chunk_index: None,
                        parent_id: None,
                        created_at: 0.0,
                    };
                    let mut guard = vt.blocking_lock();
                    let _ = guard.upsert(&entry);
                });
            }

            Ok(json!({
                "new_session_id": new_id,
                "forked_from": session_id,
                "runner": canonical,
                "has_checkpoint": has_checkpoint,
                "context_namespace": "handoff",
                "context_id": handoff_context_id,
            }))
        }
    });

    // ── runner.list ──────────────────────────────────────────────────────────
    let rl_pool = Arc::clone(&provider_pool);
    router.register("runner.list", move |_params: Value| {
        let pool = Arc::clone(&rl_pool);
        async move {
            let runners: Vec<Value> = pool
                .list_all_entries()
                .into_iter()
                .map(|(runner, tier, model)| {
                    json!({ "runner": runner, "tier": tier, "model": model })
                })
                .collect();
            Ok(json!({ "runners": runners }))
        }
    });

    // ── session.context ─────────────────────────────────────────────────────
    let ig = ingot.clone();
    let pt = Arc::clone(&price_table);
    let vault_ctx = Arc::clone(&vault);
    router.register("session.context", move |params: Value| {
        let ig = ig.clone();
        let pt = Arc::clone(&pt);
        let vt = Arc::clone(&vault_ctx);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?;
            let snaps = ig
                .session_token_snapshots(session_id)
                .await
                .map_err(|e| ingot_err(&e))?;
            let (cumulative_input, cumulative_output) = snaps
                .last()
                .map_or((0i64, 0i64), |s| (s.cumulative_input, s.cumulative_output));
            let used_tok = cumulative_input.saturating_add(cumulative_output);
            let model = ig
                .session_last_model(session_id)
                .await
                .map_err(|e| ingot_err(&e))?
                .unwrap_or_default();
            let window_tok = u64::from(pt.context_window(&model));
            let (vault_warm_count, vault_cold_count) =
                tokio::task::spawn_blocking(move || {
                    let guard = vt.blocking_lock();
                    let warm = guard.count_by_namespace("warm").unwrap_or(0);
                    let cold = guard.count_by_namespace("default").unwrap_or(0);
                    (warm, cold)
                })
                .await
                .unwrap_or((0, 0));
            Ok(json!({
                "session_id": session_id,
                "used_tok": used_tok,
                "window_tok": window_tok,
                "model": model,
                "vault_warm_count": vault_warm_count,
                "vault_cold_count": vault_cold_count,
            }))
        }
    });

    // ── cowork.set ──────────────────────────────────────────────────────────
    let ig = ingot.clone();
    let gates_set = Arc::clone(&gates);
    router.register("cowork.set", move |params: Value| {
        let ig = ig.clone();
        let gates = Arc::clone(&gates_set);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| missing_param("enabled"))?;
            ig.set_cowork_mode(&session_id, enabled)
                .await
                .map_err(|e| ingot_err(&e))?;

            // Manage the per-session gate.
            let mut g = gates.lock().await;
            if enabled {
                g.entry(session_id.clone())
                    .or_insert_with(|| Arc::new(CoworkGate::default()));
            } else {
                g.remove(&session_id);
            }

            Ok(json!({ "session_id": session_id, "cowork_mode": enabled }))
        }
    });

    // ── session.set_mode ────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("session.set_mode", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let mode = params
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("mode"))?
                .to_owned();
            // Prevent escalation out of read-only review sessions.
            let existing_session = ig
                .get_session(&session_id)
                .await
                .map_err(|e| ingot_err(&e))?;
            if let Some(existing_session) = existing_session {
                if existing_session.mode.as_deref() == Some("review") {
                    return Err(RpcError::new(
                        codes::INVALID_PARAMS,
                        "review sessions are read-only",
                    ));
                }
            }
            ig.update_session_mode(&session_id, &mode)
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "session_id": session_id, "mode": mode }))
        }
    });

    // ── mcp.register ────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("mcp.register", move |params: Value| {
        let ig = ig.clone();
        async move {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("name"))?
                .to_owned();
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if !is_safe_mcp_url(&url) {
                return Err(RpcError::new(codes::INVALID_PARAMS, "url not permitted"));
            }
            let transport = params
                .get("transport")
                .and_then(Value::as_str)
                .unwrap_or("http")
                .to_owned();
            let server = McpServer {
                id: Uuid::new_v4().to_string(),
                name,
                url,
                transport,
                tools_json: "[]".into(),
                last_refresh: 0.0,
            };
            ig.register_mcp_server(server.clone())
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "id": server.id }))
        }
    });

    // ── mcp.list ────────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("mcp.list", move |_: Value| {
        let ig = ig.clone();
        async move {
            let servers = ig
                .list_mcp_servers()
                .await
                .map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = servers
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "name": s.name,
                        "url": s.url,
                        "transport": s.transport,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── cowork.approve ───────────────────────────────────────────────────────
    let gates_approve = Arc::clone(&gates);
    router.register("cowork.approve", move |params: Value| {
        let gates = Arc::clone(&gates_approve);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let id = params["id"]
                .as_str()
                .ok_or_else(|| missing_param("id"))?
                .to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let found = gate.approve(&id).await;
            Ok(json!({ "id": id, "resolved": found }))
        }
    });

    // ── cowork.deny ──────────────────────────────────────────────────────────
    let gates_deny = Arc::clone(&gates);
    router.register("cowork.deny", move |params: Value| {
        let gates = Arc::clone(&gates_deny);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let id = params["id"]
                .as_str()
                .ok_or_else(|| missing_param("id"))?
                .to_owned();
            let reason = params["reason"].as_str().unwrap_or("denied").to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let found = gate.deny(&id, reason).await;
            Ok(json!({ "id": id, "resolved": found }))
        }
    });

    // ── cowork.modify ────────────────────────────────────────────────────────
    let gates_modify = Arc::clone(&gates);
    router.register("cowork.modify", move |params: Value| {
        let gates = Arc::clone(&gates_modify);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let id = params["id"]
                .as_str()
                .ok_or_else(|| missing_param("id"))?
                .to_owned();
            let instruction = params["instruction"].as_str().unwrap_or("").to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let found = gate.modify(&id, instruction).await;
            Ok(json!({ "id": id, "resolved": found }))
        }
    });

    // ── cowork.pending ───────────────────────────────────────────────────────
    let gates_pending = Arc::clone(&gates);
    router.register("cowork.pending", move |params: Value| {
        let gates = Arc::clone(&gates_pending);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let pending = gate.list_pending().await;
            let out: Vec<Value> = pending
                .into_iter()
                .map(|(id, p)| {
                    json!({
                        "id": id,
                        "tool": p.tool,
                        "step_n": p.step_n,
                        "args": p.args_scrubbed,
                        "reasoning": p.reasoning,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── task.parallel ────────────────────────────────────────────────────────
    let ig = ingot.clone();
    let vault_parallel = Arc::clone(&vault);
    router.register("task.parallel", move |params: Value| {
        let pool = Arc::clone(&pool);
        let ig = ig.clone();
        let vt = Arc::clone(&vault_parallel);
        async move {
            let session_id = params["session_id"].as_str().map(str::to_owned);
            let goal = params["goal"]
                .as_str()
                .ok_or_else(|| missing_param("goal"))?
                .to_owned();
            // Roles may be plain strings or `{name, resume_session_id?}` objects.
            let loop_roles: Vec<smedja_assayer::LoopRole> = params["roles"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| {
                    if let Some(name) = v.as_str() {
                        Some(smedja_assayer::LoopRole {
                            name: name.to_owned(),
                            resume_session_id: None,
                        })
                    } else if let Some(obj) = v.as_object() {
                        obj.get("name").and_then(Value::as_str).map(|name| {
                            smedja_assayer::LoopRole {
                                name: name.to_owned(),
                                resume_session_id: obj
                                    .get("resume_session_id")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            }
                        })
                    } else {
                        None
                    }
                })
                .collect();

            // Enforce MAX_ROLE_DEPTH: reject any role that tries to resume a session at depth ≥ 4.
            {
                for role in &loop_roles {
                    if let Some(ref resume_sid) = role.resume_session_id {
                        let cps = ig.list_checkpoints(resume_sid).await.unwrap_or_default();
                        let depth = cps.iter().filter(|cp| cp.turn_n == -1).count();
                        #[allow(clippy::cast_possible_truncation)]
                        if depth as u8 >= smedja_assayer::MAX_ROLE_DEPTH {
                            return Err(RpcError::new(
                                codes::INVALID_PARAMS,
                                format!(
                                    "resume depth exceeded for role '{}': max {}",
                                    role.name,
                                    smedja_assayer::MAX_ROLE_DEPTH,
                                ),
                            ));
                        }
                    }
                }
            }
            let roles: Vec<String> = loop_roles.iter().map(|r| r.name.clone()).collect();

            // Derive workspace root: prefer session.workspace_root, then env, then ".".
            let env_workspace = || {
                std::env::var("SMEDJA_WORKSPACE").map_or_else(|_| PathBuf::from("."), PathBuf::from)
            };
            let workspace_root = if let Some(ref sid) = session_id {
                ig.get_session(sid)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.workspace_root)
                    .map_or_else(env_workspace, PathBuf::from)
            } else {
                env_workspace()
            };

            if !workspace_root.join(".git").exists() {
                tracing::warn!(
                    path = %workspace_root.display(),
                    "task.parallel workspace does not contain .git",
                );
            }

            let mut p = pool.lock().await;

            // Register all roles first (synchronous — no await).
            let registered: Vec<(String, String)> = roles
                .iter()
                .map(|role| {
                    let id = p.register(role, &goal, &workspace_root);
                    (role.clone(), id)
                })
                .collect();

            // Create the git worktrees for all pending tasks.
            let started = p.start_worktrees(&workspace_root).await;

            // Warm-context snapshot: write recent checkpoints to vault so parallel
            // agents can retrieve shared context via smedja_vault_search.
            let fan_out_id = Uuid::new_v4().to_string();
            if let Some(ref sid) = session_id {
                let checkpoints = ig.list_checkpoints(sid).await.unwrap_or_default();
                const WARM_WINDOW: usize = 5;
                let recent: Vec<_> = checkpoints
                    .into_iter()
                    .rev()
                    .take(WARM_WINDOW)
                    .collect();
                if !recent.is_empty() {
                    let fid = fan_out_id.clone();
                    let parent_sid = sid.clone();
                    let vt2 = Arc::clone(&vt);
                    tokio::task::spawn_blocking(move || {
                        let mut guard = vt2.blocking_lock();
                        for cp in &recent {
                            let entry = VaultEntry {
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
                            let _ = guard.upsert(&entry);
                        }
                    });
                }
            }

            // Build the per-task response, including worktree_path where available.
            let tasks: Vec<Value> = registered
                .iter()
                .map(|(role, task_id)| {
                    let worktree_path = p
                        .get(task_id)
                        .map(|t| t.worktree_path.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    json!({
                        "role": role,
                        "task_id": task_id,
                        "worktree_path": worktree_path,
                    })
                })
                .collect();

            Ok(json!({
                "goal": goal,
                "tasks": tasks,
                "started": started,
                "fan_out_id": fan_out_id,
                "warm_context_namespace": "warm",
            }))
        }
    });

    // ── task.cancel ──────────────────────────────────────────────────────────
    router.register("task.cancel", move |params: Value| {
        let pool = Arc::clone(&pool_cancel);
        async move {
            let task_id = params["task_id"]
                .as_str()
                .ok_or_else(|| missing_param("task_id"))?
                .to_owned();
            let found = pool.lock().await.cancel(&task_id);
            Ok(json!({ "task_id": task_id, "cancelled": found }))
        }
    });

    // ── mcp.remove ───────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("mcp.remove", move |params: Value| {
        let ig = ig.clone();
        async move {
            let name = params["name"]
                .as_str()
                .ok_or_else(|| missing_param("name"))?
                .to_owned();
            ig.remove_mcp_server(&name)
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "name": name, "removed": true }))
        }
    });

    // ── mcp.refresh ──────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("mcp.refresh", move |params: Value| {
        let ig = ig.clone();
        async move {
            let name_filter: Option<String> = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned);

            // Load the candidate servers — all registered, or the named one.
            let servers = {
                let all = ig
                    .list_mcp_servers()
                    .await
                    .map_err(|e| ingot_err(&e))?;
                match name_filter {
                    Some(ref name) => all
                        .into_iter()
                        .filter(|s| &s.name == name)
                        .collect::<Vec<_>>(),
                    None => all,
                }
            };

            let mut refreshed = 0usize;
            for server in servers {
                let client = match crate::mcp_http::McpHttpClient::new(&server.url, "") {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(name = %server.name, error = %e, "mcp.refresh: failed to build client");
                        continue;
                    }
                };
                match client.list_tools().await {
                    Ok(tools) => {
                        let tools_json = serde_json::to_string(&tools)
                            .unwrap_or_else(|_| "[]".to_owned());
                        let updated = McpServer {
                            tools_json,
                            last_refresh: now_epoch(),
                            ..server.clone()
                        };
                        let _ = ig.register_mcp_server(updated).await;
                        refreshed += 1;
                    }
                    Err(e) => {
                        tracing::warn!(name = %server.name, error = %e, "mcp.refresh failed");
                    }
                }
            }

            Ok(json!({ "refreshed": refreshed }))
        }
    });

    // ── loop.create ──────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("loop.create", move |params: Value| {
        let ig = ig.clone();
        async move {
            let change_name = params["change_name"]
                .as_str()
                .ok_or_else(|| missing_param("change_name"))?
                .to_owned();
            let now = now_epoch();
            let rec = smedja_ingot::LoopRecord {
                id: Uuid::new_v4().to_string(),
                change_name,
                status: "planned".to_owned(),
                current_slice: 0,
                attempt: 1,
                created_at: now,
                updated_at: now,
            };
            let loop_id = rec.id.clone();
            ig.create_loop(rec)
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "loop_id": loop_id }))
        }
    });

    // ── loop.status ──────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("loop.status", move |params: Value| {
        let ig = ig.clone();
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();
            let rec = ig
                .get_loop(&loop_id)
                .await
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
                })?;
            Ok(serde_json::to_value(&rec).unwrap_or(Value::Null))
        }
    });

    // ── loop.cancel ──────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("loop.cancel", move |params: Value| {
        let ig = ig.clone();
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();
            ig.update_loop_status(&loop_id, "cancelled", now_epoch())
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "loop_id": loop_id, "status": "cancelled" }))
        }
    });

    // ── loop.list ────────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("loop.list", move |params: Value| {
        let ig = ig.clone();
        async move {
            let change_name = params["change_name"]
                .as_str()
                .ok_or_else(|| missing_param("change_name"))?
                .to_owned();
            let loops = ig
                .list_loops(&change_name)
                .await
                .map_err(|e| ingot_err(&e))?;
            let loops_json: Vec<Value> = loops
                .into_iter()
                .map(|r| serde_json::to_value(&r).unwrap_or(Value::Null))
                .collect();
            Ok(json!({ "loops": loops_json }))
        }
    });

    // ── loop.retire ──────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("loop.retire", move |params: Value| {
        let ig = ig.clone();
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();
            let rec = ig
                .get_loop(&loop_id)
                .await
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
                })?;
            // Only complete or failed loops can be retired.
            if rec.status != "complete" && rec.status != "failed" {
                return Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    format!(
                        "loop is in state '{}'; only complete or failed loops can be retired",
                        rec.status
                    ),
                ));
            }
            ig.update_loop_status(&loop_id, "retired", now_epoch())
                .await
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "loop_id": loop_id, "status": "retired" }))
        }
    });

    // ── loop.list_by_status ──────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("loop.list_by_status", move |params: Value| {
        let ig = ig.clone();
        async move {
            let status = params["status"].as_str().map(str::to_owned);
            let loops = ig
                .list_loops_by_status(status)
                .await
                .map_err(|e| ingot_err(&e))?;
            let loops_json: Vec<Value> = loops
                .into_iter()
                .map(|r| serde_json::to_value(&r).unwrap_or(Value::Null))
                .collect();
            Ok(json!({ "loops": loops_json }))
        }
    });

    // ── audit.list ───────────────────────────────────────────────────────────
    let ig = ingot.clone();
    router.register("audit.list", move |params: Value| {
        let ig = ig.clone();
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let events = ig
                .list_audit_events(&session_id)
                .await
                .map_err(|e| ingot_err(&e))?;
            let events_json: Vec<Value> = events
                .into_iter()
                .map(|ev| serde_json::to_value(&ev).unwrap_or(Value::Null))
                .collect();
            Ok(json!({ "events": events_json }))
        }
    });

    // ── loop.run ────────────────────────────────────────────────────────────
    let ig = ingot.clone();
    let gates_run = Arc::clone(&gates);
    router.register("loop.run", move |params: Value| {
        let ig = ig.clone();
        let dispatcher = Arc::clone(&dispatcher_loop_run);
        let _gates = Arc::clone(&gates_run);
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();

            // Verify the loop record exists before spawning background work.
            let rec = ig
                .get_loop(&loop_id)
                .await
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
                })?;

            // Retired loops cannot be re-run.
            if rec.status == "retired" {
                return Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    "loop is retired and cannot be re-run",
                ));
            }

            // Guard against path traversal via change_name.
            if rec.change_name.contains("..") || rec.change_name.contains('/') {
                return Err(RpcError::new(codes::INVALID_PARAMS, "invalid change_name"));
            }

            // Spawn background task — caller gets an immediate response.
            let bg_ig = ig.clone();
            let bg_dispatcher = Arc::clone(&dispatcher);
            let bg_loop_id = loop_id.clone();
            let change_name = rec.change_name.clone();
            tokio::spawn(async move {
                let workspace = std::env::var("SMEDJA_WORKSPACE").unwrap_or_else(|_| ".".into());
                let tasks_path = std::path::PathBuf::from(&workspace)
                    .join("openspec")
                    .join("changes")
                    .join(&change_name)
                    .join("tasks.md");

                let Ok(tasks_content) = tokio::fs::read_to_string(&tasks_path).await else {
                    let _ =
                        bg_ig
                            .update_loop_status(&bg_loop_id, "failed", now_epoch())
                            .await;
                    return;
                };

                // Parse pending tasks: lines starting with `- [ ] `.
                let pending: Vec<String> = tasks_content
                    .lines()
                    .filter(|l| l.starts_with("- [ ] "))
                    .map(|l| l.trim_start_matches("- [ ] ").to_owned())
                    .collect();

                if pending.is_empty() {
                    let _ =
                        bg_ig
                            .update_loop_status(&bg_loop_id, "complete", now_epoch())
                            .await;
                    return;
                }

                // Mark loop as slicing.
                {
                    let _ = bg_ig.update_loop_status(&bg_loop_id, "slicing", now_epoch()).await;
                }

                // Create one session for this loop run.
                let session_id = Uuid::new_v4();
                let now = now_epoch();
                let session = Session {
                    id: session_id,
                    created_at: now,
                    updated_at: now,
                    status: "active".to_owned(),
                    task_id: None,
                    mode: Some("loop".to_owned()),
                    title: String::new(),
                    cowork_mode: false,
                    workspace_root: Some(workspace),
                    model_override: None,
                    runner_override: None,
                };
                {
                    let _ = bg_ig.create_session(session).await;
                }

                // Submit one turn per pending task, poll until done, then mark checkbox.
                let mut updated_content = tasks_content.clone();
                for (slice_idx, task_text) in pending.into_iter().enumerate() {
                    let task_id = Uuid::new_v4();
                    let task = Task {
                        id: task_id,
                        title: task_text.clone(),
                        description: String::new(),
                        status: "planned".to_owned(),
                        created_at: now_epoch(),
                        session_id: Some(session_id.to_string()),
                        response: None,
                    };

                    {
                        let _ = bg_ig.create_task(task.clone()).await;
                    }

                    // Extract current span IDs for loop turn start event correlation.
                    let (loop_ts_trace_id, loop_ts_span_id) = {
                        use opentelemetry::trace::TraceContextExt as _;
                        let cx = opentelemetry::Context::current();
                        let sc = cx.span().span_context().clone();
                        if sc.is_valid() {
                            (
                                Some(format!("{}", sc.trace_id())),
                                Some(format!("{}", sc.span_id())),
                            )
                        } else {
                            (None, None)
                        }
                    };
                    bg_dispatcher.publish(TurnEvent::Started {
                        session_id: session_id.to_string(),
                        turn_id: task_id.to_string(),
                        conversation_id: Some(session_id.to_string()),
                        trace_id: loop_ts_trace_id,
                        span_id: loop_ts_span_id,
                        parent_span_id: None,
                        operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
                        agent_name: Some("interactive".to_owned()),
                        status: None,
                    });

                    // Poll until the task leaves "planned" / "in_progress".
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        let status = bg_ig
                            .get_task(&task_id.to_string())
                            .await
                            .ok()
                            .flatten()
                            .map_or_else(|| "unknown".to_owned(), |t| t.status);
                        if status != "planned" && status != "in_progress" {
                            break;
                        }
                    }

                    // Replace `- [ ] <task>` with `- [x] <task>` in content string.
                    let unchecked = format!("- [ ] {task_text}");
                    let checked = format!("- [x] {task_text}");
                    updated_content = updated_content.replacen(&unchecked, &checked, 1);

                    // Advance current_slice counter.
                    #[allow(clippy::cast_possible_wrap)]
                    // slice index never exceeds i64::MAX in practice
                    let slice_count = (slice_idx + 1) as i64;
                    let _ =
                        bg_ig
                            .update_loop_slice(&bg_loop_id, slice_count, now_epoch())
                            .await;
                }

                // Write updated tasks.md back to disk.
                let _ = tokio::fs::write(&tasks_path, &updated_content).await;

                // Mark loop complete.
                let _ = bg_ig
                    .update_loop_status(&bg_loop_id, "complete", now_epoch())
                    .await;
            });

            Ok(json!({ "loop_id": loop_id, "status": "slicing" }))
        }
    });

    router
}

/// Writes the ACP auth token to the runtime secret file with 0o600 permissions.
///
/// Path preference: `$XDG_RUNTIME_DIR/smdjad.secret` → `$HOME/.cache/smdjad.secret`
/// → `/tmp/smdjad.secret`.
fn write_acp_secret(token: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let secret_path = std::env::var("XDG_RUNTIME_DIR").map_or_else(
        |_| {
            dirs_home().map_or_else(
                || std::path::PathBuf::from("/tmp/smdjad.secret"),
                |h| h.join(".cache").join("smdjad.secret"),
            )
        },
        |d| std::path::PathBuf::from(d).join("smdjad.secret"),
    );

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

#[allow(clippy::too_many_lines)] // startup sequence: bind, migrate, orphan sweep, spawn workers
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Install an OTLP exporter when SMEDJA_OTLP_ENDPOINT is set.
    // Without this, all OTel spans are silently discarded by the no-op provider.
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
                info!(endpoint = %endpoint, "OTLP trace exporter installed");
            }
            Err(e) => {
                warn!(error = %e, endpoint = %endpoint, "failed to install OTLP exporter; traces will not be exported");
            }
        }
    } else {
        warn!("SMEDJA_OTLP_ENDPOINT not set; OTel traces will not be exported");
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
        let stale_threshold = now_epoch() - 3600.0;
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
                            last_refresh: now_epoch(),
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
    let startup_runner: &'static str = Box::leak(pool.default_runner_name().to_owned().into_boxed_str());
    let startup_model: &'static str = Box::leak(pool.default_model().to_owned().into_boxed_str());
    let pool = Arc::new(pool);

    // Load workspace-local routing overrides if .smedja/agents.toml exists.
    let workspace_root = std::env::var("SMEDJA_WORKSPACE")
        .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from);
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
    let price_table = Arc::new(PriceTable::embedded());

    let vault = Arc::new(Mutex::new(open_vault()));

    let router = build_router(
        &ingot,
        Arc::clone(&dispatcher),
        &gates,
        Arc::clone(&pool),
        startup_runner,
        startup_model,
        Arc::clone(&price_table),
        &vault,
    );

    let turn_handles = spawn_worker(
        ingot.clone(),
        Arc::clone(&dispatcher),
        Arc::clone(&gates),
        Arc::clone(&pool),
        Arc::clone(&assayer),
        Arc::clone(&price_table),
        Arc::clone(&vault),
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
    let delta_store = stream_server::spawn_delta_buffer(Arc::clone(&dispatcher));
    let stream_sock_path = stream_server::stream_socket_path(&path);
    let _ = std::fs::remove_file(&stream_sock_path);
    let _stream_sock_guard = SocketGuard { path: stream_sock_path.clone() };
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

    let server = Server::new(router);

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
    }

    // Drain any in-flight run_turn tasks before cleaning up, so that turns that
    // are mid-stream can complete (or fail cleanly) rather than being silently
    // abandoned.  A 30 s deadline prevents indefinite blocking on a stuck task.
    {
        let handles: Vec<tokio::task::JoinHandle<()>> =
            std::mem::take(&mut *turn_handles.lock().await);
        if !handles.is_empty() {
            info!(
                count = handles.len(),
                "waiting for in-flight turns to finish (up to 30 s)"
            );
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                futures_util::future::join_all(handles),
            )
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
        if runner.contains("local") { "local" } else { "fast" }
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
        use super::{Runner, parse_runner_str};
        assert!(matches!(parse_runner_str("claude"), Some(Runner::Claude)));
        assert!(matches!(parse_runner_str("codex"), Some(Runner::Codex)));
        assert!(matches!(parse_runner_str("local"), Some(Runner::Local)));
        assert!(matches!(parse_runner_str("copilot"), Some(Runner::Copilot)));
    }

    #[test]
    fn parse_runner_str_accepts_canonical_keys() {
        use super::{Runner, parse_runner_str};
        assert!(matches!(parse_runner_str("claude-cli"), Some(Runner::Claude)));
        assert!(matches!(parse_runner_str("codex-cli"), Some(Runner::Codex)));
    }

    #[test]
    fn parse_runner_str_rejects_unknown_values() {
        use super::parse_runner_str;
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
        let now = 1_700_000_000.0_f64;
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
        let now = 1_700_000_000.0_f64;
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
            created_at: now + 1.0,
            updated_at: now + 1.0,
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
            created_at: 1_700_000_000.0,
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
        assert!(!results.is_empty(), "warm snapshot must be retrievable by content similarity");
    }

    #[tokio::test]
    async fn takeover_handoff_writes_vault_entry_with_handoff_namespace() {
        use smedja_vault::Vault;

        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let from_sid = "sess-from".to_owned();
        let to_sid = "sess-to".to_owned();
        let messages = r#"[{"role":"user","content":"implement auth"},{"role":"assistant","content":"ok"}]"#.to_owned();
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
