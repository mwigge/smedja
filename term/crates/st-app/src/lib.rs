//! `st-app` — GPU-accelerated terminal application shell.
//!
//! Initialises the winit event loop, wgpu surface, PTY session, and block model.
//! Dispatches keyboard input to the PTY and cell-grid updates to the renderer.
//!
//! # Phase 6 — Tabs, Splits, and Multiplexer
//!
//! Key bindings added in this phase:
//! - `Ctrl+T` → open new tab
//! - `Ctrl+W` → close active tab
//! - `Ctrl+Tab` / `Ctrl+Shift+Tab` → next / prev tab
//! - `Ctrl+Shift+H` → split horizontal
//! - `Ctrl+Shift+V` → split vertical
//! - `Ctrl+Shift+Z` → toggle zoom on active pane
//! - `Ctrl+Shift+L` → open launch menu overlay
//! - `Ctrl+N` → open a new window

mod agent_bridge;
mod app;
mod clipboard;
mod commands;
mod input;
mod launch;
mod mouse;
mod render;
mod split;
mod ssh_mux;
mod status;
mod tab;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::info;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};
use crate::commands::{cmd_block_export, cmd_replay, cmd_ssh};
#[cfg(test)]
use crate::input::{encode_key, key_to_pty_bytes};
use crate::launch::load_launch_entries;
#[cfg(test)]
use crate::status::{build_window_title, tier_badge_text, truncate_cwd};
// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(name = "smedja", version, about = "GPU-accelerated terminal emulator")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Shell to spawn (defaults to `$SHELL` or `/bin/sh`).
    #[arg(long, short = 's')]
    shell: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
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
enum BlockAction {
    /// Export the output of a block to stdout.
    Export {
        /// The UUID of the block to export.
        block_id: uuid::Uuid,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run() -> anyhow::Result<()> {
    // Honour RUST_LOG, but default the GPU stack (wgpu/naga) to `warn` so a
    // `RUST_LOG=debug` capture isn't drowned by thousands of per-frame wgpu
    // lines — smedja's own debug events (input, redraw, mouse) stay readable.
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let filter = filter
        .add_directive("wgpu_core=warn".parse().expect("static directive"))
        .add_directive("wgpu_hal=warn".parse().expect("static directive"))
        .add_directive("naga=warn".parse().expect("static directive"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = Args::parse();

    // Handle non-GUI subcommands before creating the event loop.
    match args.command {
        Some(Command::Replay { block_id }) => return cmd_replay(block_id),
        Some(Command::Block {
            action: BlockAction::Export { block_id },
        }) => return cmd_block_export(block_id),
        Some(Command::Ssh { host, port }) => return cmd_ssh(host, port),
        None => {}
    }

    let config = st_config::Config::load().unwrap_or_default();

    // Default to smedja-tui so opening smedja goes straight into the agent
    // dashboard. Fall back to $SHELL for raw terminal access (smedja --shell fish).
    let shell = args.shell.unwrap_or_else(|| {
        which::which("smedja-tui").map_or_else(
            |_| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            |p| p.to_string_lossy().into_owned(),
        )
    });

    let launch_entries = load_launch_entries();
    info!("loaded {} launch menu entries", launch_entries.len());

    info!("starting smedja with shell={}", shell);

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("creating event loop")?;
    let mut app = App::new(config, shell, launch_entries);
    event_loop.run_app(&mut app).context("running event loop")?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
