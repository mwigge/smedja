use super::*;

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
