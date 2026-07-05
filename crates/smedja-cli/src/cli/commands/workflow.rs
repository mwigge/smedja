use clap::Subcommand;

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
    /// List checkpoints for a session
    Checkpoint {
        id: String,
    },
    /// Export a session: checkpointed turns + audit events (json) or a transcript (md)
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
    /// Print the resolved role->runner->tier->model table for the current workspace
    Show,
    /// Generate a starter .smedja/agents.toml in the current directory
    Init,
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
