//! MCP HTTP transport — JSON-RPC 2.0 client for tool discovery.
//!
//! Connects to an MCP server at a known URL. OAuth PKCE flow is a
//! future extension; for now, a static Bearer token is supported.

use tokio::sync::broadcast;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// M6 — Resources, Prompts, protocol version negotiation
// ---------------------------------------------------------------------------

/// MCP protocol versions in preference order (newest first).
pub const MCP_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// An MCP resource entry as returned by `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// An MCP prompt entry as returned by `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpPrompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A server-initiated MCP notification (no `id` field).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpNotification {
    pub server: String,
    pub method: String,
}

// ---------------------------------------------------------------------------
// M18 — MCP notification bus and background reader
// ---------------------------------------------------------------------------

/// Capacity of the notification broadcast channel.
const NOTIFICATION_CHANNEL_CAP: usize = 64;

/// Type alias for the sender half of the MCP notification bus.
pub type McpNotificationTx = broadcast::Sender<McpNotification>;

/// Creates a new notification bus and returns `(tx, rx)`.
///
/// Multiple consumers may subscribe by calling [`McpNotificationTx::subscribe`].
#[must_use]
pub fn notification_bus() -> (
    broadcast::Sender<McpNotification>,
    broadcast::Receiver<McpNotification>,
) {
    broadcast::channel(NOTIFICATION_CHANNEL_CAP)
}

/// Classifies a JSON-RPC notification by its `method` field and decides
/// whether it should trigger a tool-list refresh.
///
/// Returns `true` for `notifications/tools/list_changed`.
#[must_use]
pub fn is_tools_list_change(notification: &McpNotification) -> bool {
    notification.method == "notifications/tools/list_changed"
}

/// Spawns a background Tokio task that reads newline-delimited JSON-RPC
/// messages from `reader`, parses them as notifications, and broadcasts them
/// on `tx`.  The task exits when the reader returns EOF or an error.
///
/// This is the stdio-transport notification pump: attach it to the stdout pipe
/// of a stdio MCP server process.
pub fn spawn_notification_reader<R>(
    server: String,
    reader: R,
    tx: McpNotificationTx,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncBufRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt as _;
        let mut lines = tokio::io::BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            // A notification has "method" but no "id".
            if v.get("id").is_some() {
                continue;
            }
            let Some(method) = v["method"].as_str() else {
                continue;
            };
            let notification = McpNotification {
                server: server.clone(),
                method: method.to_owned(),
            };
            // Ignore send errors (no active receivers is fine).
            let _ = tx.send(notification);
        }
    })
}

/// Returns the negotiated protocol version given the server's declared version.
///
/// Scans [`MCP_PROTOCOL_VERSIONS`] in preference order and returns the first
/// version that is ≤ the server's declared version (string comparison works
/// because the format is `YYYY-MM-DD`).  Falls back to the server's own version
/// when none of ours is older-or-equal.
#[must_use]
pub fn negotiate_protocol_version(server_version: &str) -> &'static str {
    for &v in MCP_PROTOCOL_VERSIONS {
        if v <= server_version {
            return v;
        }
    }
    // Server is older than all our known versions; accept oldest known.
    MCP_PROTOCOL_VERSIONS[MCP_PROTOCOL_VERSIONS.len() - 1]
}

/// Reconnect delay sequence for stdio MCP clients (in seconds).
///
/// Exponential backoff capped at 30 s, up to 5 retries.
pub const RECONNECT_DELAYS_SECS: &[u64] = &[1, 2, 4, 8, 30];

/// A tool entry as returned by an MCP `tools/list` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// Minimal JSON-RPC 2.0 client for MCP HTTP servers.
pub struct McpHttpClient {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl McpHttpClient {
    /// Creates a new client pointing at `url` with optional Bearer `token`.
    ///
    /// # Errors
    ///
    /// Returns an error if the reqwest client cannot be built.
    pub fn new(url: &str, token: &str) -> Result<Self, reqwest::Error> {
        Ok(Self {
            url: url.to_owned(),
            token: token.to_owned(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()?,
        })
    }

    /// Calls `tools/call` on the MCP server and returns the result as a JSON
    /// string suitable for use as a tool response.
    ///
    /// Follows the MCP HTTP transport specification (2024-11 draft):
    /// - POST to the server URL
    /// - Body: `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":input}}`
    /// - Response: `{"result":{"content":[...],"isError":bool}}`
    ///
    /// # Errors
    ///
    /// Returns an error string on network failure, non-OK HTTP status, or
    /// JSON-RPC error from the server.
    pub async fn call_tool(&self, name: &str, input: &serde_json::Value) -> Result<String, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": input }
        });

        let mut req = self.http.post(&self.url).json(&body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("MCP HTTP request failed: {e}"))?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("MCP response parse failed: {e}"))?;

        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(format!("MCP server error: {msg}"));
        }

        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        if result
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let msg = result
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|entry| entry.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("tool returned an error");
            return Err(format!("MCP tool error: {msg}"));
        }

        Ok(serde_json::to_string(&result).unwrap_or_default())
    }

    /// Calls `tools/list` on the MCP server and returns the tool list.
    ///
    /// # Errors
    ///
    /// Returns an error on network failure or if the response is not valid JSON-RPC.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        });

        let mut req = self.http.post(&self.url).json(&body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;

        if let Some(err) = resp.get("error") {
            tracing::warn!(error = %err, url = %self.url, "MCP server returned error response");
        }

        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();

        Ok(tools)
    }

    /// Calls `resources/list` and returns the resource list.
    ///
    /// # Errors
    ///
    /// Returns an error string on network failure or parse failure.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/list",
            "params": {}
        });

        let mut req = self.http.post(&self.url).json(&body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;

        let resources = resp
            .get("result")
            .and_then(|r| r.get("resources"))
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        Ok(resources)
    }

    /// Calls `resources/read` and returns the resource content.
    ///
    /// # Errors
    ///
    /// Returns an error string on network failure or JSON-RPC error.
    pub async fn read_resource(&self, uri: &str) -> Result<String, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": { "uri": uri }
        });

        let mut req = self.http.post(&self.url).json(&body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;

        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("MCP server error: {msg}"));
        }

        Ok(
            serde_json::to_string(resp.get("result").unwrap_or(&serde_json::Value::Null))
                .unwrap_or_default(),
        )
    }

    /// Calls `prompts/list` and returns the prompt list.
    ///
    /// # Errors
    ///
    /// Returns an error string on network failure or parse failure.
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "prompts/list",
            "params": {}
        });

        let mut req = self.http.post(&self.url).json(&body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;

        let prompts = resp
            .get("result")
            .and_then(|r| r.get("prompts"))
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        Ok(prompts)
    }

    /// Calls `prompts/get` and returns the rendered prompt text.
    ///
    /// # Errors
    ///
    /// Returns an error string on network failure or JSON-RPC error.
    pub async fn get_prompt(
        &self,
        name: &str,
        args: std::collections::HashMap<String, String>,
    ) -> Result<String, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "prompts/get",
            "params": { "name": name, "arguments": args }
        });

        let mut req = self.http.post(&self.url).json(&body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())?;

        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("MCP server error: {msg}"));
        }

        Ok(
            serde_json::to_string(resp.get("result").unwrap_or(&serde_json::Value::Null))
                .unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn call_tool_against_mock_server_returns_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "content": [{ "type": "text", "text": "hello from tool" }],
                            "isError": false
                        }
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = McpHttpClient::new(&format!("http://{addr}"), "").unwrap();
        let result = client
            .call_tool("echo", &serde_json::json!({"text": "hi"}))
            .await
            .expect("call_tool must succeed");

        assert!(
            result.contains("hello from tool"),
            "result must contain tool output; got: {result}"
        );
    }

    #[tokio::test]
    async fn call_tool_propagates_mcp_is_error_flag() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "content": [{ "type": "text", "text": "permission denied" }],
                            "isError": true
                        }
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = McpHttpClient::new(&format!("http://{addr}"), "").unwrap();
        let result = client.call_tool("restricted", &serde_json::json!({})).await;

        assert!(result.is_err(), "isError: true must produce an Err result");
        assert!(
            result.unwrap_err().contains("permission denied"),
            "error message must include tool's error text"
        );
    }

    #[test]
    fn client_new_succeeds() {
        let client = McpHttpClient::new("http://localhost:9999", "token123");
        assert!(client.is_ok());
    }

    #[test]
    fn mcp_tool_deserializes() {
        let json = r#"{"name":"read_file","description":"Read a file","input_schema":{}}"#;
        let tool: McpTool = serde_json::from_str(json).unwrap();
        assert_eq!(tool.name, "read_file");
    }

    #[tokio::test]
    async fn list_tools_against_mock_server() {
        // Start a minimal mock MCP server on a random port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "tools": [
                                {
                                    "name": "read_file",
                                    "description": "Read a file from the workspace",
                                    "input_schema": {}
                                }
                            ]
                        }
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        // Give the server a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = McpHttpClient::new(&format!("http://{addr}"), "").unwrap();
        let tools = client
            .list_tools()
            .await
            .expect("list_tools should succeed");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].description, "Read a file from the workspace");
    }

    /// Integration test: registers a local mock MCP HTTP server, discovers its
    /// tool list, and verifies the tools appear in the smedja-ingot registry.
    ///
    /// Flow:
    ///   1. Spawn a minimal axum server that returns a valid MCP tool list.
    ///   2. Create an in-memory `Ingot` and register the server entry.
    ///   3. Call `McpHttpClient::list_tools()` against the local server.
    ///   4. Serialise the returned tools and persist via `update_mcp_tools`.
    ///   5. Call `get_all_mcp_tools` and verify the tool appears in routing.
    #[tokio::test]
    async fn tool_discovery_persists_into_ingot_registry() {
        // --- Step 1: spawn a minimal local MCP HTTP server on an ephemeral port ---
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
                            "tools": [
                                {
                                    "name": "echo",
                                    "description": "Echo input",
                                    "input_schema": { "type": "object" }
                                }
                            ]
                        }
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        // Give the server a moment to start listening.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // --- Step 2: create an in-memory Ingot and register the server entry ---
        let server_name = "test-echo-server";
        let ig = smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open");

        let mcp_entry = smedja_ingot::McpServer {
            id: "test-echo-server-id".into(),
            name: server_name.into(),
            url: server_url.clone(),
            transport: "http".into(),
            tools_json: "[]".into(),
            last_refresh: 0.0,
        };
        ig.register_mcp_server(&mcp_entry)
            .expect("register_mcp_server must succeed");

        // Confirm the registry now contains the server (with an empty tool list).
        let servers_before = ig.list_mcp_servers().unwrap();
        assert_eq!(servers_before.len(), 1);
        assert_eq!(servers_before[0].name, server_name);
        assert_eq!(servers_before[0].tools_json, "[]");

        // --- Step 3: call list_tools against the local MCP server ---
        let client = McpHttpClient::new(&server_url, "").expect("McpHttpClient::new must succeed");
        let tools = client
            .list_tools()
            .await
            .expect("list_tools must succeed against local server");

        assert_eq!(tools.len(), 1, "server must return exactly one tool");
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo input");

        // --- Step 4: serialise and persist via update_mcp_tools ---
        let tools_json = serde_json::to_string(&tools).expect("tools must serialise to JSON");

        ig.update_mcp_tools(server_name, &tools_json)
            .expect("update_mcp_tools must succeed");

        // --- Step 5: verify tools appear in the routing surface ---
        let all_tools = ig
            .get_all_mcp_tools()
            .expect("get_all_mcp_tools must succeed");

        assert_eq!(
            all_tools.len(),
            1,
            "exactly one server must have non-empty tools_json"
        );
        assert_eq!(
            all_tools[0].0, server_name,
            "tool entry must be keyed by server name"
        );

        // Confirm the persisted JSON round-trips to the original tool list.
        let persisted: Vec<McpTool> =
            serde_json::from_str(&all_tools[0].1).expect("persisted tools_json must deserialise");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].name, "echo");
        assert_eq!(persisted[0].description, "Echo input");

        // Confirm last_refresh was updated (non-zero after update_mcp_tools).
        let servers_after = ig.list_mcp_servers().unwrap();
        assert!(
            servers_after[0].last_refresh > 0.0,
            "last_refresh must be non-zero after update_mcp_tools"
        );
    }

    // -------------------------------------------------------------------------
    // M6 — Resources, Prompts, protocol version negotiation, reconnect backoff
    // -------------------------------------------------------------------------

    #[test]
    fn mcp_resource_serializes_correctly() {
        let resource = McpResource {
            uri: "file:///foo/bar.txt".into(),
            name: "bar.txt".into(),
            description: Some("A text file".into()),
            mime_type: Some("text/plain".into()),
        };
        let json = serde_json::to_string(&resource).unwrap();
        let back: McpResource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resource);
    }

    #[test]
    fn mcp_prompt_serializes_correctly() {
        let prompt = McpPrompt {
            name: "summarize".into(),
            description: Some("Summarize a document".into()),
        };
        let json = serde_json::to_string(&prompt).unwrap();
        let back: McpPrompt = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prompt);
    }

    #[test]
    fn protocol_version_negotiation_prefers_latest() {
        // Server declares "2025-03-26" — we accept that (it's in our list).
        let negotiated = negotiate_protocol_version("2025-03-26");
        assert_eq!(negotiated, "2025-03-26");
    }

    #[test]
    fn protocol_version_negotiation_accepts_server_older_than_all_known() {
        // Server is older than all versions we know — fall back to oldest known.
        let negotiated = negotiate_protocol_version("2023-01-01");
        assert_eq!(
            negotiated,
            MCP_PROTOCOL_VERSIONS[MCP_PROTOCOL_VERSIONS.len() - 1]
        );
    }

    #[test]
    fn reconnect_backoff_delays_grow() {
        let delays = RECONNECT_DELAYS_SECS;
        assert_eq!(delays.len(), 5, "must have exactly 5 retry delays");
        assert_eq!(delays[0], 1);
        assert_eq!(delays[1], 2);
        assert_eq!(delays[2], 4);
        assert_eq!(delays[3], 8);
        assert_eq!(delays[4], 30);
        // Verify monotone growth up to the cap.
        for w in delays.windows(2) {
            assert!(w[1] >= w[0], "delays must be non-decreasing");
        }
    }

    #[test]
    fn mcp_notification_parsed_from_json() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        // A notification has "method" but no "id".
        assert!(v.get("method").is_some());
        assert!(v.get("id").is_none());
        let n = McpNotification {
            server: "my-server".into(),
            method: v["method"].as_str().unwrap().to_owned(),
        };
        assert_eq!(n.method, "notifications/tools/list_changed");
    }

    #[tokio::test]
    async fn tools_list_change_triggers_refresh() {
        let (tx, mut rx) = notification_bus();
        // Feed a single notification line through the background reader.
        let json = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n";
        let reader = std::io::Cursor::new(json.as_bytes());
        let handle = spawn_notification_reader("test-server".into(), reader, tx);
        // Wait for the notification to arrive.
        let notification = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for notification")
            .expect("channel closed");
        // The notification should be classified as a tool-list change.
        assert!(
            is_tools_list_change(&notification),
            "notification must be classified as a tool-list change"
        );
        assert_eq!(notification.server, "test-server");
        handle.await.ok();
    }
}
