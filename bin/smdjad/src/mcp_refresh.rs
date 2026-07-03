//! Background refresh of stale MCP server tool lists at daemon startup.

use smedja_ingot::{IngotHandle, McpServer};

/// Refreshes stale MCP server tool lists in the background so startup is not
/// delayed by N×network_latency when multiple servers are registered.
///
/// Spawns detached tasks; returns immediately. A dedicated notification bus is
/// created and drained here so server-initiated notifications never block a
/// sender.
pub(crate) fn spawn_mcp_refresh(ingot: IngotHandle) {
    // Notification bus for MCP server-initiated notifications (e.g. tool-list
    // changes). Receivers can subscribe via McpNotificationTx::subscribe().
    let (mcp_notification_tx, _mcp_notification_rx) = crate::mcp_http::notification_bus();

    let ingot_clone = ingot.clone();
    let notification_tx = mcp_notification_tx.clone();
    tokio::spawn(async move {
        let stale_threshold = crate::common::now_epoch() - 3600.0;
        let servers = ingot_clone
            .list_mcp_servers()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.last_refresh < stale_threshold)
            .collect::<Vec<_>>();

        for server in servers {
            let env_token = std::env::var("MCP_TOKEN").ok();
            let token = crate::executor::resolve_mcp_token(
                &crate::mcp_oauth::TokenStore::default_store(),
                &server.url,
                env_token.as_deref(),
            );

            // For stdio servers, spawn a dedicated notification reader on a
            // second child process so the primary request/response child is
            // not disturbed. The reader exits when the child closes stdout.
            if server.transport == "stdio" {
                let server_name = server.name.clone();
                let command = server.url.clone();
                let tx = notification_tx.clone();
                tokio::spawn(async move {
                    use std::process::Stdio;
                    // Validate: reject shell metacharacters (mirrors McpStdioClient).
                    const SHELL_METAS: &[char] = &[
                        ';', '&', '|', '`', '$', '>', '<', '(', ')', '{', '}', '\n', '\r',
                    ];
                    if command.chars().any(|c| SHELL_METAS.contains(&c)) {
                        tracing::debug!(
                            server = %server_name,
                            "skipping notification reader: command contains disallowed characters"
                        );
                        return;
                    }
                    let mut parts = command.split_whitespace();
                    let Some(program) = parts.next() else { return };
                    let args: Vec<&str> = parts.collect();
                    let Ok(mut child) = tokio::process::Command::new(program)
                        .args(&args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .kill_on_drop(true)
                        .spawn()
                    else {
                        tracing::debug!(
                            server = %server_name,
                            "skipping notification reader: could not spawn child"
                        );
                        return;
                    };
                    let Some(stdout) = child.stdout.take() else {
                        return;
                    };
                    let reader = tokio::io::BufReader::new(stdout);
                    let handle =
                        crate::mcp_http::spawn_notification_reader(server_name.clone(), reader, tx);
                    tracing::debug!(server = %server_name, "MCP notification reader spawned");
                    handle.await.ok();
                });
            }

            let transport = match crate::mcp_stdio::McpTransport::for_server(&server, &token) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(name = %server.name, error = %e, "failed to build MCP transport at startup");
                    continue;
                }
            };
            match transport.list_tools().await {
                Ok(tools) => {
                    let tools_json =
                        serde_json::to_string(&tools).unwrap_or_else(|_| "[]".to_owned());
                    let updated = McpServer {
                        tools_json,
                        last_refresh: crate::common::now_epoch(),
                        ..server.clone()
                    };
                    if let Err(e) = ingot_clone.register_mcp_server(updated).await {
                        tracing::warn!(name = %server.name, error = %e, "failed to update MCP tools at startup");
                    }
                }
                Err(e) => {
                    tracing::warn!(name = %server.name, error = %e, "MCP refresh failed at startup");
                }
            }
        }

        // Log notifications at debug level. The receiver runs until this
        // task exits, draining the bus so no sender ever blocks.
        let mut rx = notification_tx.subscribe();
        loop {
            match rx.recv().await {
                Ok(n) => {
                    tracing::debug!(
                        server = %n.server,
                        method = %n.method,
                        "MCP server notification received"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(
                        dropped = n,
                        "MCP notification bus lagged; some notifications dropped"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
