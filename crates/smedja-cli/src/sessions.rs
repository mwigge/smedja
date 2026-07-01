use super::*;

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
