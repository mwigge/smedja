//! `smj service` — install/manage smdjad as a system service.
//!
//! Supports launchd on macOS and systemd --user on Linux.

use anyhow::Result;
use clap::Subcommand;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[derive(Subcommand, Debug)]
pub enum ServiceAction {
    /// Write unit file and start service immediately
    Install,
    /// Stop, disable, and remove the unit file
    Uninstall,
    /// Show running state, PID, and socket path
    Status,
    /// Tail recent log output
    Logs,
    /// Restart the running service
    Restart,
}

/// Dispatch a service action on the current platform.
///
/// # Errors
///
/// Returns an error if the platform is unsupported, the binary cannot be
/// located on PATH, or any sub-process invocation fails.
pub fn run(action: &ServiceAction) -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::dispatch(action);

    #[cfg(target_os = "linux")]
    return linux::dispatch(action);

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = action;
        anyhow::bail!("`smj service` is only supported on macOS and Linux")
    }
}
