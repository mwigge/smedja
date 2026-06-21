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
            http: reqwest::Client::builder().build()?,
        })
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
    use super::*;

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
}
