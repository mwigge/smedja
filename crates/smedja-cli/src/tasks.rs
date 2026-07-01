use super::*;

pub(crate) async fn dispatch_task(action: TaskCmd, sock: &std::path::Path) -> Result<()> {
    // Export and Import operate on the local Ingot DB directly without needing
    // a running smdjad daemon.
    match action {
        TaskCmd::Export { change } => {
            let db_path = default_ingot_path();
            let ingot = Ingot::open(&db_path)
                .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
            let records = ingot
                .export_jsonl(change.as_deref())
                .context("export_jsonl failed")?;
            for rec in &records {
                println!("{}", serde_json::to_string(rec)?);
            }
            return Ok(());
        }
        TaskCmd::Import => {
            use std::io::BufRead as _;
            let db_path = default_ingot_path();
            let ingot = Ingot::open(&db_path)
                .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
            let stdin = std::io::stdin();
            let mut records: Vec<serde_json::Value> = Vec::new();
            for line in stdin.lock().lines() {
                let line = line.context("failed to read stdin")?;
                let line = line.trim().to_owned();
                if line.is_empty() {
                    continue;
                }
                let val: serde_json::Value =
                    serde_json::from_str(&line).context("invalid JSON line")?;
                records.push(val);
            }
            let n = ingot
                .import_jsonl(&records)
                .context("import_jsonl failed")?;
            println!("Imported {n} record(s)");
            return Ok(());
        }
        _ => {}
    }

    let mut client = connect_or_exit(sock).await;
    match action {
        TaskCmd::List { status } => cmd_task_list(&mut client, status.as_deref()).await?,
        TaskCmd::Show { id } => cmd_task_show(&mut client, &id).await?,
        TaskCmd::Create { title, description } => {
            cmd_task_create(&mut client, &title, description.as_deref()).await?;
        }
        TaskCmd::Close { id } => cmd_task_close(&mut client, &id).await?,
        TaskCmd::Parallel { goal, roles } => {
            let resp = client
                .call("task.parallel", json!({ "goal": goal, "roles": roles }))
                .await
                .context("task.parallel failed")?;
            if let Some(tasks) = resp["tasks"].as_array() {
                for t in tasks {
                    println!(
                        "{} ({})",
                        t["task_id"].as_str().unwrap_or("?"),
                        t["role"].as_str().unwrap_or("?"),
                    );
                }
            }
        }
        TaskCmd::Status { id } => {
            let resp = client
                .call("task.get", json!({ "id": id }))
                .await
                .context("task.get failed")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).unwrap_or_default()
            );
        }
        TaskCmd::Cancel { id } => {
            client
                .call("task.cancel", json!({ "task_id": id }))
                .await
                .context("task.cancel failed")?;
            println!("cancelled: {id}");
        }
        // Already handled above; unreachable but required for exhaustiveness.
        TaskCmd::Export { .. } | TaskCmd::Import => unreachable!(),
    }
    Ok(())
}

pub(crate) async fn cmd_task_list(client: &mut Client, status: Option<&str>) -> Result<()> {
    let params = match status {
        Some(s) => serde_json::json!({"status": s}),
        None => serde_json::Value::Null,
    };
    let resp = client
        .call("task.list", params)
        .await
        .context("task.list failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

pub(crate) async fn cmd_task_show(client: &mut Client, id: &str) -> Result<()> {
    let resp = client
        .call("task.get", serde_json::json!({"id": id}))
        .await
        .context("task.get failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

pub(crate) async fn cmd_task_create(
    client: &mut Client,
    title: &str,
    description: Option<&str>,
) -> Result<()> {
    let params = serde_json::json!({
        "title": title,
        "description": description.unwrap_or(""),
    });
    let resp = client
        .call("task.create", params)
        .await
        .context("task.create failed")?;
    println!("Created task {}", resp["id"].as_str().unwrap_or("?"));
    Ok(())
}

pub(crate) async fn cmd_task_close(client: &mut Client, id: &str) -> Result<()> {
    client
        .call("task.close", serde_json::json!({"id": id}))
        .await
        .context("task.close failed")?;
    println!("Task {id} closed");
    Ok(())
}
