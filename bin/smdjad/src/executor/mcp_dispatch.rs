//! MCP tool dispatch for the executor.
//!
//! Routes tool calls that are not handled natively to the registered MCP server
//! that owns the tool, resolving the outbound Bearer credential and transport.

use smedja_ingot::IngotHandle;

/// Dispatches a tool call to the MCP server that owns `tool_name`.
///
/// Queries the ingot registry for a registered MCP server whose `tools_json`
/// contains an entry named `tool_name`, then forwards the call to that server
/// via `McpHttpClient::call_tool`.  Returns an error string if no server owns
/// the tool or if the HTTP call fails.
pub(crate) async fn dispatch_mcp_tool(
    tool_name: &str,
    input: &serde_json::Value,
    ingot: &IngotHandle,
) -> String {
    let store = crate::mcp_oauth::TokenStore::default_store();
    let env_token = std::env::var("MCP_TOKEN").ok();
    dispatch_mcp_tool_with_store(tool_name, input, ingot, &store, env_token.as_deref()).await
}

/// Resolves the outbound MCP Bearer credential for `server_url`.
///
/// Resolution order: a token persisted in `store`, then the `MCP_TOKEN`
/// environment value (`env_token`), then an empty string (the unauthenticated
/// path, preserving back-compatibility).
pub(crate) fn resolve_mcp_token(
    store: &crate::mcp_oauth::TokenStore,
    server_url: &str,
    env_token: Option<&str>,
) -> String {
    if let Ok(Some(token)) = store.load(server_url) {
        return token.access_token;
    }
    env_token.unwrap_or("").to_owned()
}

/// Dispatches an MCP tool call, resolving the outbound token from `store` (then
/// `env_token`, then empty) and selecting the transport from the registered
/// server's `transport` field.
pub(crate) async fn dispatch_mcp_tool_with_store(
    tool_name: &str,
    input: &serde_json::Value,
    ingot: &IngotHandle,
    store: &crate::mcp_oauth::TokenStore,
    env_token: Option<&str>,
) -> String {
    let server = match ingot.find_mcp_server_for_tool(tool_name).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::debug!(tool = tool_name, "no MCP server registered for tool");
            return format!("error: tool '{tool_name}' is not available");
        }
        Err(e) => {
            tracing::warn!(tool = tool_name, error = %e, "ingot error looking up MCP tool");
            return format!("error: tool '{tool_name}' is not available");
        }
    };

    let token = resolve_mcp_token(store, &server.url, env_token);

    tracing::debug!(
        tool = tool_name,
        server = %server.name,
        url = %server.url,
        transport = %server.transport,
        "dispatching MCP tool call"
    );

    let transport = match crate::mcp_stdio::McpTransport::for_server(&server, &token) {
        Ok(t) => t,
        Err(e) => {
            return format!(
                "error: could not connect to MCP server '{}': {e}",
                server.name
            )
        }
    };

    match transport.call_tool(tool_name, input).await {
        Ok(result) => result,
        Err(e) => format!("error: MCP tool call failed: {e}"),
    }
}
