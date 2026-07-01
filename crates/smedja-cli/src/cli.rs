use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::service;

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
    /// Internal: Claude Code `PreToolUse` approval hook. Reads the hook payload on
    /// stdin and emits a permission decision; installed via `--settings`.
    #[command(hide = true)]
    ToolGate,
}

#[derive(Subcommand)]
pub(crate) enum GovCmd {
    /// List governance artifacts.
    List {
        /// Filter by kind: wi, rfc, adr (default: all).
        #[arg(long)]
        kind: Option<String>,
    },
    /// Transition an artifact to a new status.
    Transition {
        /// Artifact ID (e.g. WI-003).
        id: String,
        /// New status: `planned`, `in_progress`, `done`, `cancelled`.
        status: String,
    },
    /// Create a new work item.
    Create {
        /// Work item title.
        title: String,
        /// Optional description.
        #[arg(long)]
        description: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum LocalCmd {
    /// List the local-model inventory with GPU fit annotations
    List {
        /// Emit the raw `local.models` JSON response
        #[arg(long)]
        json: bool,
    },
    /// Show the cached GPU snapshot
    Gpu {
        /// Emit the raw `local.gpu` JSON response
        #[arg(long)]
        json: bool,
    },
    /// Hot-swap the active local model (no daemon restart)
    Swap {
        /// The model id to make active
        model: String,
    },
    /// Install a local model via the external installer (rs-llmctl)
    Install {
        /// The model id to install
        model: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum EvalCmd {
    /// Load a suite directory, run it, print a report, and gate on the threshold
    Run {
        /// Path to the suite directory (contains `suite.toml` and case files)
        #[arg(long)]
        suite: PathBuf,
        /// Run graded (rubric / live-driver) cases instead of skipping them
        #[arg(long)]
        online: bool,
        /// Write the machine-readable JSON summary to stdout
        #[arg(long)]
        json: bool,
        /// Override the suite's configured pass-rate threshold (in [0.0, 1.0])
        #[arg(long)]
        threshold: Option<f64>,
    },
}

#[derive(Subcommand)]
pub(crate) enum SecurityCmd {
    /// Run a workspace posture scan and print the advisory findings
    Scan {
        /// Workspace directory to scan (defaults to the current directory)
        path: Option<PathBuf>,
    },
    /// Summarise recorded `security_finding` audit events (read-only query)
    Report,
    /// Emit a CycloneDX-style SBOM from the resolved Cargo.lock to stdout
    Sbom {
        /// Path to the Cargo.lock to read (defaults to ./Cargo.lock)
        #[arg(long)]
        lockfile: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub(crate) enum DaemonCmd {
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
pub(crate) enum AuditCmd {
    /// Run a read-only repo/PR/branch audit and write a markdown report
    Run {
        /// Path or whole-repo scope (omit for the working-tree diff)
        path: Option<String>,
        /// Audit a branch range against this base (`<base>...HEAD`)
        #[arg(long)]
        branch: Option<String>,
        /// Audit a pull-request reference resolved to a branch range
        #[arg(long)]
        pr: Option<String>,
        /// Force the working-tree diff scope (`git diff HEAD`)
        #[arg(long)]
        diff: bool,
        /// Write the markdown report to this path instead of stdout
        #[arg(long)]
        report: Option<String>,
        /// Output format: `md` (default) or `json`
        #[arg(long, default_value = "md")]
        format: String,
    },
    /// Query audit log events
    Query {
        /// Filter by session ID
        #[arg(long)]
        session: Option<String>,
        /// Filter by relative duration (e.g. "1h", "24h")
        #[arg(long)]
        since: Option<String>,
        /// Filter by action type
        #[arg(long)]
        action: Option<String>,
    },
    /// Show prompt hash records for a change
    PromptDiff {
        /// Name of the `OpenSpec` change to inspect
        #[arg(long)]
        change: String,
    },
    /// Show which role produced which action type for a session
    Who {
        /// Session ID to inspect
        #[arg(long)]
        session: String,
    },
    /// Export audit events for a change to stdout
    Export {
        /// Name of the `OpenSpec` change to export
        #[arg(long)]
        change: String,
        /// Output format: `jsonl` or `csv`
        #[arg(long, default_value = "jsonl")]
        format: String,
        /// Include prompt hashes as an additional column
        #[arg(long)]
        include_prompts: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum SessionCmd {
    /// Start a new session
    Start {
        /// Enable cowork mode (human approval for each tool call)
        #[arg(long)]
        cowork: bool,
        /// Create a task linked to this session
        #[arg(long)]
        task: Option<String>,
    },
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
    /// List stored blocks for a session
    Blocks {
        id: String,
    },
    /// List checkpoints for a session
    Checkpoint {
        id: String,
    },
    /// Export session cost lineage or messages
    Export {
        /// Session ID to export
        id: String,
        /// Output format: json (default) or md
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// Compact session conversation history
    Compact {
        /// Session ID to compact
        id: String,
    },
    /// Show per-turn token usage for a session
    Tokens {
        /// Session ID to query
        id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum WorkspaceCmd {
    /// Agent management
    Agents {
        #[command(subcommand)]
        action: AgentsCmd,
    },
    /// Initialise a workspace: create .smedja/, index symbols, and write workspace.toml
    Init {
        /// Directory to initialise (defaults to current directory)
        path: Option<std::path::PathBuf>,
    },
    /// Index the current workspace into the code graph
    Index {
        /// Optional git commit SHA for incremental re-indexing
        #[arg(long)]
        commit_sha: Option<String>,
    },
    /// Register a directory path with the workspace
    Add {
        /// Directory path to add to the workspace
        path: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum AgentsCmd {
    /// Print the resolved role→runner→tier→model table for the current workspace
    Show,
    /// Generate a starter .smedja/agents.toml in the current directory
    Init,
}

#[derive(Subcommand)]
pub(crate) enum PricesCmd {
    /// Update prices.toml from a local file or print current prices
    Update {
        /// Path to replacement prices.toml
        #[arg(long)]
        file: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
pub(crate) enum SkillCmd {
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
    /// Create .codex/skills and .cursor/skills symlinks pointing to ~/.claude/skills
    LinkIdes {
        /// Project directory to link into (default: current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
pub(crate) enum TaskCmd {
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
    /// Start a parallel task across multiple agent roles
    Parallel {
        /// Goal description passed to all roles
        goal: String,
        /// Comma-separated roles: impl,test,review
        #[arg(long, value_delimiter = ',')]
        roles: Vec<String>,
    },
    /// Show per-role status of a parallel task
    Status {
        /// Parallel task ID returned by `smj task parallel`
        id: String,
    },
    /// Cancel a running parallel task
    Cancel {
        /// Parallel task ID to cancel
        id: String,
    },
    /// Export tasks (and their audit events) as JSONL to stdout
    Export {
        /// Filter to tasks whose title contains this change name
        #[arg(long)]
        change: Option<String>,
    },
    /// Import tasks and audit events from JSONL on stdin
    Import,
}

#[derive(Subcommand)]
pub(crate) enum LoopCmd {
    /// Run a loop against an `OpenSpec` change
    Run {
        /// Name of the `OpenSpec` change to drive
        #[arg(long)]
        change: String,
        /// Maximum number of task slices to process
        #[arg(long, default_value = "10")]
        max_slices: u32,
        /// Stream loop progress events to stdout (default true; use --no-follow to detach).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        follow: bool,
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
    /// Retire a completed or failed loop
    Retire {
        /// Name of the `OpenSpec` change whose loop to retire
        #[arg(long)]
        change: String,
    },
    /// List loops, optionally filtered by status
    List {
        /// Filter by loop status (e.g. `complete`, `failed`, `retired`)
        #[arg(long)]
        status: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum McpCmd {
    /// Register an MCP server
    Add {
        name: String,
        url: String,
        #[arg(long)]
        stdio: Option<String>,
    },
    /// List registered MCP servers
    List,
    /// Remove an MCP server by name
    Remove { name: String },
    /// Re-fetch tool lists from registered servers
    Refresh {
        /// Refresh a specific server only (omit for all)
        name: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum SandboxCmd {
    /// Build the smedja-sandbox Docker image
    Build,
    /// Report the selected backend, its availability, the network policy, and
    /// the fallback mode
    Status,
}

#[derive(Subcommand)]
pub(crate) enum TermCmd {
    /// Download and install smedja to ~/.local/bin
    Install {
        /// URL to download the binary from (auto-detected by OS if omitted)
        #[arg(long)]
        bin_path: Option<String>,
        /// Installation prefix directory (default: ~/.local/bin)
        #[arg(long)]
        prefix: Option<PathBuf>,
    },
    /// Convert a `WezTerm` configuration to smedja terminal integration format.
    ConvertWezterm,
}

#[derive(Subcommand)]
pub(crate) enum TimelineCmd {
    /// List recent conversations with rollup statistics
    Conversations {
        /// Only show conversations from the last N seconds (e.g. 3600 for last hour)
        #[arg(long)]
        since: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show ordered timeline events for a conversation
    Show {
        /// Conversation ID
        conversation_id: String,
        /// Only show failure events
        #[arg(long)]
        failures_only: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Open a conversation in a configured backend (`Honeycomb`, `SigNoz`, etc.)
    Open {
        /// Conversation ID, trace ID, or span ID
        id: String,
    },
}

/// Operator-facing view of the active sandbox configuration.
///
/// Mirrors the daemon's backend-selection precedence (Docker when opted in and
/// reachable → the current platform's OS-native backend → none) and reads the
/// same environment contract (`SMEDJA_SANDBOX_MODE`, `SMEDJA_SANDBOX_NETWORK`,
/// legacy `SMEDJA_TOOL_SANDBOX=docker`) so `smj sandbox status` reports what the
/// daemon would select.
pub(crate) struct SandboxStatus {
    pub(crate) backend: &'static str,
    pub(crate) available: bool,
    pub(crate) network_policy: &'static str,
    pub(crate) mode: &'static str,
}

impl SandboxStatus {
    pub(crate) fn detect() -> Self {
        let legacy_docker = std::env::var("SMEDJA_TOOL_SANDBOX").is_ok_and(|v| v == "docker");
        let mode = if legacy_docker {
            "auto"
        } else {
            Self::mode_from_env()
        };
        let network_policy = Self::network_from_env();

        if mode == "off" {
            return Self {
                backend: "none",
                available: false,
                network_policy,
                mode,
            };
        }

        let docker_opt_in = legacy_docker || which::which("docker").is_ok();
        let docker_avail = docker_opt_in && Self::docker_image_ok();
        let (backend, available) = if docker_avail {
            ("docker", true)
        } else if cfg!(target_os = "macos") {
            ("seatbelt", which::which("sandbox-exec").is_ok())
        } else if cfg!(target_os = "linux") {
            // Landlock availability is a kernel property the CLI cannot probe
            // without the daemon; report the native backend name and defer the
            // definitive availability to the daemon's own detection.
            ("landlock", true)
        } else {
            ("none", false)
        };

        Self {
            backend,
            available,
            network_policy,
            mode,
        }
    }

    fn mode_from_env() -> &'static str {
        Self::mode_from_value(&std::env::var("SMEDJA_SANDBOX_MODE").unwrap_or_default())
    }

    pub(crate) fn mode_from_value(value: &str) -> &'static str {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => "required",
            "off" => "off",
            _ => "auto",
        }
    }

    fn network_from_env() -> &'static str {
        Self::network_from_value(&std::env::var("SMEDJA_SANDBOX_NETWORK").unwrap_or_default())
    }

    pub(crate) fn network_from_value(value: &str) -> &'static str {
        match value.trim().to_ascii_lowercase().as_str() {
            "allowlist" => "allowlist",
            "open" => "open",
            _ => "none",
        }
    }

    fn docker_image_ok() -> bool {
        if which::which("docker").is_err() {
            return false;
        }
        let image = std::env::var("SMEDJA_SANDBOX_IMAGE")
            .unwrap_or_else(|_| "smedja-sandbox:latest".to_owned());
        std::process::Command::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}", &image])
            .output()
            .is_ok_and(|o| o.status.success())
    }
}
