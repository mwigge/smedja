//! `smj daemon` — smdjad lifecycle (start/stop/restart/status).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Subcommand;
use smedja_rpc::client::Client;

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

/// Dispatches a `smj daemon` subcommand.
pub(crate) async fn run(sock: &Path, action: DaemonCmd) -> Result<()> {
    match action {
        DaemonCmd::Status => cmd_daemon_status(sock).await?,
        DaemonCmd::Start => cmd_daemon_start()?,
        DaemonCmd::Stop => {
            let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            let pid_path = PathBuf::from(base).join("smdjad.pid");
            let pid = std::fs::read_to_string(&pid_path)
                .context("smdjad not running (no PID file)")?
                .trim()
                .to_owned();
            std::process::Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .context("kill -TERM failed")?;
            println!("smdjad stopped (pid {pid})");
        }
        DaemonCmd::Restart => {
            let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            let pid_path = PathBuf::from(base).join("smdjad.pid");
            if let Ok(pid) = std::fs::read_to_string(&pid_path).map(|s| s.trim().to_owned()) {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid])
                    .status();
                wait_for_daemon_exit(&pid, sock).context("old smdjad did not shut down cleanly")?;
            }
            cmd_daemon_start()?;
            println!("smdjad restarted");
        }
    }
    Ok(())
}

async fn cmd_daemon_status(sock: &Path) -> Result<()> {
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

/// Returns `true` if a process with the given PID is still alive.
///
/// Uses `kill -0`, which sends no signal but reports whether the PID exists
/// and is signal-able by the caller.
fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Polls until the old daemon has exited and released its socket path.
///
/// Bounded to roughly five seconds; returns an error if the process is still
/// alive or the socket still exists after the deadline so the caller does not
/// race a fresh daemon against the dying one.
fn wait_for_daemon_exit(pid: &str, sock: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if !process_alive(pid) && !sock.exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "smdjad (pid {pid}) still running or socket {} still present after 5s",
                sock.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Returns the path to the smdjad log file, creating its parent directory.
///
/// Resolves to `$XDG_STATE_HOME/smedja/smdjad.log`, falling back to
/// `$HOME/.local/state/smedja/smdjad.log`.
fn smdjad_log_path() -> PathBuf {
    let state_home = std::env::var("XDG_STATE_HOME").map_or_else(
        |_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from(".local/state"),
                |h| PathBuf::from(h).join(".local/state"),
            )
        },
        PathBuf::from,
    );
    let dir = state_home.join("smedja");
    // Best-effort: if creation fails, File::create below surfaces the error.
    let _ = std::fs::create_dir_all(&dir);
    dir.join("smdjad.log")
}

fn cmd_daemon_start() -> Result<()> {
    // Locate smdjad relative to this binary.
    let exe = std::env::current_exe().context("cannot determine own path")?;
    let smdjad = exe
        .parent()
        .map(|p| p.join("smdjad"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("smdjad"));

    // Redirect stdout/stderr to a log file so daemon output is not lost.
    let log_path = smdjad_log_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open log file {}", log_path.display()))?;
    let stderr_file = log_file
        .try_clone()
        .context("cannot duplicate log file handle for stderr")?;

    std::process::Command::new(&smdjad)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .with_context(|| format!("failed to spawn {}", smdjad.display()))?;

    println!("smdjad started");
    println!("logs: {}", log_path.display());
    Ok(())
}
