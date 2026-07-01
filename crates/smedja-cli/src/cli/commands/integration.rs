use std::path::PathBuf;

use clap::Subcommand;

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
pub(crate) enum PricesCmd {
    /// Update prices.toml from a local file or print current prices
    Update {
        /// Path to replacement prices.toml
        #[arg(long)]
        file: Option<std::path::PathBuf>,
    },
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
