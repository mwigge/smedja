use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "smj", about = "smedja control CLI")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon lifecycle
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Session management
    Session {
        #[command(subcommand)]
        action: SessionCmd,
    },
    /// Workspace tools
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCmd,
    },
    /// Audit log queries
    Audit {
        #[arg(long)] session: Option<String>,
        #[arg(long)] since:   Option<String>,
    },
    /// Cost ledger
    Cost {
        #[arg(long)] session: Option<String>,
        #[arg(long)] since:   Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonCmd { Start, Stop, Restart, Status }

#[derive(Subcommand)]
enum SessionCmd {
    List,
    Show   { id: String },
    Fork   { id: String, #[arg(long)] turn: Option<u32> },
    Rollback { id: String, turn: u32 },
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    Agents,
    Skills { #[command(subcommand)] action: SkillsCmd },
    Index,
}

#[derive(Subcommand)]
enum SkillsCmd { List, Add { path: String } }

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Daemon { action } => match action {
            DaemonCmd::Status => println!("smdjad: not yet implemented"),
            _ => println!("smj daemon: not yet implemented"),
        },
        _ => println!("smj: not yet implemented"),
    }
    Ok(())
}
