use super::*;

pub(crate) async fn dispatch_mcp(action: McpCmd, sock: &std::path::Path) -> Result<()> {
    let mut client = Client::connect(sock)
        .await
        .with_context(|| format!("smdjad not running ({})", sock.display()))?;
    match action {
        McpCmd::Add { name, url, stdio } => {
            let resp = client
                .call(
                    "mcp.register",
                    json!({
                        "name": name,
                        "url": url,
                        "transport": if stdio.is_some() { "stdio" } else { "http" },
                        "tools_json": null,
                    }),
                )
                .await
                .context("mcp.register failed")?;
            println!("registered: {}", resp["name"].as_str().unwrap_or(&name));
        }
        McpCmd::List => {
            let servers = client
                .call("mcp.list", json!({}))
                .await
                .context("mcp.list failed")?;
            if let Some(arr) = servers.as_array() {
                for s in arr {
                    println!(
                        "{} {} ({})",
                        s["name"].as_str().unwrap_or("?"),
                        s["url"].as_str().unwrap_or(""),
                        s["transport"].as_str().unwrap_or("?"),
                    );
                }
            }
        }
        McpCmd::Remove { name } => {
            client
                .call("mcp.remove", json!({ "name": name }))
                .await
                .context("mcp.remove failed")?;
            println!("removed: {name}");
        }
        McpCmd::Refresh { name } => {
            let mut params = serde_json::json!({});
            if let Some(n) = name {
                params["name"] = serde_json::Value::String(n);
            }
            let result: serde_json::Value = client
                .call("mcp.refresh", params)
                .await
                .map_err(|e| anyhow::anyhow!("mcp.refresh failed: {e}"))?;
            println!(
                "Refreshed {} server(s)",
                result
                    .get("refreshed")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
            );
        }
    }
    Ok(())
}
