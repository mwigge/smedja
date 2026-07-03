//! `smedja` — GPU-accelerated terminal emulator entry point.
//!
//! Initialises the winit event loop, wgpu surface, PTY session, and block model.
//! Dispatches keyboard input to the PTY and cell-grid updates to the renderer.
//!
//! The event-loop state and its handlers are split across sibling modules:
//! [`app`] (state + helpers), [`handler`] (the `ApplicationHandler` lifecycle),
//! [`redraw`] (per-frame rendering), [`input_events`] (keyboard/mouse arms),
//! [`input`] (key/mouse encoding), [`render`] (rendering glue), [`config`]
//! (launch-menu config), [`agent_bridge`] (smdjad bridge), and [`cli`]
//! (argument parsing + non-GUI subcommands).
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
mod cli;
mod config;
mod handler;
mod input;
mod input_events;
mod redraw;
mod render;
mod split;
mod ssh_mux;
mod tab;

use anyhow::Context;
use clap::Parser;
use tracing::info;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};
use crate::cli::{Args, BlockAction, Command};
use crate::config::load_launch_entries;

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
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
        Some(Command::Replay { block_id }) => return cli::cmd_replay(block_id),
        Some(Command::Block {
            action: BlockAction::Export { block_id },
        }) => return cli::cmd_block_export(block_id),
        Some(Command::Ssh { host, port }) => return cli::cmd_ssh(host, port),
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
