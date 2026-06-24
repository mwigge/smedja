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

    let store = crate::mcp_oauth::TokenStore::default_store();
    let env_token = std::env::var("MCP_TOKEN").ok();
    let refreshed = refresh_servers(&ig, servers, &store, env_token.as_deref()).await;

    Ok(json!({ "refreshed": refreshed }))
}

/// Re-queries each server's tool list via its transport, authenticating with a
/// token resolved from `store` (then `env_token`, then empty), and persists the
/// refreshed list. Returns the count of successfully refreshed servers.
async fn refresh_servers(
    ig: &smedja_ingot::IngotHandle,
    servers: Vec<McpServer>,
    store: &crate::mcp_oauth::TokenStore,
    env_token: Option<&str>,
) -> usize {
    let mut refreshed = 0usize;
    for server in servers {
        let token = crate::executor::resolve_mcp_token(store, &server.url, env_token);
        let transport = match crate::mcp_stdio::McpTransport::for_server(&server, &token) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(name = %server.name, error = %e, "mcp.refresh: failed to build transport");
                continue;
            }
        };
        match transport.list_tools().await {
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
    refreshed
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::net::TcpListener;
    use tokio::sync::Mutex as TokioMutex;

    use super::refresh_servers;

    #[tokio::test]
    async fn refresh_sends_stored_bearer_to_protected_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_url = format!("http://{addr}");

        let seen: Arc<TokioMutex<Option<String>>> = Arc::new(TokioMutex::new(None));
        let seen_clone = Arc::clone(&seen);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(move |headers: axum::http::HeaderMap| {
                    let seen = Arc::clone(&seen_clone);
                    async move {
                        *seen.lock().await = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        axum::Json(serde_json::json!({
                            "jsonrpc": "2.0", "id": 1,
                            "result": { "tools": [] }
                        }))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Stored token keyed by the server URL, in a scoped store.
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
        store
            .save(
                &server_url,
                &crate::mcp_oauth::Token {
                    access_token: "refresh-bearer".into(),
                    token_type: "Bearer".into(),
                    refresh_token: None,
                    expires_in: Some(3600),
                },
            )
            .unwrap();

        let ig = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
        let server = smedja_ingot::McpServer {
            id: "r1".into(),
            name: "protected".into(),
            url: server_url.clone(),
            transport: "http".into(),
            tools_json: "[]".into(),
            last_refresh: 0.0,
        };

        let count = refresh_servers(&ig, vec![server], &store, None).await;
        assert_eq!(count, 1, "refresh must succeed");
        assert_eq!(
            seen.lock().await.as_deref(),
            Some("Bearer refresh-bearer"),
            "refresh must send the stored token as the Bearer credential"
        );
    }
}
