//! `smj mcp` — MCP server registry management.

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;

use crate::util::connect;

#[derive(Subcommand)]
pub(crate) enum McpCmd {
    /// Register an MCP server
    Add {
        name: String,
        url: String,
        #[arg(long)]
        stdio: Option<String>,
    },
    /// List registered MCP servers
    List,
    /// Remove an MCP server by name
    Remove { name: String },
    /// Re-fetch tool lists from registered servers
    Refresh {
        /// Refresh a specific server only (omit for all)
        name: Option<String>,
    },
}

/// Dispatches a `smj mcp` subcommand.
pub(crate) async fn run(sock: &Path, action: McpCmd) -> Result<()> {
    let mut client = connect(sock).await?;
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
            let mut params = json!({});
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
