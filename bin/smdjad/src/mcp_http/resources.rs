//! MCP `resources/*` and `prompts/*` client methods.

use super::client::McpHttpClient;
use super::types::{McpPrompt, McpResource};

impl McpHttpClient {
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
