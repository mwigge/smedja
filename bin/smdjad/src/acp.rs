//! ACP HTTP server — Agent Coordination Protocol over HTTP.
//!
//! Activated by `SMEDJA_ACP_PORT` environment variable (default: disabled).
//! Routes proxy into smdjad's ingot and dispatcher directly.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use smedja_bellows::{Dispatcher, ToolCallContent, TurnEvent, TurnHandle};
use smedja_ingot::{IngotHandle, McpServer, Session, Task};
use smedja_types::Timestamp;
use smedja_vault::Vault;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::cowork::{append_allow_rule, ApprovalPrompt, CoworkGate, Decision};

/// Shared state for ACP route handlers.
#[derive(Clone)]
pub struct AcpState {
    pub ingot: IngotHandle,
    pub dispatcher: Arc<Dispatcher>,
    pub auth_token: String,
    /// Workspace root used by MCP server-mode tool dispatch.
    pub workspace: std::path::PathBuf,
    /// Vector store shared with MCP server-mode tool dispatch.
    pub vault: Arc<Mutex<Vault>>,
    /// Embedding backend shared with MCP server-mode tool dispatch.
    pub embedder: Arc<dyn crate::embedder_port::Embedder>,
    /// The ONE per-session cowork gate map, shared with the native tool loop and
    /// the `smj tool-gate` claude hook. Industry-ACP `session/request_permission`
    /// requests route through this same map, so a single approval widget serves
    /// every backend.
    pub gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
}

/// Builds the ACP router with auth middleware applied to every route.
pub fn build_acp_router(state: AcpState) -> Router {
    Router::new()
        .route("/acp/v1/session/new", post(create_session))
        .route("/acp/v1/session/{id}/prompt", post(submit_prompt))
        .route("/acp/v1/session/{id}/model", post(set_model))
        .route("/acp/v1/session/{id}/mode", post(set_mode))
        // Industry-ACP (Agent Client Protocol, Zed/JetBrains) bridge routes.
        // `request_permission` funnels a backend's permission ask into the ONE
        // cowork gate; `load` replays a session's history as `session/update`
        // notifications so an editor client gets true resume.
        .route(
            "/acp/v1/session/{id}/request_permission",
            post(request_permission),
        )
        .route("/acp/v1/session/{id}/load", post(session_load))
        .route("/acp/v1/session/{id}", delete(close_session))
        // MCP server mode — JSON-RPC 2.0 over the same authenticated listener.
        .route("/mcp", post(mcp_server_endpoint))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        // /health is added after the auth layer so it is unauthenticated: a
        // supervisor or load balancer probes readiness without a token. It is
        // reachable only once the daemon is serving, so it returns 200.
        .route("/health", get(health))
        .with_state(state)
}

/// Liveness/readiness probe: returns `200 OK` whenever the daemon is serving.
async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Rejects requests that do not carry a valid `Authorization: Bearer <token>` header.
async fn require_auth(
    State(state): State<AcpState>,
    request: axum::extract::Request,
    next: Next,
) -> impl IntoResponse {
    let auth = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if auth.is_some_and(|t| smedja_auth::tokens_match(t, &state.auth_token)) {
        next.run(request).await.into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
struct PromptRequest {
    content: String,
}

/// MCP server-mode endpoint: parses a JSON-RPC 2.0 request and dispatches it to
/// the read-safe tool handler. Reached only after `require_auth` succeeds, so
/// unauthenticated requests are rejected before any tool dispatch.
async fn mcp_server_endpoint(
    State(s): State<AcpState>,
    Json(request): Json<smedja_rpc::Request>,
) -> impl IntoResponse {
    let response =
        crate::mcp_server::handle_request(&request, &s.workspace, &s.ingot, &s.vault, &s.embedder)
            .await;
    Json(response)
}

/// One MCP server entry supplied to `session/new` for per-session attachment.
///
/// Accepts both the network shape (`url`) and Zed's stdio shape (`command`); when
/// no `transport` is given it is inferred from the URL scheme. Optional `tools`
/// (pre-known descriptors) make the server's tools immediately dispatchable
/// without a live `tools/list` refresh.
#[derive(Debug, Default, Deserialize)]
struct AcpMcpServer {
    #[serde(default)]
    name: String,
    #[serde(default, alias = "command")]
    url: String,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    tools: Option<serde_json::Value>,
}

/// Body of `POST /acp/v1/session/new` — an optional list of per-session MCP
/// servers to attach. An empty body creates a session with no extra servers.
#[derive(Debug, Default, Deserialize)]
struct CreateSessionBody {
    #[serde(default, alias = "mcpServers")]
    mcp_servers: Vec<AcpMcpServer>,
}

/// Registers the per-session MCP servers from `session/new` into the ingot
/// registry, keyed by a session-scoped id (`acp-session:{session}:{name}`) so the
/// entries are strictly additive to — and never clobber — the global MCP config.
/// Returns the names successfully attached.
///
/// A server's `tools` (when supplied) are stored verbatim so its tools are
/// immediately reachable via `find_mcp_server_for_tool`; otherwise the daemon
/// picks them up on the next `mcp.refresh`. A network server whose URL is not a
/// permitted outbound target is skipped (logged), matching `mcp.register`.
async fn attach_session_mcp_servers(
    ingot: &IngotHandle,
    session_id: &str,
    servers: &[AcpMcpServer],
) -> Vec<String> {
    let mut attached = Vec::new();
    for s in servers {
        if s.name.is_empty() {
            continue;
        }
        let transport = s.transport.clone().unwrap_or_else(|| {
            if s.url.starts_with("http") {
                "http".to_owned()
            } else {
                "stdio".to_owned()
            }
        });
        // Network transports must clear the outbound URL gate; stdio commands do not.
        if transport != "stdio" && !crate::is_safe_mcp_url(&s.url) {
            tracing::warn!(server = %s.name, "session/new: skipping MCP server with disallowed url");
            continue;
        }
        let tools_json = s
            .tools
            .as_ref()
            .map_or_else(|| "[]".to_owned(), ToString::to_string);
        let server = McpServer {
            id: format!("acp-session:{session_id}:{}", s.name),
            name: s.name.clone(),
            url: s.url.clone(),
            transport,
            tools_json,
            last_refresh: 0.0,
        };
        if let Err(e) = ingot.register_mcp_server(server).await {
            tracing::warn!(server = %s.name, error = %e, "session/new: failed to attach MCP server");
            continue;
        }
        attached.push(s.name.clone());
    }
    attached
}

async fn create_session(State(s): State<AcpState>, body: axum::body::Bytes) -> impl IntoResponse {
    // The body is optional: an empty POST creates a bare session, while a JSON
    // body may carry `mcpServers` to attach for this session only.
    let cfg: CreateSessionBody = if body.is_empty() {
        CreateSessionBody::default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };

    let id = Uuid::new_v4();
    let now = Timestamp::now();
    let session = Session {
        id,
        mode: Some("acp".into()),
        title: String::new(),
        status: "active".into(),
        task_id: None,
        cowork_mode: false,
        created_at: now,
        updated_at: now,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    };
    match s.ingot.create_session(session).await {
        Ok(()) => {
            let attached =
                attach_session_mcp_servers(&s.ingot, &id.to_string(), &cfg.mcp_servers).await;
            Json(json!({ "session_id": id, "mcpServers": attached })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Returns the turn identifier an event is correlated with, if any.
fn turn_id_of(event: &TurnEvent) -> Option<&str> {
    match event {
        TurnEvent::Started { turn_id, .. }
        | TurnEvent::Completed { turn_id, .. }
        | TurnEvent::Failed { turn_id, .. }
        | TurnEvent::HistoryReplaced { turn_id, .. } => Some(turn_id.as_str()),
        TurnEvent::ToolCalled { turn_id, .. }
        | TurnEvent::AssistantDelta { turn_id, .. }
        | TurnEvent::ThinkingDelta { turn_id, .. }
        | TurnEvent::QualitySnapshot { turn_id, .. }
        | TurnEvent::CoworkRequest { turn_id, .. }
        | TurnEvent::TokenUsage { turn_id, .. }
        | TurnEvent::ToolCallUpdate { turn_id, .. }
        | TurnEvent::ToolCallChunk { turn_id, .. } => turn_id.as_deref(),
    }
}

/// Reports whether `event` is a terminal event (`Completed` or `Failed`) for
/// `turn_id`.
fn is_terminal_for(event: &TurnEvent, turn_id: &str) -> bool {
    matches!(
        event,
        TurnEvent::Completed { turn_id: t, .. } | TurnEvent::Failed { turn_id: t, .. } if t == turn_id
    )
}

/// Maps a tool-lifecycle [`TurnEvent`] to an industry-ACP `session/update`
/// notification, or `None` for any non-tool event.
///
/// - [`TurnEvent::ToolCalled`] → `sessionUpdate: "tool_call"` (status `pending`),
///   carrying the tool name as the title and its input summary as `rawInput`.
/// - [`TurnEvent::ToolCallUpdate`] → `sessionUpdate: "tool_call_update"` with the
///   mapped status (`pending | in_progress | completed | failed`) and, for edit
///   tools, a `content: [{type:"diff", path, oldText, newText}]` item so a Zed
///   client can render the proposed change inline for modify-then-approve.
///
/// Pure, so the shaping (including the diff content) is unit-testable.
fn tool_event_to_session_update(session_id: &str, event: &TurnEvent) -> Option<serde_json::Value> {
    match event {
        TurnEvent::ToolCalled {
            tool_name,
            input_summary,
            tool_call_id,
            ..
        } => Some(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": tool_call_id.clone().unwrap_or_default(),
                    "title": tool_name,
                    "status": "pending",
                    "rawInput": input_summary,
                }
            }
        })),
        TurnEvent::ToolCallUpdate {
            tool_call_id,
            status,
            content,
            ..
        } => {
            let content_json: Vec<serde_json::Value> = content
                .iter()
                .map(|c| match c {
                    ToolCallContent::Diff {
                        path,
                        old_text,
                        new_text,
                    } => json!({
                        "type": "diff",
                        "path": path,
                        "oldText": old_text,
                        "newText": new_text,
                    }),
                })
                .collect();
            let mut update = json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": status.as_acp_str(),
            });
            if !content_json.is_empty() {
                update["content"] = serde_json::Value::Array(content_json);
            }
            Some(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": session_id, "update": update }
            }))
        }
        _ => None,
    }
}

/// Renders one forwarded [`TurnEvent`] as the SSE data payload for `session_id`.
///
/// Tool-lifecycle events are reshaped into industry-ACP `session/update`
/// notifications (`tool_call` / `tool_call_update`); every other event is carried
/// as its raw serialised form, matching the historical wire shape.
fn event_to_sse_data(session_id: &str, event: &TurnEvent) -> String {
    tool_event_to_session_update(session_id, event)
        .map(|v| v.to_string())
        .unwrap_or_else(|| serde_json::to_string(event).unwrap_or_default())
}

/// Builds an SSE response that forwards every [`TurnEvent`] for `turn_id` from
/// `receiver`, terminating after the turn's terminal event. A keep-alive
/// heartbeat prevents idle-timeout disconnects. Tool-call events are reshaped
/// into ACP `tool_call` / `tool_call_update` notifications for `session_id`.
fn build_turn_sse(
    receiver: tokio::sync::broadcast::Receiver<TurnEvent>,
    session_id: String,
    turn_id: String,
) -> axum::response::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};

    let stream = futures_util::stream::unfold(
        (receiver, session_id, turn_id, false),
        |(mut rx, session_id, turn_id, finished)| async move {
            if finished {
                return None;
            }
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // Forward only events for this turn; an event with no
                        // turn correlation is skipped.
                        match turn_id_of(&event) {
                            Some(tid) if tid == turn_id => {}
                            _ => continue,
                        }
                        let terminal = is_terminal_for(&event, &turn_id);
                        let data = event_to_sse_data(&session_id, &event);
                        let sse_event = Event::default().data(data);
                        return Some((Ok(sse_event), (rx, session_id, turn_id, terminal)));
                    }
                    // A lagged subscriber skips dropped events and re-loops.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    // The dispatcher closed — end the stream.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn submit_prompt(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<PromptRequest>,
) -> impl IntoResponse {
    let turn_id = Uuid::new_v4();
    let now = Timestamp::now();
    let session_id = id.clone();
    let task = Task {
        id: turn_id,
        session_id: Some(id.clone()),
        title: body.content,
        description: String::new(),
        status: "queued".into(),
        response: None,
        created_at: now,
    };
    match s.ingot.create_task(task).await {
        Ok(()) => {
            // Subscribe BEFORE starting the TurnHandle so the Started event this
            // handle publishes is observed by the SSE stream.
            let receiver = s.dispatcher.subscribe();
            // Emit TurnEvent::Started through TurnHandle so the event is routed
            // consistently with the main run_turn path. Drop the handle
            // immediately — ACP does not drive the turn itself; spawn_worker
            // picks up the Started event and calls run_turn. The turn remains
            // recorded as a Task, so polling clients can still read the result.
            let _handle = TurnHandle::start(
                session_id.clone(),
                turn_id.to_string(),
                Arc::clone(&s.dispatcher),
            );
            build_turn_sse(receiver, session_id, turn_id.to_string()).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn set_model(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(model) = body["model"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "model field required" })),
        )
            .into_response();
    };
    match s.ingot.update_session_model_override(&id, model).await {
        Ok(()) => Json(json!({ "session_id": id, "model": model })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn set_mode(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(mode) = body["mode"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "mode field required" })),
        )
            .into_response();
    };
    match s.ingot.update_session_mode(&id, mode).await {
        Ok(()) => Json(json!({ "session_id": id, "mode": mode })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn close_session(Path(id): Path<String>, State(s): State<AcpState>) -> impl IntoResponse {
    match s.ingot.delete_session(&id).await {
        Ok(_) => Json(json!({ "session_id": id, "deleted": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ── Industry-ACP `session/request_permission` bridge ─────────────────────────
//
// smedja is the permission authority here: an ACP-capable backend that wants to
// run a tool sends its `{toolCall, options[]}` request, smedja routes it into the
// ONE `CoworkGate` (surfacing it in the same TUI approval widget the native loop
// uses), and the human's resolved `Decision` is mapped back to an ACP
// `outcome: selected{optionId}` / `cancelled`.

/// One `session/request_permission` option offered by the caller. `kind` is one
/// of `allow_once | allow_always | reject_once | reject_always`.
#[derive(Debug, Clone, Deserialize)]
struct AcpPermissionOption {
    #[serde(alias = "optionId")]
    option_id: String,
    kind: String,
}

/// The ACP tool call whose permission is being requested.
#[derive(Debug, Default, Deserialize)]
struct AcpToolCall {
    #[serde(alias = "toolName", alias = "name")]
    tool_name: Option<String>,
    #[serde(alias = "rawInput", alias = "input", alias = "arguments")]
    raw_input: Option<serde_json::Value>,
}

/// Body of `POST /acp/v1/session/{id}/request_permission`.
#[derive(Debug, Deserialize)]
struct RequestPermissionBody {
    #[serde(alias = "toolCall", default)]
    tool_call: AcpToolCall,
    #[serde(default)]
    options: Vec<AcpPermissionOption>,
}

/// The resolved industry-ACP permission outcome plus smedja's modify extension.
#[derive(Debug, PartialEq, Eq)]
enum AcpOutcome {
    /// `outcome: selected{optionId}`. `kind` is the selected option's kind (used
    /// to decide allow-always persistence); `updated_input`, when present, is the
    /// modify-rewrite handed back alongside the selection.
    Selected {
        option_id: String,
        kind: String,
        updated_input: Option<serde_json::Value>,
    },
    /// `outcome: cancelled` — no offered option matched the decision, or the gate
    /// timed out / closed without a human answer.
    Cancelled,
}

/// Maps a resolved cowork [`Decision`] to an ACP permission outcome by selecting
/// from the caller's `options`. Pure, so the mapping (including the modify and
/// allow-always cases) is unit-testable.
///
/// - `Approve` selects an allow option — preferring `allow_always` when the human
///   chose the always scope (`always == true`), else `allow_once`.
/// - `Modify` selects an allow option and carries the rewrite as `updated_input`
///   when the instruction parses to a JSON object; a non-object modify has no
///   valid rewrite, so it cancels.
/// - `Deny` selects a reject option; a gate timeout / channel close is an
///   unanswered request and maps to `Cancelled`, not an explicit reject.
/// - Any decision whose matching option class was not offered maps to `Cancelled`.
fn map_decision_to_outcome(
    decision: &Decision,
    always: bool,
    options: &[AcpPermissionOption],
) -> AcpOutcome {
    let pick = |kinds: &[&str]| -> Option<(String, String)> {
        kinds.iter().find_map(|k| {
            options
                .iter()
                .find(|o| o.kind == *k)
                .map(|o| (o.option_id.clone(), o.kind.clone()))
        })
    };
    let selected = |maybe: Option<(String, String)>, ui: Option<serde_json::Value>| match maybe {
        Some((option_id, kind)) => AcpOutcome::Selected {
            option_id,
            kind,
            updated_input: ui,
        },
        None => AcpOutcome::Cancelled,
    };
    match decision {
        Decision::Approve => {
            let order: &[&str] = if always {
                &["allow_always", "allow_once"]
            } else {
                &["allow_once", "allow_always"]
            };
            selected(pick(order), None)
        }
        Decision::Modify(instruction) => {
            match serde_json::from_str::<serde_json::Value>(instruction) {
                Ok(v) if v.is_object() => selected(pick(&["allow_once", "allow_always"]), Some(v)),
                _ => AcpOutcome::Cancelled,
            }
        }
        Decision::Deny(reason) => {
            if reason == "timeout" || reason == "channel closed" {
                AcpOutcome::Cancelled
            } else {
                selected(pick(&["reject_once", "reject_always"]), None)
            }
        }
    }
}

/// Serialises an [`AcpOutcome`] into the `session/request_permission` response
/// body. `updated_input` (smedja's modify extension) rides alongside `outcome`.
fn outcome_to_json(outcome: &AcpOutcome) -> serde_json::Value {
    match outcome {
        AcpOutcome::Selected {
            option_id,
            updated_input,
            ..
        } => {
            let mut resp = json!({
                "outcome": { "outcome": "selected", "optionId": option_id }
            });
            if let Some(ui) = updated_input {
                resp["updatedInput"] = ui.clone();
            }
            resp
        }
        AcpOutcome::Cancelled => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

/// Routes an industry-ACP `session/request_permission` into the ONE cowork gate.
///
/// Suspends (long-poll) on the gate until the human resolves it via the TUI
/// (`cowork.approve`/`deny`/`modify`, up to the gate's 30-min ceiling), then maps
/// the resolved [`Decision`] to an ACP outcome. When the resolved option is
/// `allow_always`, a matching `[[permission.rules]]` Allow entry is persisted to
/// `.smedja/workspace.toml` so the allowlist is backend-independent.
async fn request_permission(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<RequestPermissionBody>,
) -> impl IntoResponse {
    let tool_name = body.tool_call.tool_name.unwrap_or_default();
    let tool_input = body.tool_call.raw_input.unwrap_or(serde_json::Value::Null);

    // Reuse the ONE per-session gate (created on demand exactly as the cowork
    // hook path does), so ACP clients share the native approval widget.
    let gate = {
        let mut g = s.gates.lock().await;
        Arc::clone(
            g.entry(id.clone())
                .or_insert_with(|| Arc::new(CoworkGate::default())),
        )
    };

    // Suspend on the gate; a CoworkRequest is pushed to the TUI so the human sees
    // the ACP client's ask in the same widget the native loop uses.
    let (approval_id, decision) = gate
        .intercept_tracked(
            ApprovalPrompt {
                step_n: 0,
                tool: tool_name.clone(),
                args_scrubbed: tool_input.clone(),
                reasoning: String::new(),
                plan_summary: String::new(),
            },
            30 * 60,
            Some((s.dispatcher.as_ref(), None)),
        )
        .await;

    // The always-scope is only meaningful for an approval; consume the flag.
    let always = matches!(decision, Decision::Approve) && gate.take_always(&approval_id).await;
    let outcome = map_decision_to_outcome(&decision, always, &body.options);

    // Persist an allow-always rule when the resolved option is allow_always, so
    // future turns (any backend) consult the allowlist before re-asking.
    if let AcpOutcome::Selected { kind, .. } = &outcome {
        if kind == "allow_always" {
            if let Err(e) = append_allow_rule(&s.workspace, &tool_name, &tool_input) {
                tracing::warn!(error = %e, tool = %tool_name, "failed to persist allow-always rule");
            }
        }
    }

    Json(outcome_to_json(&outcome)).into_response()
}

// ── Industry-ACP `session/load` = replay ─────────────────────────────────────

/// Converts one stored checkpoint message (`{role, content}`) into an ACP
/// `session/update` notification. Returns `None` for a message with no content.
///
/// `user` role becomes a `user_message_chunk`; every other role (assistant,
/// system, tool) becomes an `agent_message_chunk`, matching the ACP model where
/// the agent replays its own prior output back to the resuming client.
fn message_to_session_update(
    session_id: &str,
    msg: &serde_json::Value,
) -> Option<serde_json::Value> {
    let content = msg.get("content")?;
    let text = content
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| content.to_string());
    if text.is_empty() {
        return None;
    }
    let role = msg
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("assistant");
    let update_kind = if role == "user" {
        "user_message_chunk"
    } else {
        "agent_message_chunk"
    };
    Some(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": update_kind,
                "content": { "type": "text", "text": text }
            }
        }
    }))
}

/// Builds the ordered list of replay notifications for a `session/load`: one
/// `session/update` per stored message, followed by a terminal
/// `session/load_complete` so the client knows the replay ended.
fn build_load_updates(session_id: &str, messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut updates: Vec<serde_json::Value> = messages
        .iter()
        .filter_map(|m| message_to_session_update(session_id, m))
        .collect();
    updates.push(json!({
        "jsonrpc": "2.0",
        "method": "session/load_complete",
        "params": { "sessionId": session_id }
    }));
    updates
}

/// Emits a finite SSE stream of the given replay `updates`.
fn build_load_sse(
    updates: Vec<serde_json::Value>,
) -> axum::response::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let stream = futures_util::stream::iter(
        updates
            .into_iter()
            .map(|u| Ok(Event::default().data(u.to_string()))),
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Implements industry-ACP `session/load{sessionId}` as replay: the session's
/// latest checkpoint history is streamed back as `session/update` notifications
/// so an editor client (Zed/Neovim/JetBrains) gets a true resume — the ACP model
/// where the agent is the source of truth and load = replay.
async fn session_load(Path(id): Path<String>, State(s): State<AcpState>) -> impl IntoResponse {
    match s.ingot.get_session(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "session not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    // The latest checkpoint holds the session's full message history; an absent
    // checkpoint replays as an empty history (just the completion marker).
    let messages = match s.ingot.latest_checkpoint(&id).await {
        Ok(Some(cp)) => {
            serde_json::from_str::<Vec<serde_json::Value>>(&cp.messages_json).unwrap_or_default()
        }
        Ok(None) => Vec::new(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    build_load_sse(build_load_updates(&id, &messages)).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::response::IntoResponse as _;
    use smedja_bellows::Dispatcher;
    use tower::ServiceExt as _;

    use super::{build_acp_router, AcpState};

    fn test_state() -> AcpState {
        let ingot = smedja_ingot::Ingot::open_in_memory().expect("in-memory ingot");
        AcpState {
            ingot: smedja_ingot::IngotHandle::new(ingot),
            dispatcher: Arc::new(Dispatcher::new(32)),
            auth_token: "test-token".to_owned(),
            workspace: std::env::temp_dir(),
            vault: Arc::new(tokio::sync::Mutex::new(
                smedja_vault::Vault::open_in_memory().expect("in-memory vault"),
            )),
            embedder: Arc::new(crate::embedder_port::FnvEmbedder::new()),
            gates: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    #[tokio::test]
    async fn post_session_new_returns_session_id() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("session_id").is_some(),
            "response must contain session_id"
        );
    }

    #[tokio::test]
    async fn missing_auth_returns_401() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn delete_unknown_session_returns_success_with_deleted_false() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/acp/v1/session/no-such-id")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // delete_session returns Ok(false) when no row matched — the handler
        // treats that as a successful deletion and returns 200.
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn set_model_returns_200_with_model_echo() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/some-session-id/model")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"model":"gemma4-27b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "gemma4-27b");
        assert_eq!(json["session_id"], "some-session-id");
    }

    #[tokio::test]
    async fn set_model_missing_field_returns_400() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/some-id/model")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r"{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_mode_persists_and_returns_200() {
        let state = test_state();
        // First create a session so update_session_mode has a row to update.
        let session_id = {
            let app = build_acp_router(state.clone());
            let resp = app
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/acp/v1/session/new")
                        .header("Authorization", "Bearer test-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            json["session_id"].as_str().unwrap().to_owned()
        };

        let app = build_acp_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/acp/v1/session/{session_id}/mode"))
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"mode":"ponytail"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["mode"], "ponytail");
        assert_eq!(json["session_id"], session_id);
    }

    #[tokio::test]
    async fn set_mode_missing_field_returns_400() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/some-id/mode")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r"{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_model_persists_override_in_db() {
        let state = test_state();
        // Create a session so the UPDATE has a row to modify.
        let session_id = {
            let app = build_acp_router(state.clone());
            let resp = app
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/acp/v1/session/new")
                        .header("Authorization", "Bearer test-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            json["session_id"].as_str().unwrap().to_owned()
        };

        let app = build_acp_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/acp/v1/session/{session_id}/model"))
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"model":"gemma4-27b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "gemma4-27b");
        assert_eq!(json["session_id"], session_id);

        // Verify the override was persisted in the DB.
        let fetched = state.ingot.get_session(&session_id).await.unwrap().unwrap();
        assert_eq!(fetched.model_override.as_deref(), Some("gemma4-27b"));
    }

    /// Verifies that the auth check uses constant-time comparison:
    /// - a token that is a prefix of the real token (different length) is rejected,
    /// - a token that shares the same length but differs in content is rejected, and
    /// - the exact correct token is accepted.
    ///
    /// A naive `==` short-circuits on the first byte mismatch (or on length
    /// mismatch), leaking timing information.  `ConstantTimeEq` pads both
    /// operands to equal length before comparing, so all three branches above
    /// must take the same code path through the comparator.
    #[tokio::test]
    async fn turn_sse_starts_with_started_and_ends_on_terminal() {
        use smedja_bellows::event::CorrelationCtx;
        use smedja_bellows::TurnEvent;

        let dispatcher = Dispatcher::new(32);
        let turn_id = "turn-sse-1".to_owned();
        let rx = dispatcher.subscribe();

        // Publish Started then Completed for the turn (buffered for the rx).
        dispatcher.publish(TurnEvent::Started {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            output_tokens: 1,
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        });

        let sse = super::build_turn_sse(rx, "s".to_owned(), turn_id);
        let resp = sse.into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);

        let started_at = text.find("Started").expect("Started must be delivered");
        let completed_at = text.find("Completed").expect("Completed must be delivered");
        assert!(
            started_at < completed_at,
            "Started must precede Completed; got: {text}"
        );
    }

    #[tokio::test]
    async fn turn_sse_ignores_other_turns_and_still_terminates() {
        use smedja_bellows::event::CorrelationCtx;
        use smedja_bellows::TurnEvent;

        let dispatcher = Dispatcher::new(32);
        let turn_id = "mine".to_owned();
        let rx = dispatcher.subscribe();

        // An event for a different turn must be ignored; heartbeats aside, the
        // stream must still end on this turn's terminal event.
        dispatcher.publish(TurnEvent::Started {
            session_id: "s".into(),
            turn_id: "someone-else".into(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Started {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Failed {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            reason: "boom".into(),
            correlation: CorrelationCtx::default(),
        });

        let sse = super::build_turn_sse(rx, "s".to_owned(), turn_id);
        let resp = sse.into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("someone-else"),
            "events for other turns must be filtered out; got: {text}"
        );
        assert!(
            text.contains("Failed"),
            "the terminal Failed event must be delivered; got: {text}"
        );
    }

    #[tokio::test]
    async fn submit_prompt_records_task_for_polling_clients() {
        let state = test_state();
        let session_id = "sse-session".to_owned();

        // Issue submit_prompt; the SSE response will hang until terminal, so we
        // race it against a short timeout and then assert the task exists.
        let app = build_acp_router(state.clone());
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/acp/v1/session/{session_id}/prompt"))
            .header("Authorization", "Bearer test-token")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"do the thing"}"#))
            .unwrap();
        // The handler creates the task synchronously before returning the
        // stream, so a short wait on the oneshot is enough to reach that point.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), app.oneshot(req)).await;

        // The turn must be recorded as a queued Task so polling still works.
        let tasks = state
            .ingot
            .list_tasks(Some("queued".to_owned()))
            .await
            .expect("list_tasks must succeed");
        assert!(
            tasks
                .iter()
                .any(|t| t.session_id.as_deref() == Some(&session_id)),
            "submit_prompt must record a queued task for the session"
        );
    }

    #[tokio::test]
    async fn mcp_endpoint_rejects_unauthenticated_request() {
        let app = build_acp_router(test_state());
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // No Authorization header → rejected before any dispatch.
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_endpoint_lists_tools_when_authenticated() {
        let app = build_acp_router(test_state());
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json["result"]["tools"].as_array().is_some(),
            "authenticated tools/list must return a tool array; got: {json}"
        );
    }

    // ── per-session mcpServers (Item A part 1) ───────────────────────────────

    #[tokio::test]
    async fn session_new_with_mcp_servers_attaches_them() {
        let state = test_state();
        let app = build_acp_router(state.clone());
        let body = serde_json::json!({
            "mcpServers": [
                {
                    "name": "gh",
                    "url": "https://example.com/mcp",
                    "transport": "http",
                    "tools": [{ "name": "gh_search" }]
                }
            ]
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["mcpServers"][0], "gh",
            "response must echo the attached server names; got: {json}"
        );

        // The server is registered (additive to the global config) and its tool
        // is now dispatchable for the session.
        let servers = state.ingot.list_mcp_servers().await.unwrap();
        assert!(
            servers.iter().any(|s| s.name == "gh"),
            "per-session MCP server must be registered"
        );
        let owner = state
            .ingot
            .find_mcp_server_for_tool("gh_search")
            .await
            .unwrap();
        assert_eq!(
            owner.map(|s| s.name).as_deref(),
            Some("gh"),
            "the attached server's tool must resolve to it"
        );
    }

    #[tokio::test]
    async fn session_new_without_body_still_creates_session() {
        // Back-compat: an empty POST body must still create a bare session.
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("session_id").is_some());
        assert_eq!(json["mcpServers"], serde_json::json!([]));
    }

    // ── ACP tool-call status stream + diff content (Item A part 2) ────────────

    #[test]
    fn tool_call_lifecycle_maps_to_acp_session_updates() {
        use smedja_bellows::event::CorrelationCtx;
        use smedja_bellows::{ToolCallStatus, TurnEvent};

        let start = TurnEvent::ToolCalled {
            tool_name: "edit_file".into(),
            input_summary: "edit x".into(),
            full_input: None,
            turn_id: Some("t".into()),
            correlation: CorrelationCtx::default(),
            tool_call_id: Some("c1".into()),
        };
        let v = super::tool_event_to_session_update("s", &start).unwrap();
        assert_eq!(v["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(v["params"]["update"]["status"], "pending");
        assert_eq!(v["params"]["update"]["toolCallId"], "c1");

        for (st, want) in [
            (ToolCallStatus::InProgress, "in_progress"),
            (ToolCallStatus::Completed, "completed"),
            (ToolCallStatus::Failed, "failed"),
        ] {
            let upd = TurnEvent::ToolCallUpdate {
                tool_call_id: "c1".into(),
                tool_name: "edit_file".into(),
                status: st,
                content: vec![],
                turn_id: Some("t".into()),
                correlation: CorrelationCtx::default(),
            };
            let v = super::tool_event_to_session_update("s", &upd).unwrap();
            assert_eq!(v["params"]["update"]["sessionUpdate"], "tool_call_update");
            assert_eq!(v["params"]["update"]["status"], want);
            assert_eq!(v["params"]["update"]["toolCallId"], "c1");
        }
    }

    #[test]
    fn tool_call_update_with_edit_emits_diff_content() {
        use smedja_bellows::event::CorrelationCtx;
        use smedja_bellows::{ToolCallContent, ToolCallStatus, TurnEvent};

        let upd = TurnEvent::ToolCallUpdate {
            tool_call_id: "c1".into(),
            tool_name: "edit_file".into(),
            status: ToolCallStatus::Completed,
            content: vec![ToolCallContent::Diff {
                path: "src/lib.rs".into(),
                old_text: "fn a() {}".into(),
                new_text: "fn a() { b(); }".into(),
            }],
            turn_id: Some("t".into()),
            correlation: CorrelationCtx::default(),
        };
        let v = super::tool_event_to_session_update("s", &upd).unwrap();
        let content = &v["params"]["update"]["content"][0];
        assert_eq!(content["type"], "diff");
        assert_eq!(content["path"], "src/lib.rs");
        assert_eq!(content["oldText"], "fn a() {}");
        assert_eq!(content["newText"], "fn a() { b(); }");
    }

    #[tokio::test]
    async fn submit_prompt_sse_reshapes_tool_events_to_acp() {
        // A tool lifecycle published to the dispatcher is reshaped into ACP
        // tool_call / tool_call_update session/update notifications on the stream,
        // ending on the turn's terminal event.
        use smedja_bellows::event::CorrelationCtx;
        use smedja_bellows::{ToolCallContent, ToolCallStatus, TurnEvent};

        let dispatcher = Dispatcher::new(64);
        let turn_id = "t-tools".to_owned();
        let rx = dispatcher.subscribe();

        dispatcher.publish(TurnEvent::ToolCalled {
            tool_name: "edit_file".into(),
            input_summary: "edit".into(),
            full_input: None,
            turn_id: Some(turn_id.clone()),
            correlation: CorrelationCtx::default(),
            tool_call_id: Some("c1".into()),
        });
        dispatcher.publish(TurnEvent::ToolCallUpdate {
            tool_call_id: "c1".into(),
            tool_name: "edit_file".into(),
            status: ToolCallStatus::InProgress,
            content: vec![],
            turn_id: Some(turn_id.clone()),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::ToolCallUpdate {
            tool_call_id: "c1".into(),
            tool_name: "edit_file".into(),
            status: ToolCallStatus::Completed,
            content: vec![ToolCallContent::Diff {
                path: "a.rs".into(),
                old_text: "x".into(),
                new_text: "y".into(),
            }],
            turn_id: Some(turn_id.clone()),
            correlation: CorrelationCtx::default(),
        });
        dispatcher.publish(TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: turn_id.clone(),
            output_tokens: 1,
            input_tokens: None,
            traceparent: None,
            correlation: CorrelationCtx::default(),
        });

        let sse = super::build_turn_sse(rx, "s".to_owned(), turn_id);
        let resp = sse.into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains(r#""sessionUpdate":"tool_call""#),
            "start must map to tool_call; got: {text}"
        );
        assert!(
            text.contains(r#""sessionUpdate":"tool_call_update""#),
            "updates must map to tool_call_update; got: {text}"
        );
        assert!(
            text.contains(r#""status":"in_progress""#),
            "in_progress transition must appear; got: {text}"
        );
        assert!(
            text.contains(r#""status":"completed""#),
            "completed transition must appear; got: {text}"
        );
        assert!(
            text.contains(r#""type":"diff""#) && text.contains("oldText"),
            "edit must carry diff content; got: {text}"
        );
    }

    // ── industry-ACP permission outcome mapping (pure) ───────────────────────

    use super::{
        map_decision_to_outcome, message_to_session_update, AcpOutcome, AcpPermissionOption,
    };
    use crate::cowork::Decision;

    fn opts() -> Vec<AcpPermissionOption> {
        serde_json::from_value(serde_json::json!([
            { "optionId": "a1", "kind": "allow_once" },
            { "optionId": "a2", "kind": "allow_always" },
            { "optionId": "r1", "kind": "reject_once" },
        ]))
        .unwrap()
    }

    #[test]
    fn approve_once_selects_allow_once_option() {
        let out = map_decision_to_outcome(&Decision::Approve, false, &opts());
        assert_eq!(
            out,
            AcpOutcome::Selected {
                option_id: "a1".into(),
                kind: "allow_once".into(),
                updated_input: None,
            }
        );
    }

    #[test]
    fn approve_always_selects_allow_always_option() {
        let out = map_decision_to_outcome(&Decision::Approve, true, &opts());
        assert_eq!(
            out,
            AcpOutcome::Selected {
                option_id: "a2".into(),
                kind: "allow_always".into(),
                updated_input: None,
            }
        );
    }

    #[test]
    fn deny_selects_reject_option() {
        let out = map_decision_to_outcome(&Decision::Deny("too risky".into()), false, &opts());
        assert_eq!(
            out,
            AcpOutcome::Selected {
                option_id: "r1".into(),
                kind: "reject_once".into(),
                updated_input: None,
            }
        );
    }

    #[test]
    fn timeout_deny_maps_to_cancelled() {
        // A gate timeout / close is an unanswered request, not an explicit reject.
        assert_eq!(
            map_decision_to_outcome(&Decision::Deny("timeout".into()), false, &opts()),
            AcpOutcome::Cancelled
        );
        assert_eq!(
            map_decision_to_outcome(&Decision::Deny("channel closed".into()), false, &opts()),
            AcpOutcome::Cancelled
        );
    }

    #[test]
    fn modify_object_selects_allow_and_carries_updated_input() {
        let out = map_decision_to_outcome(
            &Decision::Modify(r#"{"command":"ls -a"}"#.into()),
            false,
            &opts(),
        );
        match out {
            AcpOutcome::Selected {
                option_id,
                updated_input,
                ..
            } => {
                assert_eq!(option_id, "a1");
                assert_eq!(updated_input.unwrap()["command"], "ls -a");
            }
            AcpOutcome::Cancelled => panic!("object modify must select an allow option"),
        }
    }

    #[test]
    fn modify_non_object_cancels() {
        // No valid rewrite to hand back → cancelled rather than silently allowing.
        assert_eq!(
            map_decision_to_outcome(&Decision::Modify("just a string".into()), false, &opts()),
            AcpOutcome::Cancelled
        );
    }

    #[test]
    fn approve_cancels_when_no_allow_option_offered() {
        let only_reject: Vec<AcpPermissionOption> =
            serde_json::from_value(serde_json::json!([{ "optionId": "r", "kind": "reject_once" }]))
                .unwrap();
        assert_eq!(
            map_decision_to_outcome(&Decision::Approve, false, &only_reject),
            AcpOutcome::Cancelled
        );
    }

    #[test]
    fn message_to_session_update_maps_roles() {
        let u = message_to_session_update("s1", &serde_json::json!({"role":"user","content":"hi"}))
            .unwrap();
        assert_eq!(u["method"], "session/update");
        assert_eq!(u["params"]["update"]["sessionUpdate"], "user_message_chunk");
        assert_eq!(u["params"]["update"]["content"]["text"], "hi");

        let a = message_to_session_update(
            "s1",
            &serde_json::json!({"role":"assistant","content":"yo"}),
        )
        .unwrap();
        assert_eq!(
            a["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );

        // No content → skipped.
        assert!(message_to_session_update("s1", &serde_json::json!({"role":"user"})).is_none());
    }

    #[tokio::test]
    async fn request_permission_approve_routes_through_gate_to_selected() {
        // A permission request suspends on the ONE gate; a concurrent approve
        // (as the TUI would send) resolves it to `selected` on the allow_once id.
        let state = test_state();
        let session_id = "acp-perm-1".to_owned();
        let gate = {
            let mut g = state.gates.lock().await;
            Arc::clone(
                g.entry(session_id.clone())
                    .or_insert_with(|| Arc::new(crate::cowork::CoworkGate::default())),
            )
        };
        let app = build_acp_router(state.clone());
        let body = serde_json::json!({
            "toolCall": { "toolName": "write_file", "rawInput": {"path": "x"} },
            "options": [
                {"optionId": "a1", "kind": "allow_once"},
                {"optionId": "r1", "kind": "reject_once"}
            ]
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/acp/v1/session/{session_id}/request_permission"))
            .header("Authorization", "Bearer test-token")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let call = tokio::spawn(async move { app.oneshot(req).await.unwrap() });

        // Approve the suspended request once it appears.
        let id = {
            let mut found = None;
            for _ in 0..10_000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("request_permission must suspend on the gate")
        };
        assert!(gate.approve(&id).await);

        let resp = call.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["outcome"]["outcome"], "selected");
        assert_eq!(json["outcome"]["optionId"], "a1");
    }

    #[tokio::test]
    async fn request_permission_allow_always_persists_rule() {
        // Approving with allow-always scope must both select the allow_always
        // option AND write a [[permission.rules]] Allow entry to workspace.toml.
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state();
        state.workspace = tmp.path().to_path_buf();
        let session_id = "acp-perm-always".to_owned();
        let gate = {
            let mut g = state.gates.lock().await;
            Arc::clone(
                g.entry(session_id.clone())
                    .or_insert_with(|| Arc::new(crate::cowork::CoworkGate::default())),
            )
        };
        let app = build_acp_router(state.clone());
        let body = serde_json::json!({
            "toolCall": { "toolName": "bash", "rawInput": {"command": "git status"} },
            "options": [
                {"optionId": "a1", "kind": "allow_once"},
                {"optionId": "a2", "kind": "allow_always"}
            ]
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/acp/v1/session/{session_id}/request_permission"))
            .header("Authorization", "Bearer test-token")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let call = tokio::spawn(async move { app.oneshot(req).await.unwrap() });

        let id = {
            let mut found = None;
            for _ in 0..10_000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("request must suspend")
        };
        assert!(gate.approve_always(&id).await);

        let resp = call.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["outcome"]["optionId"], "a2");

        // The rule was persisted and is honoured on future turns.
        let rules = crate::cowork::load_permission_rules(tmp.path());
        assert_eq!(rules.len(), 1, "allow-always must persist exactly one rule");
        assert_eq!(rules[0].tool, "bash");
        assert_eq!(
            crate::cowork::evaluate_permission_rules(
                &rules,
                "bash",
                &serde_json::json!({"command": "git status"})
            ),
            Some(crate::cowork::PermissionDecision::Allow)
        );
    }

    #[tokio::test]
    async fn session_load_replays_history_as_session_updates() {
        let state = test_state();
        // Create a session and give it a checkpoint with a two-message history.
        let session_id = uuid::Uuid::new_v4();
        state
            .ingot
            .create_session(smedja_ingot::Session {
                id: session_id,
                mode: Some("acp".into()),
                title: String::new(),
                status: "active".into(),
                task_id: None,
                cowork_mode: false,
                created_at: smedja_types::Timestamp::now(),
                updated_at: smedja_types::Timestamp::now(),
                workspace_root: None,
                model_override: None,
                runner_override: None,
            })
            .await
            .unwrap();
        state
            .ingot
            .save_checkpoint(smedja_ingot::Checkpoint {
                id: uuid::Uuid::new_v4(),
                session_id: session_id.to_string(),
                turn_n: 0,
                messages_json: r#"[{"role":"user","content":"hello there"},{"role":"assistant","content":"general kenobi"}]"#.to_owned(),
                created_at: smedja_types::Timestamp::now(),
                compaction_id: None,
            })
            .await
            .unwrap();

        let app = build_acp_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/acp/v1/session/{session_id}/load"))
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("session/update"),
            "load must emit session/update notifications; got: {text}"
        );
        assert!(text.contains("hello there"), "user message must replay");
        assert!(
            text.contains("general kenobi"),
            "assistant message must replay"
        );
        assert!(text.contains("user_message_chunk"));
        assert!(text.contains("agent_message_chunk"));
        assert!(
            text.contains("session/load_complete"),
            "replay must end with a completion marker; got: {text}"
        );
    }

    #[tokio::test]
    async fn session_load_unknown_session_returns_404() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/no-such-session/load")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn auth_token_comparison_is_constant_time() {
        // The real token is "test-token" (10 bytes).
        // "test" is a strict prefix — a naive == would short-circuit on length.
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "prefix token must be rejected"
        );

        // Same length as "test-token" but wrong content.
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer XXXX-XXXXX")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "wrong same-length token must be rejected"
        );

        // Correct token must be accepted.
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "correct token must be accepted"
        );
    }
}
