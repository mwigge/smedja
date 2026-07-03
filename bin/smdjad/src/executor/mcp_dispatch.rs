//! Outbound MCP tool dispatch: resolving the owning server for a tool name,
//! selecting the transport, and forwarding the call with the resolved credential.

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

#[cfg(test)]
mod tests {
    use super::{dispatch_mcp_tool, dispatch_mcp_tool_with_store, resolve_mcp_token};

    #[tokio::test]
    async fn dispatch_mcp_tool_returns_error_when_no_server_registered() {
        let ig = smedja_ingot::IngotHandle::new(
            smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
        );
        let result = dispatch_mcp_tool("unknown_tool", &serde_json::json!({}), &ig).await;
        assert!(
            result.starts_with("error:"),
            "unregistered tool must return an error; got: {result}"
        );
        assert!(
            result.contains("unknown_tool"),
            "error must include the tool name; got: {result}"
        );
    }

    #[tokio::test]
    async fn dispatch_mcp_tool_routes_to_registered_server() {
        use tokio::net::TcpListener;

        // Spawn a minimal mock MCP server that responds to tools/call.
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
                            "content": [{ "type": "text", "text": "dispatched-ok" }],
                            "isError": false
                        }
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Register the mock server in an in-memory Ingot.
        let ig = smedja_ingot::IngotHandle::new(
            smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
        );
        ig.register_mcp_server(smedja_ingot::McpServer {
            id: "mock-1".into(),
            name: "mock-server".into(),
            url: server_url,
            transport: "http".into(),
            tools_json: r#"[{"name":"greet","description":"Greet"}]"#.into(),
            last_refresh: 1.0,
        })
        .await
        .expect("register_mcp_server must succeed");

        let result = dispatch_mcp_tool("greet", &serde_json::json!({"name": "world"}), &ig).await;
        assert!(
            result.contains("dispatched-ok"),
            "must return the mock server's response; got: {result}"
        );
    }

    #[test]
    fn resolve_mcp_token_prefers_stored_token() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
        let token = crate::mcp_oauth::Token {
            access_token: "stored-bearer".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            expires_in: Some(3600),
        };
        store.save("https://srv.example.com", &token).unwrap();
        let resolved = resolve_mcp_token(&store, "https://srv.example.com", None);
        assert_eq!(resolved, "stored-bearer");
    }

    #[test]
    fn resolve_mcp_token_falls_back_to_env_then_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
        // No stored token, MCP_TOKEN provided → env value.
        let resolved = resolve_mcp_token(&store, "https://none.example.com", Some("env-bearer"));
        assert_eq!(resolved, "env-bearer");
        // No stored token, no env → empty (unauthenticated path).
        let resolved = resolve_mcp_token(&store, "https://none.example.com", None);
        assert_eq!(resolved, "");
    }

    #[tokio::test]
    async fn dispatch_mcp_tool_sends_stored_token_as_bearer() {
        use std::sync::Arc;

        use tokio::net::TcpListener;
        use tokio::sync::Mutex as TokioMutex;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_url = format!("http://{addr}");

        // The mock server echoes back whether it saw the expected Bearer.
        let seen_auth: Arc<TokioMutex<Option<String>>> = Arc::new(TokioMutex::new(None));
        let seen_clone = Arc::clone(&seen_auth);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(move |headers: axum::http::HeaderMap| {
                    let seen = Arc::clone(&seen_clone);
                    async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        *seen.lock().await = auth;
                        axum::Json(serde_json::json!({
                            "jsonrpc": "2.0", "id": 1,
                            "result": { "content": [{ "type": "text", "text": "ok" }], "isError": false }
                        }))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Persist a token keyed by the server URL in a scoped store.
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
        store
            .save(
                &server_url,
                &crate::mcp_oauth::Token {
                    access_token: "abc123".into(),
                    token_type: "Bearer".into(),
                    refresh_token: None,
                    expires_in: Some(3600),
                },
            )
            .unwrap();

        let ig = smedja_ingot::IngotHandle::new(
            smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
        );
        ig.register_mcp_server(smedja_ingot::McpServer {
            id: "auth-1".into(),
            name: "auth-server".into(),
            url: server_url.clone(),
            transport: "http".into(),
            tools_json: r#"[{"name":"greet","description":"Greet"}]"#.into(),
            last_refresh: 1.0,
        })
        .await
        .expect("register_mcp_server must succeed");

        let result =
            dispatch_mcp_tool_with_store("greet", &serde_json::json!({}), &ig, &store, None).await;
        let _ = result; // result body is not under test here.

        let observed = seen_auth.lock().await.clone();
        assert_eq!(
            observed.as_deref(),
            Some("Bearer abc123"),
            "stored token must be sent as the Bearer credential"
        );
    }
}
