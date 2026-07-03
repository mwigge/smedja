//! MCP-server registry handle methods.

use crate::{Ingot, IngotError, IngotHandle, McpServer};
impl IngotHandle {
    // ── mcp_servers ──────────────────────────────────────────────────────────

    /// Registers (or replaces) an [`McpServer`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT OR REPLACE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn register_mcp_server(&self, server: McpServer) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.register_mcp_server(&server))
            .await
    }

    /// Returns all registered [`McpServer`]s ordered by `name` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_mcp_servers(&self) -> Result<Vec<McpServer>, IngotError> {
        self.run_blocking(Ingot::list_mcp_servers).await
    }

    /// Removes the [`McpServer`] with the given `id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying DELETE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn remove_mcp_server(&self, id: &str) -> Result<(), IngotError> {
        let id = id.to_owned();
        self.run_blocking(move |ig| ig.remove_mcp_server(&id)).await
    }

    /// Updates the cached tool list for a server.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_mcp_tools(&self, name: &str, tools_json: &str) -> Result<(), IngotError> {
        let name = name.to_owned();
        let tools_json = tools_json.to_owned();
        self.run_blocking(move |ig| ig.update_mcp_tools(&name, &tools_json))
            .await
    }

    /// Returns stale [`McpServer`]s whose `last_refresh` is older than `older_than_secs`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_stale_servers(
        &self,
        older_than_secs: f64,
    ) -> Result<Vec<McpServer>, IngotError> {
        self.run_blocking(move |ig| ig.get_stale_servers(older_than_secs))
            .await
    }

    /// Returns all `(server_name, tools_json)` pairs for servers with non-empty tool lists.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_all_mcp_tools(&self) -> Result<Vec<(String, String)>, IngotError> {
        self.run_blocking(Ingot::get_all_mcp_tools).await
    }

    /// Looks up a single [`McpServer`] by its registered name.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_mcp_server_by_name(
        &self,
        name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        let name = name.to_owned();
        self.run_blocking(move |ig| ig.get_mcp_server_by_name(&name))
            .await
    }

    /// Finds the MCP server that exposes a tool with the given `tool_name`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] if the tool-list query fails, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn find_mcp_server_for_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        let tool_name = tool_name.to_owned();
        self.run_blocking(move |ig| ig.find_mcp_server_for_tool(&tool_name))
            .await
    }
}
