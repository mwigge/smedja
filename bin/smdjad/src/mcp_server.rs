//! MCP server mode — exposes smedja's read-safe native tools over JSON-RPC 2.0.
//!
//! smedja is co-mounted as an MCP *server* on the existing ACP HTTP listener.
//! It reuses the [`smedja_rpc`] request/response envelope (MCP is JSON-RPC 2.0)
//! and routes `tools/call` into [`crate::executor::execute_tool`] under an
//! effective read-only (`review`-mode) session, so the least-privilege guard
//! rejects mutating tools even if [`MCP_SERVER_TOOLS`] ever drifts.

use std::sync::Arc;

use serde_json::{json, Value};
use smedja_ingot::{IngotHandle, Session};
use smedja_rpc::{codes, Request, Response, RpcError};
use smedja_types::Timestamp;
use smedja_vault::{Vault, VaultEntry, SHARED_BLOCK_NAMESPACE};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::embedder_port::Embedder;
use crate::executor::{execute_tool, MCP_SERVER_TOOLS};

/// Cross-client shared-memory tools this MCP server exposes on top of the
/// read-safe [`MCP_SERVER_TOOLS`] surface.
///
/// These are handled natively in [`handle_tools_call`] (they are *not*
/// [`execute_tool`] tools and are deliberately absent from [`MCP_SERVER_TOOLS`]),
/// letting external MCP clients (Cursor, Claude Desktop, Claude Code) share
/// smedja's vault as a common memory store. `memory_search`/`memory_list` are
/// read-only; `memory_write` is bounded to a single, clearly-scoped write target
/// (see [`MCP_MEMORY_WRITE_NAMESPACE`]) so an external client can never write into
/// smedja's internal namespaces.
pub(crate) const MCP_MEMORY_TOOLS: &[&str] = &["memory_search", "memory_write", "memory_list"];

/// Namespaces an external MCP client may *read* through `memory_search`/
/// `memory_list`. Reads never mutate, but the surface is still allow-listed so a
/// client cannot probe arbitrary internal namespaces by name.
const MCP_MEMORY_READ_NAMESPACES: &[&str] =
    &["default", "compact", "warm", "handoff", "mcp_shared"];

/// The single namespace `memory_write` targets for free-form (non-block) writes.
///
/// External writes are confined here — a bounded, clearly-scoped shared drawer —
/// so cross-client writes can never land in smedja's internal `warm`/`handoff`/
/// `compact` coordination namespaces. Block-scoped writes (with a `block_id`) go
/// to the durable shared-block namespace instead, which is equally bounded.
const MCP_MEMORY_WRITE_NAMESPACE: &str = "mcp_shared";

/// Returns the JSON-Schema input descriptor advertised for `tool_name`.
///
/// Schemas are intentionally permissive object schemas; they document the
/// parameter names a client should send without over-constraining the call.
fn input_schema(tool_name: &str) -> Value {
    let object = |props: Value| json!({ "type": "object", "properties": props });
    match tool_name {
        "read_file" | "list_files" => object(json!({ "path": { "type": "string" } })),
        "graph_query" => object(json!({
            "query": { "type": "string" },
            "depth": { "type": "integer" }
        })),
        "smedja_vault_search" => object(json!({
            "query": { "type": "string" },
            "namespace": { "type": "string" },
            "k": { "type": "integer" }
        })),
        "smedja_retrieve" => object(json!({ "hash": { "type": "string" } })),
        "otel_query" => object(json!({
            "service": { "type": "string" },
            "filter": { "type": "string" },
            "range_minutes": { "type": "integer" }
        })),
        "metric_query" => object(json!({
            "promql": { "type": "string" },
            "range_minutes": { "type": "integer" }
        })),
        "log_tail" => object(json!({
            "service": { "type": "string" },
            "filter": { "type": "string" },
            "lines": { "type": "integer" }
        })),
        _ => object(json!({})),
    }
}

/// Builds the `prompts/list` result from the workspace bundle.
///
/// Every bundle skill and rule is advertised as an MCP *prompt*, so any external
/// MCP client (Cursor, Claude Desktop, …) sees the identical one-folder skills
/// smedja injects internally. Agents are omitted — they are routing targets, not
/// prompts.
fn prompts_list_result(workspace: &std::path::Path) -> Value {
    let bundle = crate::bundle_config::load_bundle(workspace);
    let prompts: Vec<Value> = bundle
        .items
        .iter()
        .filter(|i| i.kind != smedja_plugins::BundleKind::Agent)
        .map(|i| {
            json!({
                "name": i.name,
                "description": i.description.lines().next().unwrap_or("").trim(),
            })
        })
        .collect();
    json!({ "prompts": prompts })
}

/// Builds a `prompts/get` result for a named bundle skill/rule, returning its
/// body as a single user-role prompt message. Returns `Err` with a client-facing
/// message when the name is unknown.
fn prompts_get_result(workspace: &std::path::Path, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing prompt name".to_owned())?;
    let bundle = crate::bundle_config::load_bundle(workspace);
    let item = bundle
        .find(name)
        .filter(|i| i.kind != smedja_plugins::BundleKind::Agent)
        .ok_or_else(|| format!("prompt not found: {name}"))?;
    Ok(json!({
        "description": item.description.lines().next().unwrap_or("").trim(),
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": item.body },
        }],
    }))
}

/// Builds the `resources/list` result: every bundle item's supporting files,
/// exposed as `file://` resources so a client can fetch a skill's helper assets.
fn resources_list_result(workspace: &std::path::Path) -> Value {
    let bundle = crate::bundle_config::load_bundle(workspace);
    let mut resources: Vec<Value> = Vec::new();
    for item in &bundle.items {
        for (rel, abs) in item
            .supporting_files
            .iter()
            .zip(item.supporting_file_paths())
        {
            resources.push(json!({
                "uri": format!("file://{}", abs.display()),
                "name": format!("{}/{rel}", item.name),
                "mimeType": "text/plain",
            }));
        }
    }
    json!({ "resources": resources })
}

/// Builds the `tools/list` result advertising the read-safe subset plus the
/// cross-client `memory_*` tools.
fn tools_list_result() -> Value {
    let mut tools: Vec<Value> = MCP_SERVER_TOOLS
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "description": format!("smedja native tool: {name}"),
                "inputSchema": input_schema(name),
            })
        })
        .collect();
    tools.extend(memory_tools_list());
    json!({ "tools": tools })
}

/// Advertises the cross-client shared-memory tools with their own explicit
/// schemas, kept separate from [`input_schema`] so the Phase-1 skills/prompts and
/// Phase-2 executor surfaces are untouched — this is purely additive.
fn memory_tools_list() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_search",
            "description": "Semantic search over smedja's shared memory (read-only, same-model, allow-listed namespaces).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "namespace": { "type": "string" },
                    "k": { "type": "integer" }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "memory_write",
            "description": "Write to smedja's shared memory. With `block_id` it appends to a live shared block; otherwise it stores into the bounded 'mcp_shared' drawer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "block_id": { "type": "string" },
                    "author": { "type": "string" },
                    "id": { "type": "string" },
                    "payload": { "type": "object" }
                },
                "required": ["content"]
            }
        }),
        json!({
            "name": "memory_list",
            "description": "List shared memory: with `block_id`, the segments of one shared block; otherwise per-namespace entry counts for the read-safe namespaces.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "block_id": { "type": "string" }
                }
            }
        }),
    ]
}

/// Dispatches a `memory_*` tool call, returning `(text, is_error)` in the same
/// shape [`handle_tools_call`] wraps native-tool output.
async fn handle_memory_tool(
    name: &str,
    args: &Value,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
) -> (String, bool) {
    match name {
        "memory_search" => memory_search(args, vault, embedder).await,
        "memory_write" => memory_write(args, vault, embedder).await,
        "memory_list" => memory_list(args, vault, embedder).await,
        // Unreachable: the caller only routes MCP_MEMORY_TOOLS here.
        _ => (format!("error: unknown memory tool: {name}"), true),
    }
}

/// `memory_search` — semantic read over an allow-listed namespace.
///
/// Honours the vault's same-model filter (it passes the live embedder's
/// `model_id`/`dim` straight into [`Vault::search`]), so a client only ever sees
/// rows produced by the currently-active embedder.
async fn memory_search(
    args: &Value,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
) -> (String, bool) {
    let query_text = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let ns = args
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or(MCP_MEMORY_WRITE_NAMESPACE)
        .to_owned();
    if !MCP_MEMORY_READ_NAMESPACES.contains(&ns.as_str()) {
        return (
            format!(
                "error: namespace '{ns}' is not readable over MCP; allowed: {}",
                MCP_MEMORY_READ_NAMESPACES.join(", ")
            ),
            true,
        );
    }
    let k = usize::try_from(args.get("k").and_then(Value::as_u64).unwrap_or(5)).unwrap_or(5);
    let query_vec = embedder.embed_query(&query_text).await;
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    let vault = Arc::clone(vault);
    tokio::task::spawn_blocking(move || {
        let guard = vault.blocking_lock();
        match guard.search(&query_vec, &query_text, &ns, k, &model_id, dim) {
            Ok(entries) => {
                let results: Vec<Value> = entries
                    .into_iter()
                    .map(|e| {
                        json!({
                            "id": e.id,
                            "content": e.content,
                            "namespace": e.namespace,
                            "payload": e.payload,
                        })
                    })
                    .collect();
                (json!({ "results": results }).to_string(), false)
            }
            Err(e) => (format!("error: memory_search failed: {e}"), true),
        }
    })
    .await
    .unwrap_or_else(|e| (format!("error: memory_search task panicked: {e}"), true))
}

/// `memory_write` — bounded write into shared memory.
///
/// A `block_id` routes to a concurrency-safe append on the live shared block;
/// otherwise the content is stored into the single bounded
/// [`MCP_MEMORY_WRITE_NAMESPACE`] drawer. Either way the write can never reach an
/// internal namespace.
async fn memory_write(
    args: &Value,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
) -> (String, bool) {
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if content.is_empty() {
        return (
            "error: memory_write requires non-empty 'content'".to_owned(),
            true,
        );
    }
    let author = args
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("mcp-client")
        .to_owned();
    let embedding = embedder.embed_query(&content).await;
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    let vault = Arc::clone(vault);

    // Block-scoped append (live shared block) vs. bounded free-form drawer write.
    if let Some(block_id) = args.get("block_id").and_then(Value::as_str) {
        let block_id = block_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut guard = vault.blocking_lock();
            match guard.block_append(&block_id, &author, &content, embedding, &model_id, dim) {
                Ok(seg) => (
                    json!({ "id": seg.id, "block_id": seg.block_id, "kind": "append" }).to_string(),
                    false,
                ),
                Err(e) => (format!("error: memory_write (block) failed: {e}"), true),
            }
        })
        .await
        .unwrap_or_else(|e| (format!("error: memory_write task panicked: {e}"), true))
    } else {
        let entry_id = args
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| Uuid::new_v4().to_string(), ToOwned::to_owned);
        let payload = args.get("payload").cloned().unwrap_or_else(|| json!({}));
        tokio::task::spawn_blocking(move || {
            let entry = VaultEntry {
                id: entry_id,
                embedding,
                payload,
                namespace: MCP_MEMORY_WRITE_NAMESPACE.to_owned(),
                content,
                source_file: None,
                added_by: Some(author),
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
                embedder_model_id: model_id,
                dim,
            };
            let mut guard = vault.blocking_lock();
            match guard.upsert(&entry) {
                Ok(()) => (
                    json!({ "id": entry.id, "namespace": MCP_MEMORY_WRITE_NAMESPACE, "stored": true })
                        .to_string(),
                    false,
                ),
                Err(e) => (format!("error: memory_write failed: {e}"), true),
            }
        })
        .await
        .unwrap_or_else(|e| (format!("error: memory_write task panicked: {e}"), true))
    }
}

/// `memory_list` — enumerate shared memory (read-only).
///
/// With a `block_id` it returns the full segment log of one shared block;
/// otherwise it reports per-namespace entry counts across the read-safe
/// namespaces so a client can see where shared context lives.
async fn memory_list(
    args: &Value,
    vault: &Arc<Mutex<Vault>>,
    _embedder: &Arc<dyn Embedder>,
) -> (String, bool) {
    let vault = Arc::clone(vault);
    if let Some(block_id) = args.get("block_id").and_then(Value::as_str) {
        let block_id = block_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = vault.blocking_lock();
            match guard.block_read(&block_id) {
                Ok(segs) => {
                    let segments: Vec<Value> = segs
                        .into_iter()
                        .map(|s| {
                            json!({
                                "id": s.id,
                                "author": s.author,
                                "content": s.content,
                                "kind": s.kind.as_str(),
                                "created_at": s.created_at,
                            })
                        })
                        .collect();
                    (
                        json!({ "block_id": block_id, "segments": segments }).to_string(),
                        false,
                    )
                }
                Err(e) => (format!("error: memory_list (block) failed: {e}"), true),
            }
        })
        .await
        .unwrap_or_else(|e| (format!("error: memory_list task panicked: {e}"), true))
    } else {
        tokio::task::spawn_blocking(move || {
            let guard = vault.blocking_lock();
            let mut namespaces = Vec::new();
            for ns in MCP_MEMORY_READ_NAMESPACES {
                let count = guard.count_by_namespace(ns).unwrap_or(0);
                namespaces.push(json!({ "namespace": ns, "count": count }));
            }
            let block_count = guard
                .count_by_namespace(SHARED_BLOCK_NAMESPACE)
                .unwrap_or(0);
            namespaces.push(json!({ "namespace": SHARED_BLOCK_NAMESPACE, "count": block_count }));
            (json!({ "namespaces": namespaces }).to_string(), false)
        })
        .await
        .unwrap_or_else(|e| (format!("error: memory_list task panicked: {e}"), true))
    }
}

/// Builds an ephemeral read-only (`review`-mode) session.
///
/// Routing `tools/call` through this session means the executor's `WRITE_TOOLS`
/// guard rejects any mutating tool, so server mode can never mutate the
/// workspace even if a write tool is requested.
fn review_session() -> Session {
    let now = Timestamp::from_micros(0);
    Session {
        id: Uuid::new_v4(),
        created_at: now,
        updated_at: now,
        status: "active".to_owned(),
        task_id: None,
        mode: Some("review".to_owned()),
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    }
}

/// Handles a single MCP JSON-RPC request and returns the response.
///
/// Supports `initialize`, `tools/list`, and `tools/call`. `tools/call` for a
/// tool absent from [`MCP_SERVER_TOOLS`] returns a method-not-found error
/// without dispatching. Recognised tools run under an effective read-only
/// session.
pub(crate) async fn handle_request(
    request: &Request,
    workspace: &std::path::Path,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
) -> Response {
    let id = request.id.clone();
    match request.method.as_str() {
        "initialize" => Response::ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "smedja", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": {}, "prompts": {}, "resources": {} }
            }),
        ),
        "tools/list" => Response::ok(id, tools_list_result()),
        "tools/call" => {
            handle_tools_call(id, &request.params, workspace, ingot, vault, embedder).await
        }
        "prompts/list" => Response::ok(id, prompts_list_result(workspace)),
        "prompts/get" => match prompts_get_result(workspace, &request.params) {
            Ok(result) => Response::ok(id, result),
            Err(msg) => Response::err(id, RpcError::new(codes::INVALID_PARAMS, msg)),
        },
        "resources/list" => Response::ok(id, resources_list_result(workspace)),
        other => Response::err(
            id,
            RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            ),
        ),
    }
}

/// Dispatches a `tools/call` request into the native executor under a read-only
/// session, shaping the output as MCP `result.content`.
async fn handle_tools_call(
    id: Option<Value>,
    params: &Value,
    workspace: &std::path::Path,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
) -> Response {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Response::err(
            id,
            RpcError::new(codes::INVALID_PARAMS, "missing tool name"),
        );
    };

    // Cross-client shared-memory tools are handled natively here (not via
    // `execute_tool`), so they must be dispatched before the MCP_SERVER_TOOLS
    // gate below rejects everything outside that executor subset.
    if MCP_MEMORY_TOOLS.contains(&name) {
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let (text, is_error) = handle_memory_tool(name, &arguments, vault, embedder).await;
        return Response::ok(
            id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error,
            }),
        );
    }

    if !MCP_SERVER_TOOLS.contains(&name) {
        return Response::err(
            id,
            RpcError::new(codes::METHOD_NOT_FOUND, format!("tool not found: {name}")),
        );
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let input = arguments.to_string();
    let session = review_session();
    let output = execute_tool(
        name,
        &input,
        workspace,
        Some(&session),
        ingot,
        vault,
        embedder,
        None,
    )
    .await;

    // A native tool surfaces failures as an `error:`-prefixed string. Map those
    // to an MCP error result so clients see `isError: true` rather than a 200
    // carrying an error body — the write-tool guard lands here too.
    let is_error = output.starts_with("error:")
        || output.starts_with("permission denied")
        || output.contains("TOOL_BLOCKED");

    Response::ok(
        id,
        json!({
            "content": [{ "type": "text", "text": output }],
            "isError": is_error,
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_rpc::Request;
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    use super::{handle_request, MCP_MEMORY_TOOLS, MCP_SERVER_TOOLS};

    fn deps() -> (IngotHandle, Arc<Mutex<Vault>>) {
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        (ingot, vault)
    }

    fn embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    #[tokio::test]
    async fn tools_list_advertises_the_read_safe_subset_with_schemas() {
        let (ingot, vault) = deps();
        let req = Request::new(1, "tools/list", json!({}));
        let resp = handle_request(
            &req,
            std::path::Path::new("/tmp"),
            &ingot,
            &vault,
            &embedder(),
        )
        .await;

        assert!(resp.error.is_none(), "tools/list must succeed");
        let result = resp.result.expect("result present");
        let tools = result["tools"].as_array().expect("tools array");

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // The executor read-safe subset must all be advertised …
        for want in MCP_SERVER_TOOLS {
            assert!(
                names.contains(want),
                "tools/list must advertise MCP_SERVER_TOOL '{want}'"
            );
        }
        // … plus the additive cross-client memory tools.
        for want in MCP_MEMORY_TOOLS {
            assert!(
                names.contains(want),
                "tools/list must advertise memory tool '{want}'"
            );
        }
        // The advertised set is exactly those two additive groups — nothing else.
        assert_eq!(
            names.len(),
            MCP_SERVER_TOOLS.len() + MCP_MEMORY_TOOLS.len(),
            "tools/list must advertise only the executor subset plus memory tools"
        );

        for tool in tools {
            assert!(
                tool.get("inputSchema").is_some(),
                "each tool must advertise an inputSchema; got: {tool}"
            );
        }
    }

    // ── cross-client memory surface ───────────────────────────────────────────

    #[tokio::test]
    async fn memory_write_then_search_round_trips_through_the_vault() {
        let (ingot, vault) = deps();
        // Write into the bounded shared drawer over MCP.
        let write = Request::new(
            30,
            "tools/call",
            json!({
                "name": "memory_write",
                "arguments": { "content": "the deploy key rotates every 90 days", "id": "rot" }
            }),
        );
        let resp = handle_request(
            &write,
            std::path::Path::new("/tmp"),
            &ingot,
            &vault,
            &embedder(),
        )
        .await;
        assert!(resp.error.is_none());
        assert_eq!(resp.result.as_ref().unwrap()["isError"], false);

        // Search it back through the same MCP surface.
        let search = Request::new(
            31,
            "tools/call",
            json!({
                "name": "memory_search",
                "arguments": { "query": "deploy key rotation", "namespace": "mcp_shared" }
            }),
        );
        let resp = handle_request(
            &search,
            std::path::Path::new("/tmp"),
            &ingot,
            &vault,
            &embedder(),
        )
        .await;
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            text.contains("the deploy key rotates every 90 days"),
            "memory_search must return the memory_write content; got: {text}"
        );
    }

    #[tokio::test]
    async fn memory_write_rejects_arbitrary_namespace_via_read_allowlist() {
        // memory_search must refuse a namespace outside the read allow-list.
        let (ingot, vault) = deps();
        let req = Request::new(
            32,
            "tools/call",
            json!({
                "name": "memory_search",
                "arguments": { "query": "x", "namespace": "compact_internal_secret" }
            }),
        );
        let resp = handle_request(
            &req,
            std::path::Path::new("/tmp"),
            &ingot,
            &vault,
            &embedder(),
        )
        .await;
        let result = resp.result.expect("result present");
        assert_eq!(
            result["isError"], true,
            "unlisted namespace must be rejected"
        );
    }

    #[tokio::test]
    async fn two_clients_append_one_shared_block_and_both_see_each_other() {
        let (ingot, vault) = deps();
        let ws = std::path::Path::new("/tmp");
        let append = |seq: u64, author: &str, body: &str| {
            Request::new(
                seq,
                "tools/call",
                json!({
                    "name": "memory_write",
                    "arguments": {
                        "block_id": "fan-xyz",
                        "author": author,
                        "content": body
                    }
                }),
            )
        };

        let a = handle_request(
            &append(40, "cursor", "cursor: found the leak"),
            ws,
            &ingot,
            &vault,
            &embedder(),
        )
        .await;
        let b = handle_request(
            &append(41, "desktop", "desktop: wrote the patch"),
            ws,
            &ingot,
            &vault,
            &embedder(),
        )
        .await;
        assert_eq!(a.result.unwrap()["isError"], false);
        assert_eq!(b.result.unwrap()["isError"], false);

        // A third client lists the block and must see BOTH appends.
        let list = Request::new(
            42,
            "tools/call",
            json!({ "name": "memory_list", "arguments": { "block_id": "fan-xyz" } }),
        );
        let resp = handle_request(&list, ws, &ingot, &vault, &embedder()).await;
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            text.contains("cursor: found the leak"),
            "must see cursor's append; got: {text}"
        );
        assert!(
            text.contains("desktop: wrote the patch"),
            "must see desktop's append; got: {text}"
        );
    }

    #[tokio::test]
    async fn memory_search_holds_the_same_model_filter() {
        // A row tagged with a foreign model/dim must never surface for a query
        // embedded by the active (FNV) embedder — the vault same-model filter.
        let (ingot, vault) = deps();
        {
            let mut guard = vault.lock().await;
            let foreign = smedja_vault::VaultEntry {
                id: "foreign".to_owned(),
                embedding: vec![0.5_f32; 4],
                payload: json!({}),
                namespace: "mcp_shared".to_owned(),
                content: "foreign-model secret".to_owned(),
                source_file: None,
                added_by: None,
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
                embedder_model_id: "some-other-model".to_owned(),
                dim: 4,
            };
            guard.upsert(&foreign).unwrap();
        }
        let req = Request::new(
            43,
            "tools/call",
            json!({
                "name": "memory_search",
                "arguments": { "query": "foreign-model secret", "namespace": "mcp_shared" }
            }),
        );
        let resp = handle_request(
            &req,
            std::path::Path::new("/tmp"),
            &ingot,
            &vault,
            &embedder(),
        )
        .await;
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            !text.contains("foreign-model secret"),
            "same-model filter must exclude the foreign-model row; got: {text}"
        );
    }

    #[tokio::test]
    async fn tools_call_read_file_returns_content() {
        let (ingot, vault) = deps();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("hello.txt"), "stream-content").unwrap();

        let req = Request::new(
            7,
            "tools/call",
            json!({ "name": "read_file", "arguments": { "path": "hello.txt" } }),
        );
        let resp = handle_request(&req, ws.path(), &ingot, &vault, &embedder()).await;

        assert!(resp.error.is_none(), "tools/call must succeed");
        let result = resp.result.expect("result present");
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "stream-content");
    }

    #[tokio::test]
    async fn tools_call_write_tool_is_blocked() {
        let (ingot, vault) = deps();
        let ws = tempfile::tempdir().unwrap();
        let target = ws.path().join("should_not_exist.txt");

        let req = Request::new(
            9,
            "tools/call",
            json!({
                "name": "write_file",
                "arguments": { "path": "should_not_exist.txt", "content": "nope" }
            }),
        );
        let resp = handle_request(&req, ws.path(), &ingot, &vault, &embedder()).await;

        // write_file is absent from MCP_SERVER_TOOLS → method-not-found error,
        // and the read-only guard would also reject it had the subset drifted.
        assert!(
            resp.error.is_some(),
            "write tool must be rejected; got: {resp:?}"
        );
        assert!(
            !target.exists(),
            "no workspace mutation may occur for a blocked write tool"
        );
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_not_found() {
        let (ingot, vault) = deps();
        let req = Request::new(
            11,
            "tools/call",
            json!({ "name": "definitely_not_a_tool", "arguments": {} }),
        );
        let resp = handle_request(
            &req,
            std::path::Path::new("/tmp"),
            &ingot,
            &vault,
            &embedder(),
        )
        .await;

        let err = resp.error.expect("unknown tool must error");
        assert_eq!(err.code, smedja_rpc::codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn write_tool_present_in_subset_would_still_be_guard_blocked() {
        // Defence in depth: even if a write tool were in the subset, the
        // review-mode session must block it. We assert the guard string shape
        // by calling execute_tool directly through the same review session.
        let (ingot, vault) = deps();
        let ws = tempfile::tempdir().unwrap();
        let session = super::review_session();
        let out = crate::executor::execute_tool(
            "write_file",
            &json!({ "path": "x.txt", "content": "data" }).to_string(),
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
            &embedder(),
            None,
        )
        .await;
        assert!(
            out.contains("TOOL_BLOCKED"),
            "review-mode session must block write_file; got: {out}"
        );
        assert!(
            !ws.path().join("x.txt").exists(),
            "guard-blocked write must not mutate the workspace"
        );
    }

    /// Writes a minimal one-folder bundle into `ws/.smedja` for the MCP prompt
    /// tests: one skill with a supporting file, one rule, and one agent.
    fn seed_bundle(ws: &std::path::Path) {
        let smedja = ws.join(".smedja");
        let skill_dir = smedja.join("skills/postgres-patterns");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: postgres-patterns\ndescription: Parameterised query patterns.\nmetadata:\n  supporting_files:\n    - helpers/schema.sql\n---\nUse $1 placeholders.\n",
        )
        .unwrap();
        std::fs::create_dir_all(smedja.join("rules")).unwrap();
        std::fs::write(
            smedja.join("rules/no-unwrap.md"),
            "---\nname: no-unwrap\ndescription: No unwrap in library code.\n---\nrule body\n",
        )
        .unwrap();
        std::fs::create_dir_all(smedja.join("agents")).unwrap();
        std::fs::write(
            smedja.join("agents/reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews diffs.\ntools: read_file\n---\nagent body\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn prompts_list_returns_bundle_skills_and_rules_not_agents() {
        let (ingot, vault) = deps();
        let ws = tempfile::tempdir().unwrap();
        seed_bundle(ws.path());

        let req = Request::new(20, "prompts/list", json!({}));
        let resp = handle_request(&req, ws.path(), &ingot, &vault, &embedder()).await;

        assert!(resp.error.is_none(), "prompts/list must succeed");
        let result = resp.result.expect("result present");
        let names: Vec<&str> = result["prompts"]
            .as_array()
            .expect("prompts array")
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"postgres-patterns"), "skill listed");
        assert!(names.contains(&"no-unwrap"), "rule listed");
        assert!(!names.contains(&"reviewer"), "agent not listed as a prompt");
    }

    #[tokio::test]
    async fn prompts_get_returns_skill_body() {
        let (ingot, vault) = deps();
        let ws = tempfile::tempdir().unwrap();
        seed_bundle(ws.path());

        let req = Request::new(21, "prompts/get", json!({ "name": "postgres-patterns" }));
        let resp = handle_request(&req, ws.path(), &ingot, &vault, &embedder()).await;

        assert!(resp.error.is_none(), "prompts/get must succeed");
        let result = resp.result.expect("result present");
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(
            text.contains("$1 placeholders"),
            "skill body returned; got: {text}"
        );
    }

    #[tokio::test]
    async fn prompts_get_unknown_name_is_invalid_params() {
        let (ingot, vault) = deps();
        let ws = tempfile::tempdir().unwrap();
        seed_bundle(ws.path());

        let req = Request::new(22, "prompts/get", json!({ "name": "does-not-exist" }));
        let resp = handle_request(&req, ws.path(), &ingot, &vault, &embedder()).await;
        let err = resp.error.expect("unknown prompt must error");
        assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn resources_list_exposes_supporting_files() {
        let (ingot, vault) = deps();
        let ws = tempfile::tempdir().unwrap();
        seed_bundle(ws.path());

        let req = Request::new(23, "resources/list", json!({}));
        let resp = handle_request(&req, ws.path(), &ingot, &vault, &embedder()).await;

        assert!(resp.error.is_none(), "resources/list must succeed");
        let result = resp.result.expect("result present");
        let resources = result["resources"].as_array().expect("resources array");
        assert!(
            resources
                .iter()
                .any(|r| r["name"].as_str() == Some("postgres-patterns/helpers/schema.sql")),
            "supporting file exposed as a resource; got: {resources:?}"
        );
    }

    #[tokio::test]
    async fn initialize_advertises_prompts_and_resources_capabilities() {
        let (ingot, vault) = deps();
        let req = Request::new(24, "initialize", json!({}));
        let resp = handle_request(
            &req,
            std::path::Path::new("/tmp"),
            &ingot,
            &vault,
            &embedder(),
        )
        .await;
        let caps = &resp.result.expect("result")["capabilities"];
        assert!(
            caps.get("prompts").is_some(),
            "prompts capability advertised"
        );
        assert!(
            caps.get("resources").is_some(),
            "resources capability advertised"
        );
    }

    #[test]
    fn unsupported_method_is_method_not_found() {
        let req = Request::new(1, "completion/complete", Value::Null);
        // Build minimal deps synchronously is awkward; assert via the matcher in
        // handle_request through a runtime.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (ingot, vault) = deps();
            let resp = handle_request(
                &req,
                std::path::Path::new("/tmp"),
                &ingot,
                &vault,
                &embedder(),
            )
            .await;
            let err = resp.error.expect("unsupported method must error");
            assert_eq!(err.code, smedja_rpc::codes::METHOD_NOT_FOUND);
        });
    }
}
