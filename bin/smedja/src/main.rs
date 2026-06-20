use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "smedja", about = "smedja terminal client")]
struct Cli {
    /// smdjad socket path (default: `$XDG_RUNTIME_DIR/smdjad.sock`)
    #[arg(long, env = "SMEDJA_SOCK")]
    sock: Option<String>,

    /// Agent mode (impl|review|test|sre|explain)
    #[arg(long, short = 'm')]
    mode: Option<String>,

    /// Tier override (local|fast|deep)
    #[arg(long, short = 't')]
    tier: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let _cli = Cli::parse();
    // TODO: initialise ratatui layout, connect to smdjad, start chat loop
    println!("smedja — coming soon");
    Ok(())
}
