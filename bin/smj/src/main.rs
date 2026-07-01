#[tokio::main]
async fn main() -> anyhow::Result<()> {
    smedja_cli::run().await
}
