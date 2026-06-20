use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    // TODO: bind UDS socket, start Tokio supervision tree, load sessions from ingot
    tracing::info!("smdjad starting");
    tokio::signal::ctrl_c().await?;
    tracing::info!("smdjad shutting down");
    Ok(())
}
