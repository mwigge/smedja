//! Executor unit tests, moved verbatim from `mod.rs`. `super::*` resolves to
//! the executor module, whose (test-gated) re-exports expose the private
//! helpers these tests drive.

use std::sync::Arc;

/// Shared mutex serialising env-var mutations for fetch_web network-policy tests.
static NETWORK_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Default FNV embedder for tests that drive `execute_tool`.
fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
    Arc::new(crate::embedder_port::FnvEmbedder::new())
}

// ── output-filters: filter_command_output ─────────────────────────────────

fn filter_session() -> smedja_ingot::Session {
    smedja_ingot::Session {
        id: uuid::Uuid::new_v4(),
        created_at: smedja_types::Timestamp::from_micros(0),
        updated_at: smedja_types::Timestamp::from_micros(0),
        status: "active".to_owned(),
        task_id: None,
        mode: None,
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    }
}

#[tokio::test]
async fn known_command_output_is_compressed_on_return_path() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let session = filter_session();

    // cargo-build-like noise: many progress lines, one real error.
    let raw = format!(
        "{}error[E0308]: mismatched types\n  --> src/lib.rs:1:1\n",
        "   Compiling crate v0.1.0\n".repeat(40)
    );
    let out = super::filter_command_output(
        "cargo build",
        raw.clone(),
        ws.path(),
        Some(&session),
        &ingot,
        &vault,
    )
    .await;

    assert!(
        out.contains("error[E0308]"),
        "the real error must survive filtering; got:\n{out}"
    );
    assert!(
        !out.contains("Compiling"),
        "progress noise must be filtered; got:\n{out}"
    );
    assert!(
        out.len() < raw.len(),
        "filtered output must be shorter than the original"
    );
}

#[tokio::test]
async fn filtered_command_writes_filter_tagged_row() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let session = filter_session();
    let session_id = session.id.to_string();

    let raw = format!(
        "{}error[E0308]: mismatched types\n",
        "   Compiling crate v0.1.0\n".repeat(40)
    );
    let _ = super::filter_command_output(
        "cargo build",
        raw,
        ws.path(),
        Some(&session),
        &ingot,
        &vault,
    )
    .await;

    let by_source = ingot
        .session_tokens_saved_by_source(&session_id)
        .await
        .unwrap();
    assert_eq!(by_source.len(), 1, "exactly one source recorded");
    assert_eq!(by_source[0].0, "filter", "filter path tags source=filter");
    assert!(by_source[0].1 > 0, "a positive saving must be recorded");
}

#[tokio::test]
async fn crusher_json_path_writes_crusher_tagged_row() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let session = filter_session();
    let session_id = session.id.to_string();

    // A JSON result with null fields routes through SmartCrusher, which strips
    // them, yielding a positive estimated saving tagged source=crusher.
    let raw = serde_json::json!({
        "a": 1, "b": null, "c": null, "d": null, "e": null,
        "nested": { "x": null, "y": null, "z": "keep" }
    })
    .to_string();
    let _ =
        super::filter_command_output("some_tool", raw, ws.path(), Some(&session), &ingot, &vault)
            .await;

    let by_source = ingot
        .session_tokens_saved_by_source(&session_id)
        .await
        .unwrap();
    assert_eq!(by_source.len(), 1, "exactly one source recorded");
    assert_eq!(by_source[0].0, "crusher", "JSON path tags source=crusher");
}

#[tokio::test]
async fn unknown_command_output_passes_through_blank_removal_only() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();

    // An unknown command with no blank lines is returned verbatim (ratio 1.0),
    // preserving the captured success/failure contract.
    let raw = "alpha\nbeta\ngamma".to_owned();
    let out = super::filter_command_output(
        "some-unknown-tool --flag",
        raw.clone(),
        ws.path(),
        None,
        &ingot,
        &vault,
    )
    .await;
    assert_eq!(out, raw, "unchanged content must pass through verbatim");
}

#[tokio::test]
async fn error_prefix_contract_is_preserved_through_filtering() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();

    // A failed command's `error:` prefix (the success/failure contract) must
    // never be stripped by filtering.
    let raw = "error: command failed with exit status 1".to_owned();
    let out = super::filter_command_output(
        "some-unknown-tool",
        raw.clone(),
        ws.path(),
        None,
        &ingot,
        &vault,
    )
    .await;
    assert!(
        out.starts_with("error:"),
        "the error contract must be preserved; got: {out}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // test-only: serialises a process-global env var across the await
async fn bypass_env_skips_executor_filtering() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    // Serialise env-var mutation so concurrent tests do not race on the
    // process-global SMEDJA_NO_TOOL_COMPRESS bypass.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _env_guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();

    std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
    let raw = format!(
        "{}error[E0308]: mismatched\n",
        "   Compiling crate v0.1.0\n".repeat(40)
    );
    let out =
        super::filter_command_output("cargo build", raw.clone(), ws.path(), None, &ingot, &vault)
            .await;
    std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
    assert_eq!(out, raw, "bypass must return the output verbatim");
}

// ── output-filters: tee-to-vault recovery ─────────────────────────────────

#[tokio::test]
async fn reduced_output_is_teed_and_recoverable_via_marker_hash() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let session = filter_session();

    let raw = format!(
        "{}error[E0001]: boom\n",
        "   Compiling crate v0.1.0\n".repeat(40)
    );
    let out = super::filter_command_output(
        "cargo build",
        raw.clone(),
        ws.path(),
        Some(&session),
        &ingot,
        &vault,
    )
    .await;

    // The compressed result carries a recovery marker naming the hash.
    let hash = super::content_hash(&raw);
    assert!(
        out.contains(&format!("smedja_retrieve hash={hash}")),
        "the recovery marker must name the content hash; got:\n{out}"
    );

    // The full output is recoverable via smedja_retrieve (the in-memory store).
    let recovered = super::execute_tool(
        "smedja_retrieve",
        &format!("{{\"hash\":\"{hash}\"}}"),
        ws.path(),
        Some(&session),
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert_eq!(
        recovered, raw,
        "smedja_retrieve must return the full uncompressed output"
    );

    // And the full output is teed to the vault recovery namespace.
    let count = {
        let guard = vault.lock().await;
        guard
            .count_by_namespace(super::FILTER_RECOVERY_NAMESPACE)
            .unwrap()
    };
    assert_eq!(count, 1, "full output must be teed to the vault");
}

// ── output-filters: savings accounting ────────────────────────────────────

#[tokio::test]
async fn filtering_records_positive_tokens_saved() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let session = filter_session();
    let sid = session.id.to_string();

    let raw = format!(
        "{}error[E0001]: boom\n",
        "   Compiling crate v0.1.0\n".repeat(40)
    );
    let _ = super::filter_command_output(
        "cargo build",
        raw,
        ws.path(),
        Some(&session),
        &ingot,
        &vault,
    )
    .await;

    let saved = ingot.session_tokens_saved(&sid).await.unwrap();
    assert!(
        saved > 0,
        "a filtered command must contribute a positive tokens-saved figure; got {saved}"
    );
}

#[tokio::test]
async fn execute_tool_bash_path_runs_filter_and_preserves_output() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();

    // An unknown command emitting distinct non-blank lines passes through
    // unchanged (ratio 1.0), confirming the wiring does not corrupt output.
    let out = super::execute_tool(
        "bash",
        r#"{"command":"printf 'one\ntwo\nthree\n'"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        out.contains("one"),
        "command output must survive; got: {out}"
    );
    assert!(
        out.contains("three"),
        "command output must survive; got: {out}"
    );
}

// ── MCP_SERVER_TOOLS subset ───────────────────────────────────────────────

#[test]
fn mcp_server_tools_are_exactly_the_read_safe_subset() {
    // No write/exec tool may appear in the server subset.
    const FORBIDDEN: &[&str] = &[
        "edit_file",
        "bash",
        "write_file",
        "run_command",
        "move_file",
        "copy_file",
        "delete_file",
        "smedja_vault_store",
    ];
    let expected = [
        "graph_query",
        "grep_files",
        "find_files",
        "read_file",
        "list_files",
        "load_skill",
        "smedja_vault_search",
        "smedja_retrieve",
        "otel_query",
        "metric_query",
        "log_tail",
    ];
    // The exposed subset must match the read-safe list exactly.
    let mut got = super::MCP_SERVER_TOOLS.to_vec();
    let mut want = expected.to_vec();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want, "MCP_SERVER_TOOLS must be the read-safe subset");

    for tool in FORBIDDEN {
        assert!(
            !super::MCP_SERVER_TOOLS.contains(tool),
            "MCP_SERVER_TOOLS must not expose write/exec tool '{tool}'"
        );
    }
}

#[test]
fn mcp_server_tools_is_subset_of_local_tools() {
    for tool in super::MCP_SERVER_TOOLS {
        assert!(
            super::LOCAL_TOOLS.contains(tool),
            "MCP_SERVER_TOOLS entry '{tool}' must also be in LOCAL_TOOLS"
        );
    }
}

// ── confined root resolution (worktree-aware) ─────────────────────────────

#[test]
fn confined_root_is_worktree_when_task_owns_one() {
    // The orchestrator threads the active worktree path through as the
    // tool-execution workspace when a task owns one, else the session
    // workspace. `confined_root_for` must canonicalise that subtree.
    let session_ws = tempfile::tempdir().unwrap();
    let worktree = session_ws.path().join("worktrees").join("task-1");
    std::fs::create_dir_all(&worktree).unwrap();

    // Task owns a worktree → confined root is the worktree.
    let resolved = super::confined_root_for(&worktree);
    assert_eq!(resolved, worktree.canonicalize().unwrap());

    // No worktree → confined root is the session workspace.
    let resolved = super::confined_root_for(session_ws.path());
    assert_eq!(resolved, session_ws.path().canonicalize().unwrap());
}

// ── find_tool_call_json / parse_tool_call ─────────────────────────────────

#[test]
fn find_tool_call_json_returns_none_for_empty_string() {
    let result = super::find_tool_call_json("");
    assert!(result.is_none(), "empty string must yield None");
}

#[test]
fn find_tool_call_json_handles_json_embedded_in_text() {
    let text = r#"Here is the call: {"tool":"read_file","input":{"path":"foo.txt"}} done."#;
    let result = super::find_tool_call_json(text);
    assert!(result.is_some(), "embedded JSON must be found");
    let v = result.unwrap();
    assert_eq!(v["tool"], "read_file");
    assert_eq!(v["input"]["path"], "foo.txt");
}

#[test]
fn parse_tool_call_returns_none_for_plain_text() {
    let result = super::parse_tool_call("hello world, no JSON here");
    assert!(result.is_none(), "plain text must yield None");
}

#[test]
fn parse_tool_call_returns_some_for_valid_tool_json() {
    let json = r#"{"tool":"bash","input":{"command":"ls"}}"#;
    let result = super::parse_tool_call(json);
    assert!(result.is_some(), "valid tool JSON must yield Some");
    let (tool_name, input_str) = result.unwrap();
    assert_eq!(tool_name, "bash");
    let input_val: serde_json::Value = serde_json::from_str(&input_str).unwrap();
    assert_eq!(input_val["command"], "ls");
}

#[test]
fn parse_tool_call_returns_none_for_json_without_tool_key() {
    let json = r#"{"action":"bash","input":{"command":"ls"}}"#;
    let result = super::parse_tool_call(json);
    assert!(result.is_none(), "JSON without 'tool' key must yield None");
}

// ── parse_all_tool_calls ──────────────────────────────────────────────────

#[test]
fn parse_all_tool_calls_returns_empty_for_plain_text() {
    assert!(super::parse_all_tool_calls("hello world").is_empty());
}

#[test]
fn parse_all_tool_calls_returns_single_call() {
    let text = r#"{"tool":"read_file","input":{"path":"foo.txt"}}"#;
    let calls = super::parse_all_tool_calls(text);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "read_file");
}

#[test]
fn parse_all_tool_calls_returns_multiple_calls_in_order() {
    let text = concat!(
        r#"{"tool":"read_file","input":{"path":"a.txt"}} "#,
        r#"{"tool":"grep_files","input":{"pattern":"foo"}} "#,
        r#"{"tool":"list_files","input":{}}"#,
    );
    let calls = super::parse_all_tool_calls(text);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, "read_file");
    assert_eq!(calls[1].0, "grep_files");
    assert_eq!(calls[2].0, "list_files");
}

#[test]
fn parse_all_tool_calls_skips_past_consumed_json() {
    // Nested JSON in `input` must not produce a spurious extra call.
    let text = r#"{"tool":"write_file","input":{"path":"f","content":"{\"nested\":true}"}}"#;
    let calls = super::parse_all_tool_calls(text);
    assert_eq!(calls.len(), 1, "nested JSON must not produce extra calls");
    assert_eq!(calls[0].0, "write_file");
}

#[test]
fn parse_all_tool_calls_embedded_in_prose() {
    let text = concat!(
        "Here are two reads:\n",
        r#"{"tool":"read_file","input":{"path":"a.txt"}}"#,
        " and ",
        r#"{"tool":"read_file","input":{"path":"b.txt"}}"#,
        "\nDone.",
    );
    let calls = super::parse_all_tool_calls(text);
    assert_eq!(calls.len(), 2);
}

// ── path traversal guard ──────────────────────────────────────────────────

#[tokio::test]
async fn execute_tool_bash_returns_error_for_path_outside_workspace() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    // write_file with a path that tries to escape the workspace via ../
    let result = super::execute_tool(
        "write_file",
        r#"{"path":"../../etc/passwd","content":"injected"}"#,
        std::path::Path::new("/tmp"),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;

    assert!(
        result.contains("path outside workspace") || result.starts_with("error:"),
        "path traversal must be rejected; got: {result}"
    );
}

// ── methodology gate ──────────────────────────────────────────────────────

fn session_with_mode(mode: Option<&str>) -> smedja_ingot::Session {
    smedja_ingot::Session {
        id: uuid::Uuid::new_v4(),
        created_at: smedja_types::Timestamp::from_micros(0),
        updated_at: smedja_types::Timestamp::from_micros(0),
        status: "active".to_owned(),
        task_id: None,
        mode: mode.map(str::to_owned),
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    }
}

async fn run_write(
    ingot: &smedja_ingot::IngotHandle,
    session: &smedja_ingot::Session,
    workspace: &std::path::Path,
    content: &str,
) -> String {
    use smedja_vault::Vault;
    use tokio::sync::Mutex;
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let input = serde_json::json!({ "path": "f.rs", "content": content }).to_string();
    super::execute_tool(
        "write_file",
        &input,
        workspace,
        Some(session),
        ingot,
        &vault,
        &test_embedder(),
    )
    .await
}

#[tokio::test]
async fn clean_mode_blocks_unwrap_write() {
    let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let session = session_with_mode(Some("clean"));
    let result = run_write(&ingot, &session, &ws, "fn f() {\n    x.unwrap()\n}\n").await;
    assert!(result.contains("METHODOLOGY_BLOCKED"), "got: {result}");
    assert!(result.contains("CleanGate"), "got: {result}");
}

#[tokio::test]
async fn clean_mode_allows_conforming_write() {
    let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let session = session_with_mode(Some("clean"));
    // Clean content passes the gate; it then falls through to MCP dispatch.
    let result = run_write(&ingot, &session, &ws, "fn f() -> u32 {\n    1\n}\n").await;
    assert!(
        !result.contains("METHODOLOGY_BLOCKED"),
        "conforming write must pass the gate; got: {result}"
    );
}

#[tokio::test]
async fn unmoded_session_is_ungated() {
    let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let session = session_with_mode(None);
    let result = run_write(&ingot, &session, &ws, "fn f() {\n    x.unwrap()\n}\n").await;
    assert!(
        !result.contains("METHODOLOGY_BLOCKED"),
        "a session with no mode must not be gated; got: {result}"
    );
}

#[tokio::test]
async fn no_spec_gate_escape_hatch_bypasses_gates() {
    let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let session = session_with_mode(Some("clean"));
    ingot
        .set_no_spec_gate(&session.id.to_string(), true)
        .await
        .unwrap();
    let result = run_write(&ingot, &session, &ws, "fn f() {\n    x.unwrap()\n}\n").await;
    assert!(
        !result.contains("METHODOLOGY_BLOCKED"),
        "escape hatch must bypass the gate; got: {result}"
    );
}

#[tokio::test]
async fn spec_mode_blocks_write_until_spec_and_approval_recorded() {
    let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let session = session_with_mode(Some("spec"));

    // No spec recorded → blocked, naming the missing specification.
    let blocked = run_write(&ingot, &session, &ws, "fn f() -> u32 { 1 }\n").await;
    assert!(blocked.contains("METHODOLOGY_BLOCKED"), "got: {blocked}");
    assert!(blocked.contains("specification"), "got: {blocked}");

    // Record spec + approval → the spec-first gate releases.
    let sid = session.id.to_string();
    ingot.set_spec_recorded(&sid, true).await.unwrap();
    ingot.set_approval_recorded(&sid, true).await.unwrap();
    let released = run_write(&ingot, &session, &ws, "fn f() -> u32 { 1 }\n").await;
    assert!(
        !released.contains("METHODOLOGY_BLOCKED"),
        "spec+approval must release the spec-first gate; got: {released}"
    );
}

// ── dispatch_mcp_tool ─────────────────────────────────────────────────────

#[tokio::test]
async fn dispatch_mcp_tool_returns_error_when_no_server_registered() {
    let ig = smedja_ingot::IngotHandle::new(
        smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
    );
    let result = super::dispatch_mcp_tool("unknown_tool", &serde_json::json!({}), &ig).await;
    assert!(
        result.starts_with("error:"),
        "unregistered tool must return an error; got: {result}"
    );
    assert!(
        result.contains("unknown_tool"),
        "error must include the tool name; got: {result}"
    );
}

#[tokio::test]
async fn dispatch_mcp_tool_routes_to_registered_server() {
    use tokio::net::TcpListener;

    // Spawn a minimal mock MCP server that responds to tools/call.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_url = format!("http://{addr}");

    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "content": [{ "type": "text", "text": "dispatched-ok" }],
                        "isError": false
                    }
                }))
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Register the mock server in an in-memory Ingot.
    let ig = smedja_ingot::IngotHandle::new(
        smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
    );
    ig.register_mcp_server(smedja_ingot::McpServer {
        id: "mock-1".into(),
        name: "mock-server".into(),
        url: server_url,
        transport: "http".into(),
        tools_json: r#"[{"name":"greet","description":"Greet"}]"#.into(),
        last_refresh: 1.0,
    })
    .await
    .expect("register_mcp_server must succeed");

    let result =
        super::dispatch_mcp_tool("greet", &serde_json::json!({"name": "world"}), &ig).await;
    assert!(
        result.contains("dispatched-ok"),
        "must return the mock server's response; got: {result}"
    );
}

#[test]
fn resolve_mcp_token_prefers_stored_token() {
    let tmp = tempfile::tempdir().unwrap();
    let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
    let token = crate::mcp_oauth::Token {
        access_token: "stored-bearer".into(),
        token_type: "Bearer".into(),
        refresh_token: None,
        expires_in: Some(3600),
    };
    store.save("https://srv.example.com", &token).unwrap();
    let resolved = super::resolve_mcp_token(&store, "https://srv.example.com", None);
    assert_eq!(resolved, "stored-bearer");
}

#[test]
fn resolve_mcp_token_falls_back_to_env_then_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
    // No stored token, MCP_TOKEN provided → env value.
    let resolved = super::resolve_mcp_token(&store, "https://none.example.com", Some("env-bearer"));
    assert_eq!(resolved, "env-bearer");
    // No stored token, no env → empty (unauthenticated path).
    let resolved = super::resolve_mcp_token(&store, "https://none.example.com", None);
    assert_eq!(resolved, "");
}

#[tokio::test]
async fn dispatch_mcp_tool_sends_stored_token_as_bearer() {
    use std::sync::Arc;

    use tokio::net::TcpListener;
    use tokio::sync::Mutex as TokioMutex;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_url = format!("http://{addr}");

    // The mock server echoes back whether it saw the expected Bearer.
    let seen_auth: Arc<TokioMutex<Option<String>>> = Arc::new(TokioMutex::new(None));
    let seen_clone = Arc::clone(&seen_auth);
    tokio::spawn(async move {
        let app = axum::Router::new().route(
                "/",
                axum::routing::post(
                    move |headers: axum::http::HeaderMap| {
                        let seen = Arc::clone(&seen_clone);
                        async move {
                            let auth = headers
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_owned);
                            *seen.lock().await = auth;
                            axum::Json(serde_json::json!({
                                "jsonrpc": "2.0", "id": 1,
                                "result": { "content": [{ "type": "text", "text": "ok" }], "isError": false }
                            }))
                        }
                    },
                ),
            );
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Persist a token keyed by the server URL in a scoped store.
    let tmp = tempfile::tempdir().unwrap();
    let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
    store
        .save(
            &server_url,
            &crate::mcp_oauth::Token {
                access_token: "abc123".into(),
                token_type: "Bearer".into(),
                refresh_token: None,
                expires_in: Some(3600),
            },
        )
        .unwrap();

    let ig = smedja_ingot::IngotHandle::new(
        smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
    );
    ig.register_mcp_server(smedja_ingot::McpServer {
        id: "auth-1".into(),
        name: "auth-server".into(),
        url: server_url.clone(),
        transport: "http".into(),
        tools_json: r#"[{"name":"greet","description":"Greet"}]"#.into(),
        last_refresh: 1.0,
    })
    .await
    .expect("register_mcp_server must succeed");

    let result =
        super::dispatch_mcp_tool_with_store("greet", &serde_json::json!({}), &ig, &store, None)
            .await;
    let _ = result; // result body is not under test here.

    let observed = seen_auth.lock().await.clone();
    assert_eq!(
        observed.as_deref(),
        Some("Bearer abc123"),
        "stored token must be sent as the Bearer credential"
    );
}

// ── output scanning (advisory by default) ─────────────────────────────────

#[tokio::test]
async fn secret_bearing_tool_result_records_finding_and_returns_original() {
    use smedja_ingot::{Ingot, IngotHandle, Session};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();

    // A session ties the finding to a known session id for lookup.
    let session = Session {
        id: uuid::Uuid::new_v4(),
        created_at: smedja_types::Timestamp::from_micros(0),
        updated_at: smedja_types::Timestamp::from_micros(0),
        status: "active".to_owned(),
        task_id: None,
        mode: None,
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    };
    let sid = session.id.to_string();

    // Write a file whose content contains a synthetic AWS-style key, then
    // read it back through the executor so the result carries the secret.
    let secret = "AKIAIOSFODNN7EXAMPLE";
    std::fs::write(ws.path().join("creds.txt"), format!("key={secret}")).unwrap();

    let result = super::execute_tool(
        "read_file",
        r#"{"path":"creds.txt"}"#,
        ws.path(),
        Some(&session),
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;

    // Advisory default: the original content is returned unmodified.
    assert!(
        result.contains(secret),
        "advisory default must return the original content; got: {result}"
    );

    // A security_finding event is recorded with status "warn".
    let events = ingot.list_audit_events(&sid).await.unwrap();
    let finding = events
        .iter()
        .find(|e| e.action_type == "security_finding")
        .expect("a security_finding event must be recorded");
    assert_eq!(finding.status.as_deref(), Some("warn"));
    assert_eq!(finding.tool_name.as_deref(), Some("read_file"));
}

#[tokio::test]
async fn clean_tool_result_records_no_finding() {
    use smedja_ingot::{Ingot, IngotHandle, Session};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();

    let session = Session {
        id: uuid::Uuid::new_v4(),
        created_at: smedja_types::Timestamp::from_micros(0),
        updated_at: smedja_types::Timestamp::from_micros(0),
        status: "active".to_owned(),
        task_id: None,
        mode: None,
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    };
    let sid = session.id.to_string();

    std::fs::write(ws.path().join("notes.txt"), "nothing secret here").unwrap();
    let result = super::execute_tool(
        "read_file",
        r#"{"path":"notes.txt"}"#,
        ws.path(),
        Some(&session),
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert_eq!(result, "nothing secret here");

    let events = ingot.list_audit_events(&sid).await.unwrap();
    assert!(
        events.iter().all(|e| e.action_type != "security_finding"),
        "clean output must record no security_finding"
    );
}

// ── vault tools via execute_tool ──────────────────────────────────────────

#[tokio::test]
async fn vault_search_returns_empty_when_no_entries() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    let result = super::execute_tool(
        "smedja_vault_search",
        r#"{"query":"rust async"}"#,
        std::path::Path::new("/tmp"),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;

    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["results"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn vault_store_then_search_finds_entry() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    let store_result = super::execute_tool(
        "smedja_vault_store",
        r#"{"content":"tokio async runtime executor","namespace":"test"}"#,
        std::path::Path::new("/tmp"),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let stored: serde_json::Value = serde_json::from_str(&store_result).unwrap();
    assert_eq!(stored["stored"], true);

    let search_result = super::execute_tool(
        "smedja_vault_search",
        r#"{"query":"tokio async","namespace":"test","k":5}"#,
        std::path::Path::new("/tmp"),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&search_result).unwrap();
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "stored entry must be found");
    assert_eq!(results[0]["namespace"], "test");
}

#[tokio::test]
async fn vault_search_respects_k_limit() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    for i in 0..5_u8 {
        super::execute_tool(
            "smedja_vault_store",
            &format!(r#"{{"content":"rust programming language crate {i}","namespace":"ns"}}"#),
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
    }

    let result = super::execute_tool(
        "smedja_vault_search",
        r#"{"query":"rust programming","namespace":"ns","k":2}"#,
        std::path::Path::new("/tmp"),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert!(
        v["results"].as_array().unwrap().len() <= 2,
        "k=2 must cap results at 2"
    );
}

#[tokio::test]
async fn smedja_vault_search_returns_results_when_vault_has_matching_entries() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    // Insert a known entry via the store tool so the embedding path is exercised.
    let store_result = super::execute_tool(
        "smedja_vault_store",
        r#"{"content":"Rust ownership model borrow checker lifetimes","namespace":"search-test"}"#,
        std::path::Path::new("/tmp"),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let stored: serde_json::Value = serde_json::from_str(&store_result).unwrap();
    assert_eq!(
        stored["stored"], true,
        "entry must be stored before searching"
    );

    // Query with text similar to the inserted entry.
    let search_result = super::execute_tool(
        "smedja_vault_search",
        r#"{"query":"Rust ownership borrow checker","namespace":"search-test","k":5}"#,
        std::path::Path::new("/tmp"),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;

    let v: serde_json::Value = serde_json::from_str(&search_result).unwrap();
    let results = v["results"].as_array().expect("results must be an array");
    assert!(
        !results.is_empty(),
        "smedja_vault_search must return at least one result for a matching query"
    );

    // Verify the returned entry has a positive cosine similarity by checking
    // that the vault itself scores the entry > 0 when searched directly.
    let similarity = {
        let guard = vault.lock().await;
        let qv = crate::embedder::embed("Rust ownership borrow checker");
        let entries = guard
            .search(
                &qv,
                "Rust ownership borrow checker",
                "search-test",
                1,
                smedja_vault::LEGACY_MODEL_ID,
                crate::embedder::DIM,
            )
            .unwrap();
        if entries.is_empty() {
            return;
        }
        // Re-score using the embedder to confirm similarity is positive.
        let stored_vec = crate::embedder::embed("Rust ownership model borrow checker lifetimes");
        qv.iter()
            .zip(stored_vec.iter())
            .map(|(a, b)| a * b)
            .sum::<f32>()
    };
    assert!(
        similarity > 0.0,
        "cosine similarity between query and stored entry must be > 0, got {similarity}"
    );
}

// ── is_confirm_edits_enabled ──────────────────────────────────────────────

#[test]
fn confirm_edits_defaults_to_false_when_no_workspace_toml() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !super::is_confirm_edits_enabled(dir.path()),
        "missing workspace.toml must resolve to false"
    );
}

#[test]
fn confirm_edits_false_when_key_absent() {
    let dir = tempfile::tempdir().unwrap();
    let smedja = dir.path().join(".smedja");
    std::fs::create_dir_all(&smedja).unwrap();
    std::fs::write(smedja.join("workspace.toml"), "[workspace]\nname = \"x\"\n").unwrap();
    assert!(
        !super::is_confirm_edits_enabled(dir.path()),
        "missing [tools] key must resolve to false"
    );
}

#[test]
fn confirm_edits_true_when_enabled_in_workspace_toml() {
    let dir = tempfile::tempdir().unwrap();
    let smedja = dir.path().join(".smedja");
    std::fs::create_dir_all(&smedja).unwrap();
    std::fs::write(
        smedja.join("workspace.toml"),
        "[tools]\nconfirm_edits = true\n",
    )
    .unwrap();
    assert!(
        super::is_confirm_edits_enabled(dir.path()),
        "confirm_edits = true must be detected"
    );
}

#[test]
fn confirm_edits_false_when_explicitly_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let smedja = dir.path().join(".smedja");
    std::fs::create_dir_all(&smedja).unwrap();
    std::fs::write(
        smedja.join("workspace.toml"),
        "[tools]\nconfirm_edits = false\n",
    )
    .unwrap();
    assert!(
        !super::is_confirm_edits_enabled(dir.path()),
        "confirm_edits = false must resolve to false"
    );
}

// --- WI-018: read_file line ranges, list_files depth/pattern, glob_match ----

#[test]
fn glob_match_star_matches_any_suffix() {
    assert!(super::glob_match("*.rs", "main.rs"));
    assert!(!super::glob_match("*.rs", "main.toml"));
}

#[test]
fn glob_match_question_mark_matches_one_char() {
    assert!(super::glob_match("fo?", "foo"));
    assert!(!super::glob_match("fo?", "fo"));
}

#[test]
fn glob_match_star_matches_empty() {
    assert!(super::glob_match("*.rs", ".rs"));
    assert!(super::glob_match("*", "anything"));
    assert!(super::glob_match("*", ""));
}

#[test]
fn glob_match_exact() {
    assert!(super::glob_match("Cargo.toml", "Cargo.toml"));
    assert!(!super::glob_match("Cargo.toml", "cargo.toml"));
}

// read_file and list_files helpers are tested via the full execute_tool path
// in integration — here we test the glob helper and the executor's behaviour
// through a direct unit path.

#[test]
fn read_file_line_range_returns_subset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    std::fs::write(&path, "line1\nline2\nline3\nline4\n").unwrap();

    // Simulate the line-range extraction used in the read_file handler.
    let text = std::fs::read_to_string(&path).unwrap();
    let start_line: usize = 2;
    let end_line: usize = 3;
    let start = start_line.saturating_sub(1);
    let lines: Vec<&str> = text.lines().collect();
    let end = end_line.min(lines.len());
    let result = lines[start..end].join("\n");
    assert_eq!(result, "line2\nline3");
}

#[test]
fn read_file_base64_roundtrip() {
    use base64::Engine as _;
    let raw = b"\x00\x01\x02\x03binary";
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&encoded)
        .unwrap();
    assert_eq!(decoded, raw);
}

// --- WI-014: bash blocked_patterns ---

#[test]
fn bash_blocked_patterns_empty_when_no_workspace_toml() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        super::bash_config(dir.path())
            .blocked_patterns
            .unwrap_or_default()
            .is_empty(),
        "missing workspace.toml must return empty patterns"
    );
}

#[test]
fn bash_blocked_patterns_loaded_from_workspace_toml() {
    let dir = tempfile::tempdir().unwrap();
    let smedja = dir.path().join(".smedja");
    std::fs::create_dir_all(&smedja).unwrap();
    std::fs::write(
        smedja.join("workspace.toml"),
        "[tools.bash]\nblocked_patterns = [\"rm -rf /\", \"curl * | sh\"]\n",
    )
    .unwrap();
    let patterns = super::bash_config(dir.path())
        .blocked_patterns
        .unwrap_or_default();
    assert_eq!(patterns, vec!["rm -rf /", "curl * | sh"]);
}

#[tokio::test]
async fn bash_blocked_pattern_match_returns_error() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let smedja = dir.path().join(".smedja");
    std::fs::create_dir_all(&smedja).unwrap();
    std::fs::write(
        smedja.join("workspace.toml"),
        "[tools.bash]\nblocked_patterns = [\"rm -rf /\"]\n",
    )
    .unwrap();

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    let result = super::execute_tool(
        "bash",
        r#"{"command":"rm -rf / --no-preserve-root"}"#,
        dir.path(),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("blocked by policy"),
        "blocked command must return policy error, got: {result}"
    );
}

#[tokio::test]
async fn bash_non_blocked_command_not_affected() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let smedja = dir.path().join(".smedja");
    std::fs::create_dir_all(&smedja).unwrap();
    std::fs::write(
        smedja.join("workspace.toml"),
        "[tools.bash]\nblocked_patterns = [\"rm -rf /\"]\n",
    )
    .unwrap();

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    let result = super::execute_tool(
        "bash",
        r#"{"command":"echo hello"}"#,
        dir.path(),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("hello"),
        "non-blocked command must execute normally, got: {result}"
    );
}

// --- WI-019: bash timeout_secs, env map, stdin ---

#[tokio::test]
async fn bash_env_blocklisted_key_rejected() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    let result = super::execute_tool(
        "bash",
        r#"{"command":"echo hi","env":{"PATH":"/evil"}}"#,
        dir.path(),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("not allowed"),
        "blocklisted env key must return error, got: {result}"
    );
}

#[tokio::test]
async fn bash_env_injected_into_command() {
    let dir = tempfile::tempdir().unwrap();
    let env: std::collections::HashMap<String, String> =
        [("MY_VAR".into(), "smedja_test".into())].into();
    let result = crate::exec_bash_ext("echo $MY_VAR", dir.path(), None, Some(env), None).await;
    assert!(
        result.contains("smedja_test"),
        "injected env var must appear in output, got: {result}"
    );
}

#[tokio::test]
async fn bash_stdin_fed_to_command() {
    let dir = tempfile::tempdir().unwrap();
    let result = crate::exec_bash_ext(
        "cat",
        dir.path(),
        None,
        None,
        Some(b"hello from stdin".to_vec()),
    )
    .await;
    assert!(
        result.contains("hello from stdin"),
        "stdin must be forwarded to command, got: {result}"
    );
}

#[tokio::test]
async fn bash_timeout_secs_short_timeout_errors() {
    let dir = tempfile::tempdir().unwrap();
    let result = crate::exec_bash_ext("sleep 10", dir.path(), Some(1), None, None).await;
    assert!(
        result.contains("timed out"),
        "short timeout must return timeout error, got: {result}"
    );
}

// ── WI-012: stderr block, partial output on timeout ───────────────────────

#[tokio::test]
async fn bash_stderr_appended_as_block_on_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    // Write something to stdout and something to stderr, then exit 1.
    let result = crate::exec_bash_ext(
        "echo out; echo err >&2; exit 1",
        dir.path(),
        None,
        None,
        None,
    )
    .await;
    assert!(
        result.starts_with("error:"),
        "non-zero exit must start with error: prefix; got: {result}"
    );
    assert!(
        result.contains("[stderr]"),
        "stderr must appear in a [stderr] block; got: {result}"
    );
    assert!(
        result.contains("err"),
        "stderr content must be included; got: {result}"
    );
}

#[tokio::test]
async fn bash_partial_output_returned_on_timeout() {
    let dir = tempfile::tempdir().unwrap();
    // Print one line immediately, then sleep to trigger timeout.
    let result =
        crate::exec_bash_ext("echo partial; sleep 10", dir.path(), Some(1), None, None).await;
    assert!(
        result.contains("partial"),
        "output emitted before timeout must be returned; got: {result}"
    );
    assert!(
        result.contains("timed out"),
        "timeout message must be present; got: {result}"
    );
}

#[tokio::test]
async fn bash_returns_when_background_child_keeps_pipe_open() {
    let dir = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        crate::exec_bash_ext("printf done; sleep 1 &", dir.path(), Some(5), None, None),
    )
    .await
    .expect("exec_bash_ext must not wait for a background child holding stdout");
    assert!(
        result.contains("done"),
        "stdout emitted before shell exit must be preserved; got: {result}"
    );
}

// ── fetch_web ──────────────────────────────────────────────────────────────

#[test]
fn strip_html_removes_tags() {
    let result = super::strip_html("<p>Hello <b>world</b></p>");
    assert!(result.contains("Hello"), "text must be preserved: {result}");
    assert!(result.contains("world"), "text must be preserved: {result}");
    assert!(!result.contains('<'), "tags must be stripped: {result}");
}

#[test]
fn strip_html_removes_script_block() {
    let html = "<head><script>alert(1)</script></head><body>Keep</body>";
    let result = super::strip_html(html);
    assert!(
        !result.contains("alert"),
        "script content must be removed: {result}"
    );
    assert!(result.contains("Keep"), "body text must be kept: {result}");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fetch_web_policy_none_blocks() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;
    let _g = NETWORK_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("SMEDJA_SANDBOX_NETWORK");
    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let result = super::execute_tool(
        "fetch_web",
        r#"{"url":"https://example.com"}"#,
        ws.path(),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.starts_with("error:"),
        "NetworkPolicy::None must block fetch_web: {result}"
    );
    assert!(
        result.contains("network access disabled"),
        "error must name the policy reason: {result}"
    );
}

// ── WI-017: grep_files, find_files, move_file, copy_file, delete_file ────────

fn mk_ingot_vault() -> (
    smedja_ingot::IngotHandle,
    std::sync::Arc<tokio::sync::Mutex<smedja_vault::Vault>>,
) {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;
    (
        IngotHandle::new(Ingot::open_in_memory().unwrap()),
        Arc::new(Mutex::new(Vault::open_in_memory().unwrap())),
    )
}

#[tokio::test]
async fn grep_files_finds_matching_lines() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("a.txt"), "hello world\nno match here\n").unwrap();
    std::fs::write(ws.join("b.txt"), "world domination\n").unwrap();

    let result = super::execute_tool(
        "grep_files",
        r#"{"pattern":"world"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    let matches = v["matches"].as_array().unwrap();
    assert_eq!(v["count"], 2, "two lines contain 'world'; got: {result}");
    assert!(
        matches
            .iter()
            .any(|m| m["text"].as_str().unwrap().contains("hello world")),
        "hello world match must appear; got: {result}"
    );
}

#[tokio::test]
async fn grep_files_respects_max_results() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    // Write 10 matching lines across two files
    std::fs::write(ws.join("x.txt"), "match\n".repeat(6)).unwrap();
    std::fs::write(ws.join("y.txt"), "match\n".repeat(6)).unwrap();

    let result = super::execute_tool(
        "grep_files",
        r#"{"pattern":"match","max_results":3}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["count"], 3, "max_results=3 must cap at 3; got: {result}");
}

#[tokio::test]
async fn grep_files_rejects_path_outside_workspace() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();

    let result = super::execute_tool(
        "grep_files",
        r#"{"pattern":"x","path":"../../etc"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("path outside workspace") || result.starts_with("error:"),
        "traversal must be rejected; got: {result}"
    );
}

#[tokio::test]
#[allow(clippy::case_sensitive_file_extension_comparisons)]
async fn find_files_matches_glob_pattern() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn lib() {}").unwrap();
    std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();

    let result = super::execute_tool(
        "find_files",
        r#"{"pattern":"*.rs"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "two .rs files; got: {result}");
    assert!(
        files
            .iter()
            .any(|f| f.as_str().unwrap().ends_with("main.rs")),
        "main.rs must appear; got: {result}"
    );
    assert!(
        files.iter().all(|f| f.as_str().unwrap().ends_with(".rs")),
        "only .rs files; got: {result}"
    );
}

#[tokio::test]
async fn find_files_respects_max_results() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    for i in 0..10u8 {
        std::fs::write(ws.join(format!("f{i}.txt")), "").unwrap();
    }

    let result = super::execute_tool(
        "find_files",
        r#"{"pattern":"*.txt","max_results":4}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["count"], 4, "max_results=4 must cap at 4; got: {result}");
}

#[tokio::test]
async fn find_files_rejects_path_outside_workspace() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();

    let result = super::execute_tool(
        "find_files",
        r#"{"pattern":"*","path":"../../etc"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("path outside workspace") || result.starts_with("error:"),
        "traversal must be rejected; got: {result}"
    );
}

#[tokio::test]
async fn move_file_renames_within_workspace() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("old.txt"), "content").unwrap();

    let result = super::execute_tool(
        "move_file",
        r#"{"source":"old.txt","destination":"new.txt"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["moved"], true, "move must succeed; got: {result}");
    assert!(
        !ws.join("old.txt").exists(),
        "source must not exist after move"
    );
    assert!(
        ws.join("new.txt").exists(),
        "destination must exist after move"
    );
}

#[tokio::test]
async fn move_file_rejects_source_outside_workspace() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();

    let result = super::execute_tool(
        "move_file",
        r#"{"source":"../../etc/passwd","destination":"out.txt"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("path outside workspace") || result.starts_with("error:"),
        "boundary rejection must fire on source; got: {result}"
    );
}

#[tokio::test]
async fn copy_file_copies_within_workspace() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("src.txt"), "data").unwrap();

    let result = super::execute_tool(
        "copy_file",
        r#"{"source":"src.txt","destination":"dst.txt"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["copied"], true, "copy must succeed; got: {result}");
    assert!(
        ws.join("src.txt").exists(),
        "source must still exist after copy"
    );
    assert_eq!(std::fs::read_to_string(ws.join("dst.txt")).unwrap(), "data");
}

#[tokio::test]
async fn copy_file_creates_parent_dirs() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("src.txt"), "data").unwrap();

    let result = super::execute_tool(
        "copy_file",
        r#"{"source":"src.txt","destination":"sub/dir/dst.txt"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        v["copied"], true,
        "copy to new subdir must succeed; got: {result}"
    );
    assert!(ws.join("sub/dir/dst.txt").exists());
}

#[tokio::test]
async fn copy_file_rejects_destination_outside_workspace() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("src.txt"), "x").unwrap();

    let result = super::execute_tool(
        "copy_file",
        r#"{"source":"src.txt","destination":"../../tmp/evil.txt"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("path outside workspace") || result.starts_with("error:"),
        "boundary rejection must fire on destination; got: {result}"
    );
}

#[tokio::test]
async fn delete_file_removes_a_file() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("rm_me.txt"), "bye").unwrap();

    let result = super::execute_tool(
        "delete_file",
        r#"{"path":"rm_me.txt"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["deleted"], true, "delete must succeed; got: {result}");
    assert!(
        !ws.join("rm_me.txt").exists(),
        "file must be gone after delete"
    );
}

#[tokio::test]
async fn delete_file_rejects_path_outside_workspace() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();

    let result = super::execute_tool(
        "delete_file",
        r#"{"path":"../../etc/passwd"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.contains("path outside workspace") || result.starts_with("error:"),
        "boundary rejection must fire; got: {result}"
    );
}

#[tokio::test]
async fn delete_file_refuses_nonempty_directory() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let subdir = ws.join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(subdir.join("file.txt"), "keep").unwrap();

    let result = super::execute_tool(
        "delete_file",
        r#"{"path":"subdir"}"#,
        &ws,
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        result.starts_with("error:"),
        "non-empty dir delete must fail; got: {result}"
    );
    assert!(subdir.exists(), "non-empty dir must remain; got: {result}");
}

#[tokio::test]
async fn move_copy_delete_blocked_in_review_session() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("f.txt"), "x").unwrap();
    let session = session_with_mode(Some("review"));

    for (tool, input) in [
        ("move_file", r#"{"source":"f.txt","destination":"g.txt"}"#),
        ("copy_file", r#"{"source":"f.txt","destination":"h.txt"}"#),
        ("delete_file", r#"{"path":"f.txt"}"#),
    ] {
        let result = super::execute_tool(
            tool,
            input,
            &ws,
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("TOOL_BLOCKED") || result.contains("blocked"),
            "{tool} must be blocked in review session; got: {result}"
        );
    }
}

#[tokio::test]
async fn grep_find_allowed_in_review_session() {
    let (ingot, vault) = mk_ingot_vault();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
    let session = session_with_mode(Some("review"));

    let grep_result = super::execute_tool(
        "grep_files",
        r#"{"pattern":"fn main"}"#,
        &ws,
        Some(&session),
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        !grep_result.contains("TOOL_BLOCKED"),
        "grep_files must be allowed in review session; got: {grep_result}"
    );

    let find_result = super::execute_tool(
        "find_files",
        r#"{"pattern":"*.rs"}"#,
        &ws,
        Some(&session),
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    assert!(
        !find_result.contains("TOOL_BLOCKED"),
        "find_files must be allowed in review session; got: {find_result}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fetch_web_ssrf_loopback_blocked() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;
    let _g = NETWORK_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("SMEDJA_SANDBOX_NETWORK", "open");
    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();
    let result = super::execute_tool(
        "fetch_web",
        r#"{"url":"http://127.0.0.1/"}"#,
        ws.path(),
        None,
        &ingot,
        &vault,
        &test_embedder(),
    )
    .await;
    std::env::remove_var("SMEDJA_SANDBOX_NETWORK");
    assert!(
        result.starts_with("error:"),
        "loopback IP must be blocked by SSRF policy: {result}"
    );
    assert!(result.contains("SSRF"), "error must mention SSRF: {result}");
}

// ── large response offload ────────────────────────────────────────────────

#[tokio::test]
async fn large_text_output_is_offloaded_rather_than_returned_verbatim() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();

    // Generate a text result that exceeds the threshold and won't be
    // compressed by the filter registry (not a known command output pattern).
    let large = "z".repeat(super::LARGE_RESPONSE_THRESHOLD + 1);

    let out = super::filter_command_output(
        "unknown_cmd",
        large.clone(),
        ws.path(),
        None,
        &ingot,
        &vault,
    )
    .await;

    assert!(
        out.len() < large.len(),
        "offloaded output must be shorter than original ({} bytes returned)",
        out.len()
    );
    assert!(
        out.contains("bytes"),
        "reference must mention byte count: {out}"
    );
}

#[tokio::test]
async fn small_text_output_below_threshold_is_returned_verbatim() {
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let ws = tempfile::tempdir().unwrap();

    let small = "hello world";

    let out = super::filter_command_output(
        "unknown_cmd",
        small.to_owned(),
        ws.path(),
        None,
        &ingot,
        &vault,
    )
    .await;

    assert_eq!(out, small, "small output must pass through unchanged");
}

// --- WI-027: load_skill tool ---

#[tokio::test]
async fn load_skill_returns_wrapped_body_for_installed_skill() {
    let tmp = tempfile::tempdir().expect("tmp");
    let skills_dir = tmp.path().to_path_buf();
    // Write a minimal flat skill file.
    let skill_content = "---\nname: myskill\ndescription: A test skill.\n---\nDo the thing.\n";
    std::fs::write(skills_dir.join("myskill.md"), skill_content).unwrap();

    let result = super::execute_load_skill("myskill", &skills_dir);
    assert!(result.contains("Do the thing."), "body must be present");
    assert!(result.contains("<skill_content"), "must be wrapped");
}

#[tokio::test]
async fn load_skill_returns_error_for_missing_skill() {
    let tmp = tempfile::tempdir().expect("tmp");
    let result = super::execute_load_skill("nonexistent", tmp.path());
    assert!(
        result.starts_with("error:"),
        "missing skill must return error"
    );
}
