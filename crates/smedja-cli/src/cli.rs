use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::service;

mod commands;
pub(crate) use commands::*;

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
    /// Manage skill files
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
        action: service::ServiceAction,
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
    /// Internal: `PreToolUse` approval hook for the claude CLI runner. Reads the
    /// hook payload on stdin and emits a permission decision; installed via
    /// `--settings`.
    #[command(hide = true)]
    ToolGate,
}
