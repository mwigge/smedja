use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
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
    /// Project task management
    Task {
        #[command(subcommand)]
        action: TaskCmd,
    },
    /// Loop engine control
    Loop {
        #[command(subcommand)]
        action: LoopCmd,
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

#[derive(Subcommand)]
enum TaskCmd {
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
}

#[derive(Subcommand)]
enum LoopCmd {
    /// Run a loop against an `OpenSpec` change
    Run {
        /// Name of the `OpenSpec` change to drive
        #[arg(long)]
        change: String,
        /// Maximum number of task slices to process
        #[arg(long, default_value = "10")]
        max_slices: u32,
    },
    /// Show loop status for a change
    Status {
        /// Name of the `OpenSpec` change to query
        #[arg(long)]
        change: String,
    },
    /// Cancel a running loop for a change
    Cancel {
        /// Name of the `OpenSpec` change to cancel
        #[arg(long)]
        change: String,
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
        Cmd::Task { action } => {
            let mut client = Client::connect(&sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            match action {
                TaskCmd::List { status } => cmd_task_list(&mut client, status.as_deref()).await?,
                TaskCmd::Show { id } => cmd_task_show(&mut client, &id).await?,
                TaskCmd::Create { title, description } => {
                    cmd_task_create(&mut client, &title, description.as_deref()).await?;
                }
                TaskCmd::Close { id } => cmd_task_close(&mut client, &id).await?,
            }
        }
        Cmd::Session { action } => {
            let mut client = Client::connect(&sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            match action {
                SessionCmd::List => cmd_session_list(&mut client).await?,
                SessionCmd::Show { id } => cmd_session_show(&mut client, &id).await?,
                SessionCmd::Rollback { id, turn } => {
                    cmd_session_rollback(&mut client, &id, turn).await?;
                }
                SessionCmd::Fork { id, .. } => {
                    println!("fork: not yet implemented for session {id}");
                }
            }
        }
        Cmd::Cost { session, .. } => {
            let mut client = Client::connect(&sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            let session_id = session.unwrap_or_default();
            if session_id.is_empty() {
                println!("smj cost: --session <session-id> required");
                return Ok(());
            }
            let resp = client
                .call("session.cost", json!({"session_id": session_id}))
                .await
                .context("session.cost failed")?;
            let usd = resp["total_usd"].as_f64().unwrap_or(0.0);
            println!("Session {session_id}: ${usd:.6}");
        }
        Cmd::Workspace { action } => match action {
            WorkspaceCmd::Agents => cmd_workspace_agents()?,
            WorkspaceCmd::Index => println!("smj workspace index: not yet implemented"),
        },
        Cmd::Audit { .. } => {
            println!("smj audit: not yet implemented");
        }
        Cmd::Loop { action } => match action {
            LoopCmd::Run { change, max_slices } => {
                println!(
                    "smj loop run --change {change} --max-slices {max_slices}: not yet implemented"
                );
            }
            LoopCmd::Status { change } => {
                println!("smj loop status --change {change}: not yet implemented");
            }
            LoopCmd::Cancel { change } => {
                println!("smj loop cancel --change {change}: not yet implemented");
            }
        },
    }
    Ok(())
}

fn cmd_workspace_agents() -> Result<()> {
    use smedja_assayer::{Complexity, Role, Runner, Tier};

    let workspace_dir = std::env::current_dir().context("cannot determine working directory")?;
    let file_rules = smedja_assayer::load_rules(&workspace_dir).map_err(|e| anyhow::anyhow!(e))?;
    let mut assayer = smedja_assayer::Assayer::default_rules();
    assayer.prepend_rules(file_rules);

    println!("{:<15} {:<10} {:<8} MODEL", "ROLE", "RUNNER", "TIER");
    println!("{}", "-".repeat(55));

    for (role_name, role) in &[
        ("orchestrator", Role::Orchestrator),
        ("impl", Role::Impl),
        ("test", Role::Test),
        ("review", Role::Review),
        ("sre", Role::Sre),
    ] {
        let route = assayer.route(*role, Complexity::Coding);
        let runner = match route.runner {
            Runner::Claude => "claude",
            Runner::Local => "local",
            Runner::Codex => "codex",
            Runner::Copilot => "copilot",
        };
        let tier = match route.tier {
            Tier::Fast => "fast",
            Tier::Local => "local",
            Tier::Deep => "deep",
        };
        let model = route.model.as_deref().unwrap_or("-");
        println!("{role_name:<15} {runner:<10} {tier:<8} {model}");
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

async fn cmd_task_list(client: &mut Client, status: Option<&str>) -> Result<()> {
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

async fn cmd_task_show(client: &mut Client, id: &str) -> Result<()> {
    let resp = client
        .call("task.get", serde_json::json!({"id": id}))
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

async fn cmd_task_close(client: &mut Client, id: &str) -> Result<()> {
    client
        .call("task.close", serde_json::json!({"id": id}))
        .await
        .context("task.close failed")?;
    println!("Task {id} closed");
    Ok(())
}

async fn cmd_session_list(client: &mut Client) -> Result<()> {
    let resp = client
        .call("session.list", serde_json::Value::Null)
        .await
        .context("session.list failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn cmd_session_show(client: &mut Client, id: &str) -> Result<()> {
    let resp = client
        .call("session.get", json!({"id": id}))
        .await
        .context("session.get failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn cmd_session_rollback(client: &mut Client, session_id: &str, turn: u32) -> Result<()> {
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
