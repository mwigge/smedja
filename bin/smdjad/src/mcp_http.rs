//! MCP HTTP transport — JSON-RPC 2.0 client for tool discovery.
//!
//! Connects to an MCP server at a known URL. OAuth PKCE flow is a
//! future extension; for now, a static Bearer token is supported.

use serde::{Deserialize, Serialize};

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
            http: reqwest::Client::new(),
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

        let result = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);

        if result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
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
        let result = client
            .call_tool("restricted", &serde_json::json!({}))
            .await;

        assert!(
            result.is_err(),
            "isError: true must produce an Err result"
        );
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
        let mut ig = smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open");

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
}
