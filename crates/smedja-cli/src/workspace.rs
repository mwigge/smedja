use super::*;

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
