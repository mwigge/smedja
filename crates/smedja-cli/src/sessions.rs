use super::*;
use crate::daemon::connect_or_exit;

pub(crate) async fn dispatch_session(action: SessionCmd, sock: &std::path::Path) -> Result<()> {
    if let SessionCmd::Blocks { id } = action {
        eprintln!("smj session blocks: 'session.blocks' RPC not yet implemented");
        eprintln!("  session: {id}");
        std::process::exit(1);
    } else {
        let mut client = connect_or_exit(sock).await;
        match action {
            SessionCmd::Start { cowork, task } => {
                let resp = client
                    .call(
                        "session.create",
                        json!({
                            "cowork_mode": cowork,
                            "task_description": task,
                        }),
                    )
                    .await
                    .context("session.create failed")?;
                let session_id = resp["id"].as_str().unwrap_or("?");
                println!("Session: {session_id}");
                if let Some(task_id) = resp["task_id"].as_str() {
                    println!("Task created: {task_id}");
                }
            }
            SessionCmd::List => cmd_session_list(&mut client).await?,
            SessionCmd::Show { id } => cmd_session_show(&mut client, &id).await?,
            SessionCmd::Rollback { id, turn } => {
                cmd_session_rollback(&mut client, &id, turn).await?;
            }
            SessionCmd::Fork { id, .. } => {
                let resp = client
                    .call("session.fork", json!({ "session_id": id }))
                    .await
                    .context("session.fork failed")?;
                println!(
                    "Forked: {} → {}",
                    id,
                    resp["session_id"].as_str().unwrap_or("?")
                );
            }
            SessionCmd::Checkpoint { id } => {
                let resp = client
                    .call("session.checkpoint.list", json!({ "session_id": id }))
                    .await
                    .context("session.checkpoint.list failed")?;
                if let Some(arr) = resp.as_array() {
                    for cp in arr {
                        println!(
                            "turn={} ts={} messages={}",
                            cp["turn_n"].as_i64().unwrap_or(-1),
                            cp["created_at"].as_f64().unwrap_or(0.0),
                            cp["message_count"].as_u64().unwrap_or(0),
                        );
                    }
                }
            }
            SessionCmd::Blocks { .. } => unreachable!(),
            SessionCmd::Export { id, format } => {
                if format == "md" {
                    let resp = client
                        .call(
                            "session.messages",
                            json!({ "session_id": id, "limit": 1000 }),
                        )
                        .await
                        .context("session.messages failed")?;
                    let messages = resp["messages"].as_array().cloned().unwrap_or_default();
                    println!("# Session {id}\n");
                    for msg in &messages {
                        let role = msg["role"].as_str().unwrap_or("unknown");
                        let content = msg["content"].as_str().unwrap_or("");
                        match role {
                            "user" => println!("**User:**\n\n{content}\n"),
                            "assistant" => println!("**Assistant:**\n\n{content}\n"),
                            _ => println!("**{role}:**\n\n{content}\n"),
                        }
                    }
                } else {
                    let resp = client
                        .call("session.export", json!({ "id": id }))
                        .await
                        .context("session.export failed")?;
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                }
            }
            SessionCmd::Compact { id } => {
                let resp = client
                    .call("session.compact", json!({ "session_id": id }))
                    .await
                    .context("session.compact failed")?;
                let summary = resp["summary"].as_str().unwrap_or("(no summary)");
                println!("Compaction summary for {id}:\n{summary}");
            }
            SessionCmd::Tokens { id } => {
                let resp = client
                    .call("session.token_usage", json!({ "session_id": id }))
                    .await
                    .context("session.token_usage failed")?;
                if let Some(arr) = resp.as_array() {
                    println!(
                        "{:<8} {:<12} {:<12} {:<16} {:<16}",
                        "turn_n", "input_tok", "output_tok", "cum_input", "cum_output"
                    );
                    println!("{}", "-".repeat(68));
                    for snap in arr {
                        println!(
                            "{:<8} {:<12} {:<12} {:<16} {:<16}",
                            snap["turn_n"].as_i64().unwrap_or(-1),
                            snap["input_tok"].as_i64().unwrap_or(0),
                            snap["output_tok"].as_i64().unwrap_or(0),
                            snap["cumulative_input"].as_i64().unwrap_or(0),
                            snap["cumulative_output"].as_i64().unwrap_or(0),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn cmd_session_list(client: &mut Client) -> Result<()> {
    let resp = client
        .call("session.list", serde_json::Value::Null)
        .await
        .context("session.list failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

pub(crate) async fn cmd_session_show(client: &mut Client, id: &str) -> Result<()> {
    let resp = client
        .call("session.get", json!({"id": id}))
        .await
        .context("session.get failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    let cowork = resp["cowork_mode"].as_bool().unwrap_or(false);
    println!("Cowork mode: {}", if cowork { "yes" } else { "no" });
    if let Some(task_id) = resp["task_id"].as_str() {
        println!("Active task: {task_id}");
    }
    Ok(())
}

pub(crate) async fn cmd_session_rollback(
    client: &mut Client,
    session_id: &str,
    turn: u32,
) -> Result<()> {
    let resp = client
        .call(
            "session.rollback",
            json!({"session_id": session_id, "turn_n": turn}),
        )
        .await
        .context("session.rollback failed")?;
    println!(
        "Rolled back to turn {}: {}",
        turn,
        serde_json::to_string_pretty(&resp)?
    );
    Ok(())
}
