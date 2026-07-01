use std::path::PathBuf;

use clap::Subcommand;

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
pub(crate) enum SandboxCmd {
    /// Build the smedja-sandbox Docker image
    Build,
    /// Report the selected backend, its availability, the network policy, and
    /// the fallback mode
    Status,
}
