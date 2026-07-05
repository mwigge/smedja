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
use smedja_vault::Vault;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::executor::{execute_tool, MCP_SERVER_TOOLS};

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

/// Builds the `tools/list` result advertising the read-safe subset.
fn tools_list_result() -> Value {
    let tools: Vec<Value> = MCP_SERVER_TOOLS
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "description": format!("smedja native tool: {name}"),
                "inputSchema": input_schema(name),
            })
        })
        .collect();
    json!({ "tools": tools })
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

    use super::{handle_request, MCP_SERVER_TOOLS};

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

        let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        names.sort_unstable();
        let mut want = MCP_SERVER_TOOLS.to_vec();
        want.sort_unstable();
        assert_eq!(names, want, "tools/list must enumerate MCP_SERVER_TOOLS");

        for tool in tools {
            assert!(
                tool.get("inputSchema").is_some(),
                "each tool must advertise an inputSchema; got: {tool}"
            );
        }
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
