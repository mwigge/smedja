use clap::Subcommand;

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
