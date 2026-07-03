//! MCP-server registry operations.

use crate::{mcp, Ingot, IngotError, McpServer};
impl Ingot {
    // mcp_servers ------------------------------------------------------------

    /// Registers (or replaces) an [`McpServer`] in the registry.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT OR REPLACE fails.
    #[must_use = "check the Result to confirm the MCP server was registered"]
    pub fn register_mcp_server(&self, server: &McpServer) -> Result<(), IngotError> {
        mcp::insert(&self.conn, server)
    }

    /// Returns all registered [`McpServer`]s ordered by `name` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned servers"]
    pub fn list_mcp_servers(&self) -> Result<Vec<McpServer>, IngotError> {
        mcp::list(&self.conn)
    }

    /// Removes the [`McpServer`] with the given `id` from the registry.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the DELETE fails.
    #[must_use = "check the Result to confirm the MCP server was removed"]
    pub fn remove_mcp_server(&self, id: &str) -> Result<(), IngotError> {
        mcp::remove(&self.conn, id)
    }

    /// Updates the cached tool list and refresh timestamp for the server identified
    /// by `name`. Sets `last_refresh` to the current Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the tool list was updated"]
    pub fn update_mcp_tools(&self, name: &str, tools_json: &str) -> Result<(), IngotError> {
        mcp::update_tools(&self.conn, name, tools_json)
    }

    /// Returns all registered [`McpServer`]s whose `last_refresh` is older than
    /// `older_than_secs` seconds ago, or that have never been refreshed.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned servers"]
    pub fn get_stale_servers(&self, older_than_secs: f64) -> Result<Vec<McpServer>, IngotError> {
        mcp::stale(&self.conn, older_than_secs)
    }

    /// Returns all MCP servers that have a non-empty `tools_json`, as
    /// `(server_name, tools_json)` pairs.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned tool pairs"]
    pub fn get_all_mcp_tools(&self) -> Result<Vec<(String, String)>, IngotError> {
        mcp::all_tools(&self.conn)
    }

    /// Looks up a single [`McpServer`] by its registered name, returning `None`
    /// when no server with that name exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned server"]
    pub fn get_mcp_server_by_name(&self, name: &str) -> Result<Option<McpServer>, IngotError> {
        mcp::by_name(&self.conn, name)
    }

    /// Finds the MCP server that exposes a tool with the given `tool_name`.
    ///
    /// Searches the `tools_json` of every server that has a non-empty tool
    /// list.  Returns the first server whose list contains a tool entry with
    /// `"name": tool_name`, or `None` when no match is found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the tool-list query fails.
    #[must_use = "check the Result; None means no registered MCP server owns this tool"]
    pub fn find_mcp_server_for_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        for (server_name, tools_json) in mcp::all_tools(&self.conn)? {
            let tools: Vec<serde_json::Value> =
                serde_json::from_str(&tools_json).unwrap_or_default();
            let owns_tool = tools
                .iter()
                .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(tool_name));
            if owns_tool {
                return mcp::by_name(&self.conn, &server_name);
            }
        }
        Ok(None)
    }
}
