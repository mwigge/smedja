//! Top-level CLI definition: the `smj` parser and its command enum.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::audit::AuditCmd;
use crate::daemon::DaemonCmd;
use crate::eval::EvalCmd;
use crate::gov::GovCmd;
use crate::local::LocalCmd;
use crate::loops::LoopCmd;
use crate::mcp::McpCmd;
use crate::models::ModelsCmd;
use crate::prices::PricesCmd;
use crate::sandbox::SandboxCmd;
use crate::security::SecurityCmd;
use crate::service::ServiceAction;
use crate::session::SessionCmd;
use crate::shell::ShellCmd;
use crate::skill::SkillCmd;
use crate::task::TaskCmd;
use crate::term::TermCmd;
use crate::timeline::TimelineCmd;
use crate::workspace::WorkspaceCmd;

#[derive(Parser)]
#[command(name = "smj", version, about = "smedja control CLI")]
pub(crate) struct Cli {
    /// smdjad socket path (overrides `XDG_RUNTIME_DIR`)
    #[arg(long, env = "SMEDJA_SOCK", global = true)]
    pub(crate) sock: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
        #[command(subcommand)]
        action: AuditCmd,
    },
    /// Cost ledger
    Cost {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        since: Option<String>,
        /// Emit raw JSON instead of a formatted table
        #[arg(long)]
        json: bool,
    },
    /// Local time-tiered metrics rollups (tokens / cost / errors per runner)
    Metrics {
        /// Rollup tier: raw | hourly | daily | weekly | monthly
        #[arg(long, default_value = "daily")]
        tier: String,
        /// Lower bound: a duration back from now (`7d`, `24h`, `30m`, `90s`) or bare seconds
        #[arg(long, default_value = "7d")]
        since: String,
        /// Optional exclusive upper bound, same format as `--since`
        #[arg(long)]
        until: Option<String>,
        /// Show only this runner
        #[arg(long)]
        runner: Option<String>,
        /// Emit the raw `metrics.summary` JSON response
        #[arg(long)]
        json: bool,
    },
    /// Token-economy savings rollup (tokens saved per source + efficiency ratio)
    Savings {
        /// Rollup tier: raw | hourly | daily | weekly | monthly
        #[arg(long, default_value = "daily")]
        tier: String,
        /// Lower bound: a duration back from now (`7d`, `24h`, `30m`, `90s`) or bare seconds
        #[arg(long, default_value = "7d")]
        since: String,
        /// Optional exclusive upper bound, same format as `--since`
        #[arg(long)]
        until: Option<String>,
        /// Emit the raw `savings.summary` JSON response
        #[arg(long)]
        json: bool,
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
    /// MCP server registry
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// Docker sandbox management
    Sandbox {
        #[command(subcommand)]
        action: SandboxCmd,
    },
    /// Manage model pricing
    Prices {
        #[command(subcommand)]
        action: PricesCmd,
    },
    /// smedja utilities
    Term {
        #[command(subcommand)]
        action: TermCmd,
    },
    /// Conversation timeline inspection (local Agent Timeline view)
    Timeline {
        #[command(subcommand)]
        action: TimelineCmd,
    },
    /// Manage smdjad as a system service (launchd / systemd)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Security plane: posture scan, findings report, SBOM (advisory by default)
    Security {
        #[command(subcommand)]
        action: SecurityCmd,
    },
    /// Eval harness: run a case suite and gate on its pass-rate threshold
    Eval {
        #[command(subcommand)]
        action: EvalCmd,
    },
    /// Local-model management: install, list, GPU inspect, and hot-swap
    Local {
        #[command(subcommand)]
        action: LocalCmd,
    },
    /// Governance artifact management (WIs, RFCs, ADRs).
    Gov {
        #[command(subcommand)]
        action: GovCmd,
    },
    /// Provider health check — shows which runners are active, their kind
    /// (native HTTP vs subprocess CLI), and environment variable status.
    Doctor {
        /// Emit raw JSON instead of a formatted table.
        #[arg(long)]
        json: bool,
    },
    /// Model catalog — list or inspect configured provider models.
    Models {
        #[command(subcommand)]
        action: ModelsCmd,
    },
    /// Shell integration — inject precmd/postcmd hooks into shell config files.
    Shell {
        #[command(subcommand)]
        action: ShellCmd,
    },
    /// Check for or install a newer smj release.
    Upgrade {
        /// Only check whether a newer version is available; do not install.
        #[arg(long)]
        check: bool,
    },
    /// Internal: Claude Code `PreToolUse` approval hook. Reads the hook payload on
    /// stdin and emits a permission decision; installed via `--settings`.
    #[command(hide = true)]
    ToolGate,
}
