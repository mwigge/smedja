//! Generic Agent Client Protocol (ACP) client provider.
//!
//! Drives any ACP-capable agent CLI (`kimi acp`, `gemini --acp`, …) as a
//! subprocess speaking newline-delimited JSON-RPC 2.0 over stdio, and — the
//! reason this exists — routes the agent's `session/request_permission`
//! requests through smedja's per-session approval gate
//! ([`crate::ToolGate`] on [`CallOptions`]), so an external agent's tool
//! calls get the same approve/deny prompt as in-process tools. This closes
//! the gap left by one-shot prompt modes (`kimi -p`, `claude -p`) that
//! auto-approve their own tools.
//!
//! Wire shapes were verified live against kimi-code 0.27.0 and gemini-cli
//! 0.49.0 (both protocol version 1):
//!
//! ```text
//! → {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,...}}
//! → {"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/abs","mcpServers":[]}}
//! → {"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"…","prompt":[{"type":"text","text":"…"}]}}
//! ← {"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk",…}}}
//! ← {"id":9,"method":"session/request_permission","params":{"options":[…],"toolCall":{…}}}
//! → {"jsonrpc":"2.0","id":9,"result":{"outcome":{"outcome":"selected","optionId":"…"}}}
//! ```
//!
//! The client declares no fs/terminal capabilities, so agents execute tools
//! themselves and only ask for permission; any capability request an agent
//! sends anyway is answered with JSON-RPC `-32601` (method not found).

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AdapterError, CallOptions, Delta, DeltaStream, Message, Provider, Role, SubprocessProvider,
    ToolGateDecision,
};

/// Handshake budget: `initialize` + `session/new` must complete within this
/// window or the turn fails (a wedged agent must not hang a turn silently).
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Description of an ACP-capable agent CLI: the binary to spawn and the
/// arguments that put it in ACP server mode.
#[derive(Debug, Clone, Copy)]
pub struct AcpAgentSpec {
    /// Short agent name used in logs and errors.
    pub name: &'static str,
    /// Binary looked up on `$PATH`.
    pub binary: &'static str,
    /// Arguments selecting ACP stdio mode.
    pub args: &'static [&'static str],
    /// The id of the agent's model config option, when the agent exposes one
    /// in its `session/new` response (kimi does; the client then issues a
    /// best-effort `session/set_config_option`). Empty disables selection.
    pub model_config_id: &'static str,
}

/// Built-in spec for Kimi Code CLI (`kimi acp`).
pub const KIMI_ACP: AcpAgentSpec = AcpAgentSpec {
    name: "kimi",
    binary: "kimi",
    args: &["acp"],
    model_config_id: "model",
};

/// Built-in spec for Gemini CLI (`gemini --acp`).
pub const GEMINI_ACP: AcpAgentSpec = AcpAgentSpec {
    name: "gemini",
    binary: "gemini",
    args: &["--acp"],
    model_config_id: "model",
};

/// A [`Provider`] that drives an ACP agent subprocess for each turn.
pub struct AcpProvider {
    spec: AcpAgentSpec,
}

impl AcpProvider {
    /// Creates a provider for `spec` without probing the binary.
    #[must_use]
    pub fn new(spec: AcpAgentSpec) -> Self {
        Self { spec }
    }

    /// Returns `Some(Self)` when `spec.binary` is on `$PATH`.
    #[must_use]
    pub fn detect(spec: AcpAgentSpec) -> Option<Self> {
        SubprocessProvider::available(spec.binary).then(|| Self::new(spec))
    }
}

impl Provider for AcpProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        stream_acp(self.spec, messages, opts)
    }
}

/// Renders the conversation into a single prompt text block.
///
/// ACP `session/prompt` takes content blocks, not role-tagged history, and the
/// smedja orchestrator re-renders the full conversation each turn (no reliance
/// on the agent's own session store — same policy as the claude/kimi one-shot
/// adapters). The system block leads the prompt.
fn render_conversation(messages: &[Message], system: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(system) = system {
        if !system.trim().is_empty() {
            out.push_str("System: ");
            out.push_str(system);
            out.push_str("\n\n");
        }
    }
    let dialogue: Vec<&Message> = messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .collect();
    match dialogue.as_slice() {
        [] => {
            if let Some(m) = messages.last() {
                out.push_str(&m.content);
            }
        }
        [single] => out.push_str(&single.content),
        many => {
            for m in many {
                let label = match m.role {
                    Role::Assistant => "Assistant",
                    _ => "Human",
                };
                out.push_str(label);
                out.push_str(": ");
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
        }
    }
    out
}

/// One live ACP connection: the child process plus framed stdio halves.
struct AcpConn {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl AcpConn {
    async fn send_request(&mut self, method: &str, params: Value) -> Result<u64, AdapterError> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_frame(&frame).await?;
        Ok(id)
    }

    async fn send_result(&mut self, id: &Value, result: Value) -> Result<(), AdapterError> {
        let frame = json!({"jsonrpc": "2.0", "id": id, "result": result});
        self.write_frame(&frame).await
    }

    async fn send_error(
        &mut self,
        id: &Value,
        code: i64,
        message: &str,
    ) -> Result<(), AdapterError> {
        let frame =
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        self.write_frame(&frame).await
    }

    async fn write_frame(&mut self, frame: &Value) -> Result<(), AdapterError> {
        let mut line = frame.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AdapterError::Request(format!("acp stdin write: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| AdapterError::Request(format!("acp stdin flush: {e}")))
    }

    /// Reads the next parsed JSON message, skipping non-JSON noise lines.
    async fn read_message(&mut self) -> Result<Option<Value>, AdapterError> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| AdapterError::Request(format!("acp stdout read: {e}")))?;
            if n == 0 {
                return Ok(None);
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                return Ok(Some(value));
            }
        }
    }
}

/// Waits for the response to `request_id`, servicing any interleaved
/// agent-side requests/notifications via `on_message` (which must handle
/// them; incoming requests it does not consume are answered method-not-found).
async fn wait_response(
    conn: &mut AcpConn,
    request_id: u64,
) -> Result<Result<Value, Value>, AdapterError> {
    loop {
        let Some(msg) = conn.read_message().await? else {
            return Err(AdapterError::Request(
                "acp agent closed the stream before responding".to_owned(),
            ));
        };
        if msg.get("id").and_then(Value::as_u64) == Some(request_id) && msg.get("method").is_none()
        {
            if let Some(err) = msg.get("error") {
                return Ok(Err(err.clone()));
            }
            return Ok(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
        }
        // Requests arriving during the handshake (rare) are refused safely;
        // notifications are ignored until the prompt loop takes over.
        if msg.get("method").is_some() && msg.get("id").is_some() {
            let id = msg["id"].clone();
            conn.send_error(&id, -32601, "method not supported by this client")
                .await?;
        }
    }
}

fn stream_acp(spec: AcpAgentSpec, messages: &[Message], opts: &CallOptions) -> DeltaStream {
    let prompt = render_conversation(messages, opts.system.as_deref());
    let model = opts.model.clone();
    let workspace = opts.workspace.clone();
    let tool_gate = opts.tool_gate.clone();
    let permission_mode = opts.permission_mode.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Delta, AdapterError>>(64);

    tokio::spawn(async move {
        let fail = |detail: String| AdapterError::Request(format!("acp[{}]: {detail}", spec.name));

        let mut command = tokio::process::Command::new(spec.binary);
        command
            .args(spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // An interrupted turn (turn.cancel aborts the run task) must kill
            // the agent instead of leaking a runaway subprocess.
            .kill_on_drop(true);
        if let Some(dir) = workspace.as_ref().filter(|d| d.is_dir()) {
            command.current_dir(dir);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = tx.send(Err(fail(format!("spawn: {e}")))).await;
                return;
            }
        };
        let Some(stdin) = child.stdin.take() else {
            let _ = tx.send(Err(fail("no stdin handle".to_owned()))).await;
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = tx.send(Err(fail("no stdout handle".to_owned()))).await;
            return;
        };
        let stderr = child.stderr.take();
        let mut conn = AcpConn {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
            next_id: 0,
        };

        let outcome = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            handshake(&mut conn, workspace.as_deref(), &model, spec),
        )
        .await;
        let session_id = match outcome {
            Ok(Ok(session_id)) => session_id,
            Ok(Err(e)) => {
                let detail = drain_failure(conn.child, stderr, e).await;
                let _ = tx.send(Err(fail(detail))).await;
                return;
            }
            Err(_) => {
                let _ = tx
                    .send(Err(fail(format!(
                        "handshake timed out after {}s (is the agent authenticated? try `{} login`)",
                        HANDSHAKE_TIMEOUT.as_secs(),
                        spec.binary
                    ))))
                    .await;
                return;
            }
        };
        let _ = tx.send(Ok(Delta::SessionId(session_id.clone()))).await;

        let prompt_id = match conn
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": prompt}],
                }),
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        };

        if let Err(e) = prompt_loop(&mut conn, prompt_id, &tx, tool_gate, permission_mode).await {
            let detail = drain_failure(conn.child, stderr, e).await;
            let _ = tx.send(Err(fail(detail))).await;
            return;
        }

        // Close stdin so the agent exits; reap it (kill_on_drop covers the
        // uncooperative case).
        drop(conn.stdin);
        let _ = conn.child.wait().await;
    });

    Box::pin(ReceiverStream::new(rx))
}

/// `initialize` + `session/new` (+ best-effort model selection). Returns the
/// ACP session id.
async fn handshake(
    conn: &mut AcpConn,
    workspace: Option<&std::path::Path>,
    model: &str,
    spec: AcpAgentSpec,
) -> Result<String, AdapterError> {
    let init_id = conn
        .send_request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                },
                "clientInfo": {"name": "smedja", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
    if let Err(err) = wait_response(conn, init_id).await? {
        return Err(AdapterError::Request(format!("initialize failed: {err}")));
    }

    let cwd = workspace
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let new_id = conn
        .send_request(
            "session/new",
            json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
        )
        .await?;
    let new_result = match wait_response(conn, new_id).await? {
        Ok(v) => v,
        Err(err) => {
            return Err(AdapterError::Request(format!(
                "session/new failed: {err} (is the agent authenticated? try `{} login`)",
                spec.binary
            )));
        }
    };
    let session_id = new_result
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Request("session/new returned no sessionId".to_owned()))?
        .to_owned();

    // Best-effort model selection: only when the agent advertises a matching
    // config option whose values include the requested model. A missing
    // option, unknown value, or agent-side error falls back to the agent's
    // default model rather than failing the turn.
    if !model.is_empty() && !spec.model_config_id.is_empty() {
        let advertised = new_result
            .get("configOptions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|opt| opt.get("id").and_then(Value::as_str) == Some(spec.model_config_id));
        let value_known = advertised.is_some_and(|opt| {
            opt.get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|v| v.get("value").and_then(Value::as_str) == Some(model))
        });
        if value_known {
            let set_id = conn
                .send_request(
                    "session/set_config_option",
                    json!({
                        "sessionId": session_id,
                        "configOptionId": spec.model_config_id,
                        "value": model,
                    }),
                )
                .await?;
            if let Err(err) = wait_response(conn, set_id).await? {
                tracing::warn!(
                    agent = spec.name,
                    model,
                    %err,
                    "acp model selection failed; using the agent's default model"
                );
            }
        } else {
            tracing::debug!(
                agent = spec.name,
                model,
                "model not advertised by agent config options; using agent default"
            );
        }
    }

    Ok(session_id)
}

/// Streams `session/update` notifications as [`Delta`]s and answers
/// `session/request_permission` via the injected gate until the prompt
/// response arrives.
async fn prompt_loop(
    conn: &mut AcpConn,
    prompt_id: u64,
    tx: &tokio::sync::mpsc::Sender<Result<Delta, AdapterError>>,
    tool_gate: Option<crate::ToolGate>,
    permission_mode: Option<String>,
) -> Result<(), AdapterError> {
    // Accumulated tool-call argument text keyed by toolCallId. Kimi's
    // `session/request_permission` omits `rawInput`, but the arguments stream
    // beforehand as `tool_call_update` content — captured here so the approval
    // prompt can show WHAT is being approved, not just the tool name.
    let mut args_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    loop {
        let Some(msg) = conn.read_message().await? else {
            return Err(AdapterError::Request(
                "agent closed the stream mid-turn".to_owned(),
            ));
        };

        // The prompt response ends the turn.
        if msg.get("id").and_then(Value::as_u64) == Some(prompt_id) && msg.get("method").is_none() {
            if let Some(err) = msg.get("error") {
                return Err(AdapterError::Request(format!("session/prompt: {err}")));
            }
            let stop = msg
                .pointer("/result/stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn");
            if stop == "refusal" {
                return Err(AdapterError::Request("agent refused the prompt".to_owned()));
            }
            return Ok(());
        }

        match msg.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                record_streamed_args(&msg, &mut args_by_id);
                if let Some(delta) = map_session_update(&msg) {
                    if tx.send(Ok(delta)).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Some("session/request_permission") => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let params = msg.get("params").unwrap_or(&Value::Null);
                let fallback_input = params
                    .pointer("/toolCall/toolCallId")
                    .and_then(Value::as_str)
                    .and_then(|call_id| args_by_id.get(call_id))
                    .map(|raw| {
                        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
                    })
                    .unwrap_or(Value::Null);
                let outcome = decide_permission(
                    params,
                    fallback_input,
                    tool_gate.as_ref(),
                    permission_mode.as_deref(),
                )
                .await;
                conn.send_result(&id, json!({"outcome": outcome})).await?;
            }
            // Any other agent-side request (fs/*, terminal/*) was not granted
            // in our capabilities; refuse it rather than wedge the agent.
            Some(_) if msg.get("id").is_some() => {
                let id = msg["id"].clone();
                conn.send_error(&id, -32601, "method not supported by this client")
                    .await?;
            }
            _ => {}
        }
    }
}

/// Records a tool call's argument text from `tool_call`/`tool_call_update`
/// notifications. Kimi streams cumulative snapshots of the argument JSON (each
/// update carries the whole prefix so far); other agents may stream true
/// deltas — a snapshot that extends the stored text replaces it, anything else
/// appends.
fn record_streamed_args(msg: &Value, args_by_id: &mut std::collections::HashMap<String, String>) {
    let Some(update) = msg.pointer("/params/update") else {
        return;
    };
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if !matches!(kind, Some("tool_call" | "tool_call_update")) {
        return;
    }
    let Some(call_id) = update.get("toolCallId").and_then(Value::as_str) else {
        return;
    };
    if let Some(raw_input) = update.get("rawInput") {
        if !raw_input.is_null() {
            args_by_id.insert(call_id.to_owned(), raw_input.to_string());
            return;
        }
    }
    // A completed/failed update's content is the tool RESULT, not arguments.
    if matches!(
        update.get("status").and_then(Value::as_str),
        Some("completed" | "failed")
    ) {
        return;
    }
    let text = content_text(update);
    if text.is_empty() {
        return;
    }
    let entry = args_by_id.entry(call_id.to_owned()).or_default();
    if text.starts_with(entry.as_str()) {
        *entry = text;
    } else {
        entry.push_str(&text);
    }
}

/// Maps one `session/update` notification to a [`Delta`].
fn map_session_update(msg: &Value) -> Option<Delta> {
    let update = msg.pointer("/params/update")?;
    match update.get("sessionUpdate").and_then(Value::as_str)? {
        "agent_message_chunk" => update
            .pointer("/content/text")
            .and_then(Value::as_str)
            .map(|t| Delta::Text(t.to_owned())),
        "tool_call" => {
            let name = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let input = update
                .get("rawInput")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Some(Delta::ToolCall { name, input })
        }
        "tool_call_update" => {
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            let text = content_text(update);
            match status {
                "completed" => Some(Delta::ToolResult {
                    tool_use_id: update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    content: text,
                }),
                "failed" => Some(Delta::ToolResult {
                    tool_use_id: update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    content: format!("failed: {text}"),
                }),
                // Streaming argument fragments — display-only.
                _ if !text.is_empty() => Some(Delta::ToolCallChunk {
                    name: String::new(),
                    partial_input: text,
                }),
                _ => None,
            }
        }
        // Thinking, plans, command lists, mode changes: not part of the
        // response text.
        _ => None,
    }
}

/// Joins the text fragments of a tool-call update's `content` array.
fn content_text(update: &Value) -> String {
    update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            c.pointer("/content/text")
                .or_else(|| c.get("text"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Resolves a `session/request_permission` into an ACP outcome object.
///
/// `auto` mode allows without consulting the gate (matching the in-process
/// policy). Otherwise the injected [`crate::ToolGate`] decides; when the gate
/// is absent the request fails CLOSED (reject) — mirroring the claude
/// adapter's deny-all fallback when a gate is expected but unavailable.
async fn decide_permission(
    params: &Value,
    fallback_input: Value,
    tool_gate: Option<&crate::ToolGate>,
    permission_mode: Option<&str>,
) -> Value {
    let tool_name = params
        .pointer("/toolCall/title")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    // Prefer the request's own rawInput; fall back to the arguments streamed
    // via earlier tool_call_update notifications (kimi omits rawInput here).
    let input = params
        .pointer("/toolCall/rawInput")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or(fallback_input);

    let is_auto = permission_mode.is_some_and(|m| m.eq_ignore_ascii_case("auto"));
    let decision = if is_auto {
        ToolGateDecision::Allow
    } else if let Some(gate) = tool_gate {
        gate.decide(tool_name, input).await
    } else {
        tracing::error!(
            "acp permission request with no gate installed; denying fail-closed \
             (permission mode is not auto)"
        );
        ToolGateDecision::Deny("smedja approval gate unavailable; denied fail-closed".to_owned())
    };

    let options = params.get("options").and_then(Value::as_array);
    // Only ever select an option whose kind matches the decision's intent —
    // never fall back to an arbitrary option (picking a stray allow while
    // denying would defeat the gate). No match → `cancelled`.
    let pick = |kinds: &[&str]| -> Option<String> {
        let options = options?;
        kinds
            .iter()
            .find_map(|k| {
                options
                    .iter()
                    .find(|o| o.get("kind").and_then(Value::as_str) == Some(*k))
            })
            .and_then(|o| o.get("optionId").and_then(Value::as_str))
            .map(str::to_owned)
    };

    let option_id = match &decision {
        ToolGateDecision::Allow => pick(&["allow_once", "allow_always"]),
        ToolGateDecision::AllowAlways => pick(&["allow_always", "allow_once"]),
        ToolGateDecision::Deny(_) => pick(&["reject_once", "reject_always"]),
    };
    match option_id {
        Some(option_id) => json!({"outcome": "selected", "optionId": option_id}),
        // No usable options (malformed request): cancel is the safe outcome.
        None => json!({"outcome": "cancelled"}),
    }
}

/// Collects stderr/exit detail for a failed turn.
async fn drain_failure(
    mut child: tokio::process::Child,
    stderr: Option<tokio::process::ChildStderr>,
    e: AdapterError,
) -> String {
    let _ = child.kill().await;
    let mut detail = e.to_string();
    if let Some(mut stderr) = stderr {
        use tokio::io::AsyncReadExt as _;
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        let trimmed = buf.trim();
        if !trimmed.is_empty() {
            // Keep the tail — node agents print long stack traces.
            let tail: String = trimmed
                .lines()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ");
            detail.push_str(": ");
            detail.push_str(&tail);
        }
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    use crate::TEST_ENV_LOCK as ENV_LOCK;

    fn update(update: Value) -> Value {
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":update}})
    }

    #[test]
    fn maps_agent_message_chunk_to_text() {
        let msg = update(
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}),
        );
        assert_eq!(map_session_update(&msg), Some(Delta::Text("hi".into())));
    }

    #[test]
    fn thought_chunks_are_not_response_text() {
        let msg = update(
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}),
        );
        assert_eq!(map_session_update(&msg), None);
    }

    #[test]
    fn maps_tool_call_and_completed_update() {
        let call = update(
            json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"Write","kind":"edit","status":"pending"}),
        );
        assert_eq!(
            map_session_update(&call),
            Some(Delta::ToolCall {
                name: "Write".into(),
                input: Value::Null,
            })
        );
        let done = update(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"ok"}}]}),
        );
        assert_eq!(
            map_session_update(&done),
            Some(Delta::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
            })
        );
    }

    #[test]
    fn streaming_argument_fragments_become_chunks() {
        let frag = update(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"{\"pa"}}]}),
        );
        assert_eq!(
            map_session_update(&frag),
            Some(Delta::ToolCallChunk {
                name: String::new(),
                partial_input: "{\"pa".into(),
            })
        );
    }

    fn permission_params() -> Value {
        json!({
            "sessionId": "s",
            "options": [
                {"optionId": "approve_once", "name": "Approve once", "kind": "allow_once"},
                {"optionId": "approve_always", "name": "Approve always", "kind": "allow_always"},
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
            ],
            "toolCall": {"toolCallId": "t1", "title": "Write", "rawInput": {"path": "x"}}
        })
    }

    #[tokio::test]
    async fn permission_allows_via_gate() {
        let gate = crate::ToolGate::new(|name, _input| {
            Box::pin(async move {
                assert_eq!(name, "Write");
                ToolGateDecision::Allow
            })
        });
        let outcome =
            decide_permission(&permission_params(), Value::Null, Some(&gate), Some("ask")).await;
        assert_eq!(outcome["outcome"], "selected");
        assert_eq!(outcome["optionId"], "approve_once");
    }

    #[tokio::test]
    async fn permission_denies_via_gate() {
        let gate =
            crate::ToolGate::new(|_, _| Box::pin(async { ToolGateDecision::Deny("no".into()) }));
        let outcome =
            decide_permission(&permission_params(), Value::Null, Some(&gate), Some("ask")).await;
        assert_eq!(outcome["outcome"], "selected");
        assert_eq!(outcome["optionId"], "reject");
    }

    #[tokio::test]
    async fn permission_fails_closed_without_gate() {
        let outcome = decide_permission(&permission_params(), Value::Null, None, Some("ask")).await;
        assert_eq!(outcome["outcome"], "selected");
        assert_eq!(outcome["optionId"], "reject");
    }

    #[tokio::test]
    async fn permission_auto_mode_allows_without_gate() {
        let outcome =
            decide_permission(&permission_params(), Value::Null, None, Some("auto")).await;
        assert_eq!(outcome["outcome"], "selected");
        assert_eq!(outcome["optionId"], "approve_once");
    }

    #[test]
    fn streamed_args_reach_the_permission_fallback() {
        let mut args = std::collections::HashMap::new();
        // Kimi streams cumulative snapshots of the argument JSON.
        for frag in ["{\"pa", "{\"path\":", "{\"path\": \"x\"}"] {
            let msg = update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "t1",
                "status": "in_progress",
                "content": [{"type": "content", "content": {"type": "text", "text": frag}}],
            }));
            record_streamed_args(&msg, &mut args);
        }
        assert_eq!(
            args.get("t1").map(String::as_str),
            Some("{\"path\": \"x\"}")
        );
        // A completed update's content is the RESULT and must not clobber args.
        let done = update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "wrote 5 bytes"}}],
        }));
        record_streamed_args(&done, &mut args);
        assert_eq!(
            args.get("t1").map(String::as_str),
            Some("{\"path\": \"x\"}")
        );
    }

    #[tokio::test]
    async fn permission_fallback_input_feeds_the_gate() {
        let gate = crate::ToolGate::new(|_, input| {
            Box::pin(async move {
                assert_eq!(input["path"], "x");
                ToolGateDecision::Allow
            })
        });
        // Request without rawInput — the streamed-args fallback must be used.
        let outcome = decide_permission(
            &permission_params_without_raw_input(),
            json!({"path": "x"}),
            Some(&gate),
            Some("ask"),
        )
        .await;
        assert_eq!(outcome["optionId"], "approve_once");
    }

    fn permission_params_without_raw_input() -> Value {
        json!({
            "sessionId": "s",
            "options": [
                {"optionId": "approve_once", "name": "Approve once", "kind": "allow_once"},
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
            ],
            "toolCall": {"toolCallId": "t1", "title": "Write"}
        })
    }

    #[tokio::test]
    async fn permission_cancels_on_malformed_options() {
        let params = json!({"toolCall": {"title": "X"}});
        let outcome = decide_permission(&params, Value::Null, None, Some("auto")).await;
        assert_eq!(outcome["outcome"], "cancelled");
    }

    /// Full-stream test against a mock ACP agent scripted in shell: handshake,
    /// a permission request answered allow, message chunks, prompt response.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the stream to serialize $PATH mutation
    async fn streams_mock_acp_agent_with_gated_permission() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!(
            "smedja-acp-mock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let seen_file = temp_dir.join("seen.jsonl");
        let script_path = temp_dir.join("acp-mock");
        // The mock reads our frames line-by-line and answers by shape:
        // initialize → caps; session/new → sessionId; session/prompt →
        // emit a permission request, wait for the reply, then chunks + stop.
        std::fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
exec 3>>'{seen}'
while IFS= read -r line; do
  printf '%s\n' "$line" >&3
  case "$line" in
    *'"initialize"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}' ;;
    *'"session/new"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"mock-acp-session"}}}}' ;;
    *'"session/prompt"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":100,"method":"session/request_permission","params":{{"sessionId":"mock-acp-session","options":[{{"optionId":"ok","kind":"allow_once"}},{{"optionId":"no","kind":"reject_once"}}],"toolCall":{{"toolCallId":"t1","title":"Bash","rawInput":{{"command":"ls"}}}}}}}}' ;;
    *'"id":100'*)
      printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"mock-acp-session","update":{{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Bash","status":"pending"}}}}}}'
      printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"mock-acp-session","update":{{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{{"type":"content","content":{{"type":"text","text":"files"}}}}]}}}}}}'
      printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"mock-acp-session","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"done"}}}}}}}}'
      printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
      ;;
  esac
done
"#,
                seen = seen_file.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script_path, permissions).unwrap();
        }

        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", temp_dir.display()));

        const MOCK: AcpAgentSpec = AcpAgentSpec {
            name: "mock",
            binary: "acp-mock",
            args: &[],
            model_config_id: "",
        };
        let provider = AcpProvider::detect(MOCK).expect("mock agent on PATH");

        let gated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gated_flag = gated.clone();
        let gate = crate::ToolGate::new(move |name, input| {
            let gated_flag = gated_flag.clone();
            Box::pin(async move {
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "ls");
                gated_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                ToolGateDecision::Allow
            })
        });
        let opts = CallOptions {
            model: String::new(),
            max_tokens: None,
            temperature: None,
            system: Some("be terse".into()),
            tools: None,
            provider_session_id: None,
            smedja_session_id: None,
            permission_mode: Some("ask".into()),
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
            workspace: None,
            tool_gate: Some(gate),
        };
        let messages = vec![Message {
            role: Role::User,
            content: "list files".into(),
        }];

        let mut stream = provider.stream_chat(&messages, &opts);
        let mut deltas = Vec::new();
        while let Some(item) = stream.next().await {
            deltas.push(item.expect("no adapter errors"));
        }

        std::env::set_var("PATH", old_path);

        assert!(
            gated.load(std::sync::atomic::Ordering::SeqCst),
            "the permission request must flow through the gate"
        );
        assert!(deltas.contains(&Delta::SessionId("mock-acp-session".into())));
        assert!(deltas.contains(&Delta::ToolCall {
            name: "Bash".into(),
            input: Value::Null,
        }));
        assert!(deltas.contains(&Delta::ToolResult {
            tool_use_id: "t1".into(),
            content: "files".into(),
        }));
        assert!(deltas.contains(&Delta::Text("done".into())));

        // The permission reply must have selected the allow option.
        let seen = std::fs::read_to_string(&seen_file).unwrap();
        assert!(
            seen.contains(r#""optionId":"ok""#),
            "allow option must be selected; frames were:\n{seen}"
        );
        // The prompt must carry the system block.
        assert!(seen.contains("System: be terse"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
