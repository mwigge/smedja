use super::*;

pub(crate) async fn dispatch_workspace(action: WorkspaceCmd, sock: &std::path::Path) -> Result<()> {
    match action {
        WorkspaceCmd::Agents {
            action: agents_action,
        } => match agents_action {
            AgentsCmd::Show => {
                let mut client = Client::connect(sock)
                    .await
                    .with_context(|| format!("smdjad not running ({})", sock.display()))?;
                cmd_workspace_agents(&mut client).await?;
            }
            AgentsCmd::Init => {
                let target = std::path::Path::new(".smedja");
                std::fs::create_dir_all(target)?;
                let agents_toml = target.join("agents.toml");
                if agents_toml.exists() {
                    anyhow::bail!(".smedja/agents.toml already exists");
                }
                std::fs::write(&agents_toml, AGENTS_TOML_TEMPLATE)?;
                println!("Created {}", agents_toml.display());
            }
        },
        WorkspaceCmd::Init { path } => {
            let target = path.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            if !target.exists() {
                anyhow::bail!("path does not exist: {}", target.display());
            }
            let mut client = Client::connect(sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            let resp = client
                .call(
                    "graph.index",
                    json!({ "workspace": target.display().to_string() }),
                )
                .await
                .context("graph.index failed")?;
            let count = resp["indexed"].as_u64().unwrap_or(0);
            println!("Indexed {count} symbols in {}", target.display());
            cmd_workspace_init(&target)?;
        }
        WorkspaceCmd::Index => {
            // The server resolves the enclosing git root from this path and runs
            // an incremental (mtime-based) index, so launching from a subdir
            // still indexes the whole repository.
            let workspace =
                std::env::current_dir().context("cannot determine current directory")?;
            let mut client = Client::connect(sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            let resp = client
                .call(
                    "graph.index",
                    json!({ "workspace": workspace.display().to_string() }),
                )
                .await
                .context("graph.index failed")?;
            let count = resp["indexed"].as_u64().unwrap_or(0);
            println!("Indexed {count} symbols in {}", workspace.display());
        }
        WorkspaceCmd::Add { path } => {
            let workspace =
                std::env::current_dir().context("cannot determine current directory")?;
            let smedja_dir = workspace.join(".smedja");
            std::fs::create_dir_all(&smedja_dir)?;
            let toml_path = smedja_dir.join("workspace.toml");

            // Read existing content or start fresh.
            let mut content = if toml_path.exists() {
                std::fs::read_to_string(&toml_path).context("failed to read workspace.toml")?
            } else {
                String::new()
            };

            // Append the new path entry.
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str("\n[[workspace.paths]]\npath = \"");
            content.push_str(&path);
            content.push_str("\"\n");
            std::fs::write(&toml_path, &content).context("failed to write workspace.toml")?;
            println!("Added path '{}' to {}", path, toml_path.display());
        }
    }
    Ok(())
}

pub(crate) async fn cmd_workspace_agents(client: &mut Client) -> Result<()> {
    println!("{:<15} {:<10} {:<8} MODEL", "ROLE", "RUNNER", "TIER");
    println!("{}", "-".repeat(55));

    for role_name in &["orchestrator", "impl", "test", "review", "sre"] {
        let resp = client
            .call(
                "agent.routing",
                json!({ "role": role_name, "complexity": "coding" }),
            )
            .await
            .context("agent.routing failed")?;
        let runner = resp["runner"].as_str().unwrap_or("-");
        let tier = resp["tier"].as_str().unwrap_or("-");
        let model = resp["model"].as_str().unwrap_or("-");
        println!("{role_name:<15} {runner:<10} {tier:<8} {model}");
    }
    Ok(())
}

pub(crate) fn cmd_workspace_init(dir: &std::path::Path) -> Result<()> {
    use chrono::Utc;
    let smedja_dir = dir.join(".smedja");
    std::fs::create_dir_all(&smedja_dir)?;
    let toml_path = smedja_dir.join("workspace.toml");
    let ts = Utc::now().to_rfc3339();
    let content = format!("[graph]\nauto_index = true\nlast_indexed_at = \"{ts}\"\n");
    std::fs::write(&toml_path, content)?;
    println!("Initialized workspace at {}", dir.display());
    Ok(())
}
