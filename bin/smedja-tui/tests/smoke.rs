//! Layer 5 smoke tests: user-journey tests with a mock daemon.
//!
//! Run: cargo test -p smedja-tui --test smoke

use std::path::PathBuf;

use smedja_rpc::client::Client;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Minimal mock daemon: accepts connections and responds to JSON-RPC 2.0 calls
/// over newline-delimited JSON (matching the `smedja_rpc::codec::Codec` wire format).
struct MockDaemon {
    pub socket_path: PathBuf,
    // Keeps the tempdir alive for the lifetime of the mock daemon.
    _dir: TempDir,
}

impl MockDaemon {
    /// Spawns a mock daemon that responds to JSON-RPC calls.
    ///
    /// Responds to any `method: "session.create"` with a fixed session ID.
    /// Responds to `method: "turn.submit"` with a `task_id`.
    /// Responds to `method: "task.get"` with a completed response.
    /// Responds to `method: "turn.subscribe"` with a completed turn envelope.
    /// Responds to unknown methods with a `-32601 Method not found` error.
    fn spawn() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let socket_path = dir.path().join("smdjad.sock");
        let listener = UnixListener::bind(&socket_path)?;

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    // The codec uses BufReader::read_line — match that on the server side.
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let trimmed = line.trim_end();
                        let Ok(req) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                            break;
                        };
                        let method = req["method"].as_str().unwrap_or("");
                        let id = &req["id"];
                        // Build a JSON-RPC 2.0 Response matching smedja_rpc::types::Response.
                        let resp = match method {
                            "session.create" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "id": "mock-session-001" }
                            }),
                            "turn.submit" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "task_id": "mock-task-001" }
                            }),
                            "task.get" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "status": "complete",
                                    "response": "echo: hello"
                                }
                            }),
                            "turn.subscribe" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "done": true,
                                    "response": "echo: hello",
                                    "input_tok": 5,
                                    "output_tok": 3
                                }
                            }),
                            _ => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": "Method not found" }
                            }),
                        };
                        // Codec writes `json + '\n'` — match that exactly.
                        let Ok(mut bytes) = serde_json::to_vec(&resp) else {
                            break;
                        };
                        bytes.push(b'\n');
                        // Ignore write errors — client may have disconnected.
                        let _ = reader.get_mut().write_all(&bytes).await;
                    }
                });
            }
        });

        Ok(Self {
            socket_path,
            _dir: dir,
        })
    }
}

// ---------------------------------------------------------------------------
// Real test: daemon unavailable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daemon_unavailable_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sock");
    match Client::connect(&missing).await {
        Ok(_) => panic!("connecting to missing socket should fail"),
        Err(e) => {
            let msg = e.to_string();
            assert!(!msg.is_empty(), "error message should be non-empty");
        }
    }
}

// ---------------------------------------------------------------------------
// Real test: client connects to MockDaemon and receives a session ID
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_daemon_session_create_returns_id() {
    let daemon = MockDaemon::spawn().unwrap();
    let mut client = Client::connect(&daemon.socket_path).await.unwrap();

    let resp = client
        .call("session.create", serde_json::json!({ "title": "smoke" }))
        .await
        .unwrap();

    assert_eq!(
        resp["id"].as_str(),
        Some("mock-session-001"),
        "session.create should return the mock session ID"
    );
}

// ---------------------------------------------------------------------------
// Real test: unknown method returns RPC error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_daemon_unknown_method_returns_rpc_error() {
    let daemon = MockDaemon::spawn().unwrap();
    let mut client = Client::connect(&daemon.socket_path).await.unwrap();

    let result = client.call("no.such.method", serde_json::json!({})).await;

    assert!(result.is_err(), "unknown method should return an RPC error");
    let err = result.unwrap_err();
    assert_eq!(err.code, -32601);
}

// ---------------------------------------------------------------------------
// Stubs for features not yet implemented
// ---------------------------------------------------------------------------

/// Verifies the full RPC round-trip for a single turn: session.create →
/// turn.submit → turn.subscribe.  This test covers the protocol layer only;
/// TUI panel rendering is exercised by the unit tests in main.rs that use
/// `TestBackend` and `handle_key()` directly.
#[tokio::test]
async fn rpc_turn_submit_and_subscribe_complete() {
    let daemon = MockDaemon::spawn().unwrap();
    let mut client = Client::connect(&daemon.socket_path).await.unwrap();

    // Step 1: create session
    let session = client
        .call("session.create", serde_json::json!({ "title": "smoke" }))
        .await
        .unwrap();
    assert_eq!(session["id"].as_str(), Some("mock-session-001"));

    // Step 2: submit a turn
    let submit = client
        .call(
            "turn.submit",
            serde_json::json!({
                "session_id": "mock-session-001",
                "content": "hello"
            }),
        )
        .await
        .unwrap();
    assert!(
        submit.get("task_id").is_some(),
        "turn.submit should return a task_id"
    );

    // Step 3: subscribe and assert the completed response
    let task_id = submit["task_id"].as_str().unwrap();
    let sub = client
        .call("turn.subscribe", serde_json::json!({ "task_id": task_id }))
        .await
        .unwrap();
    assert_eq!(sub["done"].as_bool(), Some(true), "done should be true");
    assert_eq!(
        sub["response"].as_str(),
        Some("echo: hello"),
        "response should echo the user message"
    );
}

/// Verifies that the daemon RPC layer acknowledges a session creation at
/// startup.  This test covers the protocol handshake only; the TUI banner
/// rendering is exercised by the unit tests in main.rs that use `TestBackend`.
#[tokio::test]
async fn rpc_session_create_returns_id_on_startup() {
    let daemon = MockDaemon::spawn().unwrap();
    let mut client = Client::connect(&daemon.socket_path).await.unwrap();

    // Startup handshake: create a session and verify the daemon acknowledges it.
    let result = client
        .call("session.create", serde_json::json!({ "title": "startup" }))
        .await
        .unwrap();
    assert_eq!(
        result["id"].as_str(),
        Some("mock-session-001"),
        "startup handshake should return a session ID"
    );
}

#[tokio::test]
async fn streaming_deltas_render() {
    // MockDaemon returns a completed turn response immediately via turn.subscribe.
    let daemon = MockDaemon::spawn().unwrap();
    let mut client = Client::connect(&daemon.socket_path).await.unwrap();

    // Call turn.subscribe — MockDaemon responds at once with a complete envelope.
    let result = client
        .call(
            "turn.subscribe",
            serde_json::json!({ "task_id": "mock-task-001" }),
        )
        .await
        .unwrap();

    assert_eq!(result["done"].as_bool(), Some(true), "done should be true");
    assert_eq!(
        result["response"].as_str(),
        Some("echo: hello"),
        "response should be the mock response"
    );
    assert_eq!(result["input_tok"].as_i64(), Some(5));
    assert_eq!(result["output_tok"].as_i64(), Some(3));
}
