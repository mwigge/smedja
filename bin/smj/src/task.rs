//! `smj task` — project task management, parallel tasks, and JSONL import/export.

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;
use smedja_ingot::Ingot;
use smedja_rpc::client::Client;

use crate::util::{connect_or_exit, default_ingot_path};

#[derive(Subcommand)]
pub(crate) enum TaskCmd {
    /// List tasks (optionally filtered by status)
    List {
        #[arg(long)]
        status: Option<String>,
    },
    /// Show details of a specific task
    Show { id: String },
    /// Create a new project task
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// Mark a task complete
    Close { id: String },
    /// Start a parallel task across multiple agent roles
    Parallel {
        /// Goal description passed to all roles
        goal: String,
        /// Comma-separated roles: impl,test,review
        #[arg(long, value_delimiter = ',')]
        roles: Vec<String>,
    },
    /// Show per-role status of a parallel task
    Status {
        /// Parallel task ID returned by `smj task parallel`
        id: String,
    },
    /// Cancel a running parallel task
    Cancel {
        /// Parallel task ID to cancel
        id: String,
    },
    /// Export tasks (and their audit events) as JSONL to stdout
    Export {
        /// Filter to tasks whose title contains this change name
        #[arg(long)]
        change: Option<String>,
    },
    /// Import tasks and audit events from JSONL on stdin
    Import,
}

/// Dispatches a `smj task` subcommand.
pub(crate) async fn run(sock: &Path, action: TaskCmd) -> Result<()> {
    // Export and Import operate on the local Ingot DB directly without
    // needing a running smdjad daemon.
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

async fn cmd_task_list(client: &mut Client, status: Option<&str>) -> Result<()> {
    let params = match status {
        Some(s) => json!({"status": s}),
        None => serde_json::Value::Null,
    };
    let resp = client
        .call("task.list", params)
        .await
        .context("task.list failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn cmd_task_show(client: &mut Client, id: &str) -> Result<()> {
    let resp = client
        .call("task.get", json!({"id": id}))
        .await
        .context("task.get failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn cmd_task_create(
    client: &mut Client,
    title: &str,
    description: Option<&str>,
) -> Result<()> {
    let params = json!({
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

async fn cmd_task_close(client: &mut Client, id: &str) -> Result<()> {
    client
        .call("task.close", json!({"id": id}))
        .await
        .context("task.close failed")?;
    println!("Task {id} closed");
    Ok(())
}
