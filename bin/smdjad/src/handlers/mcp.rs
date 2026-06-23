//! MCP RPC handlers: `mcp.register/list/remove/refresh`.

use serde_json::{json, Value};
use smedja_ingot::McpServer;
use smedja_rpc::{codes, RpcError};
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{ingot_err, is_safe_mcp_url, missing_param};

/// Handles `mcp.register`.
///
/// # Errors
///
/// Returns an error when `name` is missing, the URL is not permitted, or the
/// ingot write fails.
pub(crate) async fn register(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("name"))?
        .to_owned();
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if !is_safe_mcp_url(&url) {
        return Err(RpcError::new(codes::INVALID_PARAMS, "url not permitted"));
    }
    let transport = params
        .get("transport")
        .and_then(Value::as_str)
        .unwrap_or("http")
        .to_owned();
    let server = McpServer {
        id: Uuid::new_v4().to_string(),
        name,
        url,
        transport,
        tools_json: "[]".into(),
        last_refresh: 0.0,
    };
    ig.register_mcp_server(server.clone())
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "id": server.id }))
}

/// Handles `mcp.list`.
///
/// # Errors
///
/// Returns an error when the ingot query fails.
pub(crate) async fn list(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let servers = ig.list_mcp_servers().await.map_err(|e| ingot_err(&e))?;
    let out: Vec<Value> = servers
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "url": s.url,
                "transport": s.transport,
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Handles `mcp.remove`.
///
/// # Errors
///
/// Returns an error when `name` is missing or the ingot write fails.
pub(crate) async fn remove(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let name = params["name"]
        .as_str()
        .ok_or_else(|| missing_param("name"))?
        .to_owned();
    ig.remove_mcp_server(&name)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "name": name, "removed": true }))
}

/// Handles `mcp.refresh`: re-queries each registered server's tool list.
///
/// # Errors
///
/// Returns an error when listing the registered servers fails.
pub(crate) async fn refresh(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let name_filter: Option<String> = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // Load the candidate servers — all registered, or the named one.
    let servers = {
        let all = ig.list_mcp_servers().await.map_err(|e| ingot_err(&e))?;
        match name_filter {
            Some(ref name) => all
                .into_iter()
                .filter(|s| &s.name == name)
                .collect::<Vec<_>>(),
            None => all,
        }
    };

    let mut refreshed = 0usize;
    for server in servers {
        let client = match crate::mcp_http::McpHttpClient::new(&server.url, "") {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(name = %server.name, error = %e, "mcp.refresh: failed to build client");
                continue;
            }
        };
        match client.list_tools().await {
            Ok(tools) => {
                let tools_json = serde_json::to_string(&tools).unwrap_or_else(|_| "[]".to_owned());
                let updated = McpServer {
                    tools_json,
                    last_refresh: crate::common::now_epoch(),
                    ..server.clone()
                };
                let _ = ig.register_mcp_server(updated).await;
                refreshed += 1;
            }
            Err(e) => {
                tracing::warn!(name = %server.name, error = %e, "mcp.refresh failed");
            }
        }
    }

    Ok(json!({ "refreshed": refreshed }))
}
