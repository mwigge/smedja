//! Command-line interface definitions and non-GUI subcommand handlers.
//!
//! Holds the `clap` argument structs plus the `replay`, `block export`, and
//! `ssh` subcommand implementations that run without spawning the event loop.

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::info;

use crate::ssh_mux;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(name = "smedja", version, about = "GPU-accelerated terminal emulator")]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    /// Shell to spawn (defaults to `$SHELL` or `/bin/sh`).
    #[arg(long, short = 's')]
    pub(crate) shell: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Replay the command from a block by its UUID.
    Replay {
        /// The UUID of the block to replay.
        block_id: uuid::Uuid,
    },
    /// Block management commands.
    Block {
        #[command(subcommand)]
        action: BlockAction,
    },
    /// Connect to a remote host via SSH and forward the smdjad socket.
    Ssh {
        /// Remote host, optionally prefixed with `user@`.
        host: String,
        /// SSH port.
        #[arg(long, default_value = "22")]
        port: u16,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum BlockAction {
    /// Export the output of a block to stdout.
    Export {
        /// The UUID of the block to export.
        block_id: uuid::Uuid,
    },
}

// ── Subcommand handlers ────────────────────────────────────────────────────────

pub(crate) fn cmd_replay(block_id: uuid::Uuid) -> anyhow::Result<()> {
    let db_path = default_db_path();
    let store = st_blocks::BlockStore::new(&db_path)
        .with_context(|| format!("opening block store at {}", db_path.display()))?;
    let block = store
        .get(&block_id)?
        .with_context(|| format!("block {block_id} not found"))?;
    let cmd = block.cmd.as_deref().unwrap_or("echo 'no command'");
    info!("replaying: {}", cmd);
    // Spawn a PTY and re-run the command so its output can be observed.
    let mut pty = st_pty::PtySession::spawn(80, 24, cmd).with_context(|| "spawning replay PTY")?;
    pty.write_input(b"\r")?;
    // Let it run briefly then exit.
    std::thread::sleep(std::time::Duration::from_secs(2));
    Ok(())
}

pub(crate) fn cmd_block_export(block_id: uuid::Uuid) -> anyhow::Result<()> {
    let db_path = default_db_path();
    let store = st_blocks::BlockStore::new(&db_path)
        .with_context(|| format!("opening block store at {}", db_path.display()))?;
    let output = store
        .get_output(&block_id)?
        .with_context(|| format!("block {block_id} not found"))?;
    print!("{output}");
    Ok(())
}

/// Connects via SSH and forwards the remote smdjad socket locally.
pub(crate) fn cmd_ssh(host: String, port: u16) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new().context("creating Tokio runtime")?;
    rt.block_on(async move {
        let (username, hostname) = ssh_mux::parse_host_user(&host);
        let client = ssh_mux::connect(&hostname, port, &username).await?;
        client.ensure_mux_daemon().await?;

        let local_sock = std::env::temp_dir().join("smedja-mux.sock");
        client.open_local_tunnel(&local_sock)?;
        info!(
            socket = %local_sock.display(),
            "tunnel active — Ctrl-C to exit"
        );
        tokio::signal::ctrl_c()
            .await
            .context("waiting for Ctrl-C")?;
        Ok(())
    })
}

fn default_db_path() -> std::path::PathBuf {
    // XDG data directory or HOME fallback.
    let base = std::env::var("XDG_DATA_HOME").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(home).join(".local").join("share")
        },
        std::path::PathBuf::from,
    );
    base.join("smedja").join("blocks.db")
}
