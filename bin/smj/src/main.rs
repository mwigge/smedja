use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use smedja_plugins::SkillRegistry;
use smedja_rpc::client::Client;

#[derive(Parser)]
#[command(name = "smj", about = "smedja control CLI")]
struct Cli {
    /// smdjad socket path (overrides `XDG_RUNTIME_DIR`)
    #[arg(long, env = "SMEDJA_SOCK", global = true)]
    sock: Option<PathBuf>,

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
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        since: Option<String>,
    },
    /// Cost ledger
    Cost {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        since: Option<String>,
    },
    /// Manage Claude Code skill files
    Skill {
        #[command(subcommand)]
        action: SkillCmd,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start smdjad in the background
    Start,
    /// Stop a running smdjad
    Stop,
    /// Restart smdjad
    Restart,
    /// Check whether smdjad is running
    Status,
}

#[derive(Subcommand)]
enum SessionCmd {
    List,
    Show {
        id: String,
    },
    Fork {
        id: String,
        #[arg(long)]
        turn: Option<u32>,
    },
    Rollback {
        id: String,
        turn: u32,
    },
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    Agents,
    Index,
}

#[derive(Subcommand)]
enum SkillCmd {
    /// List installed skills
    List,
    /// Install a skill from a SKILL.md file or directory
    Install {
        /// Path to a SKILL.md file or a directory containing one
        path: PathBuf,
    },
    /// Update an existing skill from a SKILL.md file or directory
    Update {
        /// Skill name to update
        name: String,
        /// Path to the new SKILL.md file or a directory containing one
        path: PathBuf,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name to remove
        name: String,
    },
    /// Sync all skills from a bundle directory using symlinks
    Sync {
        /// Path to a directory of skills (e.g. agent-toolkit-bundle/skills)
        path: PathBuf,
    },
}

fn default_socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("smdjad.sock")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let sock = cli.sock.unwrap_or_else(default_socket_path);

    match cli.command {
        Cmd::Daemon { action } => match action {
            DaemonCmd::Status => cmd_daemon_status(&sock).await?,
            DaemonCmd::Start => cmd_daemon_start()?,
            _ => println!(
                "smj daemon {}: not yet implemented",
                match action {
                    DaemonCmd::Stop => "stop",
                    DaemonCmd::Restart => "restart",
                    _ => unreachable!(),
                }
            ),
        },
        Cmd::Skill { action } => {
            let registry = SkillRegistry::new(SkillRegistry::default_path());
            match action {
                SkillCmd::List => cmd_skill_list(&registry)?,
                SkillCmd::Install { path } => cmd_skill_install(&registry, &path)?,
                SkillCmd::Update { name, path } => cmd_skill_update(&registry, &name, &path)?,
                SkillCmd::Remove { name } => cmd_skill_remove(&registry, &name)?,
                SkillCmd::Sync { path } => cmd_skill_sync(&registry, &path)?,
            }
        }
        _ => println!("smj: not yet implemented"),
    }
    Ok(())
}

async fn cmd_daemon_status(sock: &std::path::Path) -> Result<()> {
    match Client::connect(sock).await {
        Err(_) => {
            println!(
                "smdjad: not running (socket not found at {})",
                sock.display()
            );
            std::process::exit(1);
        }
        Ok(mut client) => {
            let resp = client
                .call("ping", serde_json::Value::Null)
                .await
                .with_context(|| "ping failed")?;
            println!("smdjad: running ({})", sock.display());
            println!("response: {resp}");
            Ok(())
        }
    }
}

fn cmd_daemon_start() -> Result<()> {
    // Locate smdjad relative to this binary.
    let exe = std::env::current_exe().context("cannot determine own path")?;
    let smdjad = exe
        .parent()
        .map(|p| p.join("smdjad"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("smdjad"));

    std::process::Command::new(&smdjad)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", smdjad.display()))?;

    println!("smdjad started");
    Ok(())
}

fn cmd_skill_list(registry: &SkillRegistry) -> Result<()> {
    let skills = registry.scan()?;
    if skills.is_empty() {
        println!(
            "No skills installed at {}",
            SkillRegistry::default_path().display()
        );
        return Ok(());
    }
    println!("{:<30} {:<10} DESCRIPTION", "NAME", "VERSION");
    println!("{}", "-".repeat(80));
    for skill in &skills {
        let version = skill.manifest.version.as_deref().unwrap_or("-");
        let desc = skill
            .manifest
            .description
            .lines()
            .next()
            .unwrap_or("")
            .trim();
        println!("{:<30} {:<10} {}", skill.manifest.name, version, desc);
    }
    println!("\n{} skill(s) installed", skills.len());
    Ok(())
}

fn cmd_skill_install(registry: &SkillRegistry, path: &std::path::Path) -> Result<()> {
    let (name, content) = read_skill_file(path)?;
    registry
        .install(&name, &content)
        .with_context(|| format!("failed to install skill `{name}`"))?;
    println!("Installed skill `{name}`");
    Ok(())
}

fn cmd_skill_update(registry: &SkillRegistry, name: &str, path: &std::path::Path) -> Result<()> {
    let (_parsed_name, content) = read_skill_file(path)?;
    registry
        .update(name, &content)
        .with_context(|| format!("failed to update skill `{name}`"))?;
    println!("Updated skill `{name}`");
    Ok(())
}

fn cmd_skill_remove(registry: &SkillRegistry, name: &str) -> Result<()> {
    registry
        .remove(name)
        .with_context(|| format!("failed to remove skill `{name}`"))?;
    println!("Removed skill `{name}`");
    Ok(())
}

fn cmd_skill_sync(registry: &SkillRegistry, path: &std::path::Path) -> Result<()> {
    println!("Syncing from {} ...", path.display());
    let r = registry
        .sync_from(path)
        .with_context(|| format!("sync failed from {}", path.display()))?;
    for (name, reason) in &r.errors {
        println!("  error:   {name} — {reason}");
    }
    println!(
        "\n{} linked, {} updated, {} skipped, {} error(s)",
        r.linked,
        r.updated,
        r.skipped,
        r.errors.len()
    );
    Ok(())
}

/// Resolves a path to its SKILL.md content and the skill name from frontmatter.
fn read_skill_file(path: &std::path::Path) -> Result<(String, String)> {
    let skill_md = if path.is_dir() {
        path.join("SKILL.md")
    } else {
        path.to_owned()
    };
    let content = std::fs::read_to_string(&skill_md)
        .with_context(|| format!("cannot read {}", skill_md.display()))?;
    let skill = smedja_plugins::parse_skill(&content, &skill_md)
        .with_context(|| format!("invalid frontmatter in {}", skill_md.display()))?;
    Ok((skill.manifest.name, content))
}
