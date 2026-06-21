//! `smedja-term` — GPU-accelerated terminal emulator entry point.
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

mod split;
mod ssh_mux;
mod tab;

use std::collections::HashMap;
use std::sync::{atomic::Ordering, Arc};

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::{debug, error, info};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, Modifiers, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

use crate::split::{SplitDirection, SplitLayout};
use crate::tab::TabBar;

use st_agent::SharedPaneState;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(name = "smedja-term", about = "GPU-accelerated terminal emulator")]
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

// ── User events (sent from async tasks to the event loop) ────────────────────

/// Events that async background tasks can post into the winit event loop.
#[allow(dead_code)] // variants are constructed via EventLoopProxy from background tasks
#[derive(Debug)]
enum UserEvent {
    /// Request that a new terminal window be opened.
    OpenWindow,
}

// ── Launch menu entry ─────────────────────────────────────────────────────────

/// A single entry in the launch menu, loaded from `[[launch_menu]]` in
/// `~/.config/smedja-term/config.toml`.
#[derive(Debug, Clone)]
pub struct LaunchEntry {
    /// Display label shown in the overlay.
    pub label: String,
    /// Command to execute in a new pane.
    pub command: String,
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Application state threaded through the winit event loop.
///
/// `PtySession` is owned directly (not behind `Arc`) because the event loop
/// runs on the main thread and the PTY reader thread only accesses the session
/// through the cloned `Arc<Mutex<CellGrid>>` and `Arc<AtomicBool>` that are
/// fields of `PtySession` — not through the session itself.
struct App {
    /// All open windows, keyed by `WindowId`.
    windows: HashMap<WindowId, Arc<Window>>,
    renderer: Option<st_render::Renderer>,
    pty: Option<st_pty::PtySession>,
    config: st_config::Config,
    shell: String,
    /// Tab bar — owns all tabs and the active tab index.
    tab_bar: TabBar,
    /// Per-tab split layout.  Keyed by tab index (positional, not UUID) for
    /// simplicity; rebuilt when tabs are opened or closed.
    split_layouts: Vec<SplitLayout>,
    /// Current keyboard modifier state.
    modifiers: Modifiers,
    /// Launch menu entries loaded from config.
    launch_entries: Vec<LaunchEntry>,
    /// Whether the launch menu overlay is currently visible.
    launch_menu_open: bool,
    /// Selected entry index in the launch menu overlay.
    launch_menu_selection: usize,
    /// Live agent state fed from the st-agent UDS listener.
    pane_state: SharedPaneState,
}

impl App {
    fn new(config: st_config::Config, shell: String, launch_entries: Vec<LaunchEntry>) -> Self {
        // Initialise with one tab and a split layout for its root pane.
        let tab_bar = TabBar::new();
        let root_pane_id = tab_bar.tabs[0].panes[0].id;
        let split_layouts = vec![SplitLayout::new(root_pane_id)];

        Self {
            windows: HashMap::new(),
            renderer: None,
            pty: None,
            config,
            shell,
            tab_bar,
            split_layouts,
            modifiers: Modifiers::default(),
            launch_entries,
            launch_menu_open: false,
            launch_menu_selection: 0,
            pane_state: SharedPaneState::new(),
        }
    }

    // ── Window helpers ────────────────────────────────────────────────────────

    /// Opens a new terminal window and registers it in `self.windows`.
    fn open_window(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("smedja-term")
            .with_inner_size(winit::dpi::LogicalSize::new(1200u32, 800u32));

        match event_loop.create_window(attrs) {
            Ok(w) => {
                let w = Arc::new(w);
                info!("opened window {:?}", w.id());
                self.windows.insert(w.id(), w);
            }
            Err(e) => error!("failed to create window: {}", e),
        }
    }

    // ── Tab helpers ───────────────────────────────────────────────────────────

    /// Opens a new tab, creating a matching split layout for it.
    fn open_tab(&mut self) {
        let count = self.tab_bar.tabs.len() + 1;
        let new_tab = self.tab_bar.open_tab(count.to_string());
        let root_pane_id = new_tab.panes[0].id;
        self.split_layouts.push(SplitLayout::new(root_pane_id));
        info!("opened tab {}", count);
    }

    /// Closes the active tab, also removing its split layout.
    fn close_active_tab(&mut self) {
        let idx = self.tab_bar.active;
        if self.tab_bar.tabs.len() <= 1 {
            // Never close the last tab.
            return;
        }
        self.tab_bar.close_tab(idx);
        if idx < self.split_layouts.len() {
            self.split_layouts.remove(idx);
        }
        info!("closed tab {}", idx);
    }

    /// Splits the active pane in the active tab.
    fn split_active_pane(&mut self, dir: SplitDirection) {
        // Collect the IDs we need while holding the tab borrow, then release it.
        let ids = {
            let Some(tab) = self.tab_bar.active_tab_mut() else {
                return;
            };
            let Some(active_pane) = tab.active_pane() else {
                return;
            };
            let existing_id = active_pane.id;
            let new_idx = tab.push_pane();
            let new_id = tab.panes[new_idx].id;
            tab.active_pane = new_idx;
            (existing_id, new_id)
        }; // borrow of tab_bar ends here

        let (existing_id, new_id) = ids;
        let active_tab_idx = self.tab_bar.active;
        if let Some(layout) = self.split_layouts.get_mut(active_tab_idx) {
            if let Err(e) = layout.split(existing_id, dir, new_id) {
                error!("split layout error: {}", e);
            }
        }
        info!("split {:?}", dir);
    }

    /// Toggles zoom on the active pane of the active tab.
    fn toggle_zoom(&mut self) {
        if let Some(tab) = self.tab_bar.active_tab_mut() {
            tab.toggle_zoom();
        }
    }

    /// Opens or closes the launch menu overlay.
    fn toggle_launch_menu(&mut self) {
        self.launch_menu_open = !self.launch_menu_open;
        self.launch_menu_selection = 0;
        info!(
            "launch menu {}",
            if self.launch_menu_open {
                "open"
            } else {
                "closed"
            }
        );
    }

    /// Activates the currently selected launch menu entry.
    fn activate_launch_entry(&mut self) {
        if !self.launch_menu_open {
            return;
        }
        // Collect the command string before splitting (borrow ends after the block).
        let launch_cmd = self
            .launch_entries
            .get(self.launch_menu_selection)
            .map(|e| e.command.clone());

        if let Some(launch_cmd) = launch_cmd {
            info!("launch: {}", launch_cmd);
            // Split the active pane horizontally to host the new command.
            self.split_active_pane(SplitDirection::Horizontal);
        }
        self.launch_menu_open = false;
    }

    // ── Modifier helpers ──────────────────────────────────────────────────────

    fn ctrl(&self) -> bool {
        self.modifiers.state().control_key()
    }

    fn shift(&self) -> bool {
        self.modifiers.state().shift_key()
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Create the first window on initial resume.
        if !self.windows.is_empty() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("smedja-term")
            .with_inner_size(winit::dpi::LogicalSize::new(1200u32, 800u32));

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!("failed to create window: {}", e);
                event_loop.exit();
                return;
            }
        };

        // Initialise wgpu renderer — this blocks briefly; in production we'd
        // do this async but pollster makes it tractable here.
        let renderer =
            match pollster::block_on(st_render::Renderer::new(Arc::clone(&window), &self.config)) {
                Ok(r) => r,
                Err(e) => {
                    // ponytail: on headless CI wgpu will fail — log and continue
                    // without a renderer so the process at least starts cleanly.
                    error!("renderer init failed (headless CI?): {}", e);
                    self.windows.insert(window.id(), window);
                    return;
                }
            };

        // Compute initial grid size from window dimensions and font metrics.
        // Reserve the status bar height from the usable area so the terminal
        // grid never draws into the bottom strip.
        let size = window.inner_size();
        let sb_h = status_bar_height_for_font(self.config.font.size);
        let grid_h = size.height.saturating_sub(sb_h);
        let (cols, rows) = st_glyph::pixel_size_to_grid(size.width, grid_h, self.config.font.size);

        // Each pane gets a stable UUID injected as SMEDJA_TERM_PANE so smdjad
        // can route agent events back to the correct window.
        let pane_id = self.tab_bar.tabs[0].panes[0].id;
        let pane_id_str = pane_id.to_string();

        // Spawn PTY session with the pane env var so the shell (and smdjad
        // child processes) inherit it.
        let mut pty = match st_pty::PtySession::spawn_with_env(
            cols,
            rows,
            &self.shell,
            &[("SMEDJA_TERM_PANE", &pane_id_str)],
        ) {
            Ok(p) => p,
            Err(e) => {
                error!("PTY spawn failed: {}", e);
                self.windows.insert(window.id(), window);
                self.renderer = Some(renderer);
                return;
            }
        };
        pty.start_reader_detached();

        spawn_agent_bridge(self.pane_state.clone(), pane_id_str);

        self.windows.insert(window.id(), window);
        self.renderer = Some(renderer);
        self.pty = Some(pty);
        info!("smedja-term initialised (pane {pane_id})");
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::OpenWindow => self.open_window(event_loop),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested for {:?}", window_id);
                self.windows.remove(&window_id);
                if self.windows.is_empty() {
                    event_loop.exit();
                }
            }

            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods;
            }

            WindowEvent::Resized(new_size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(new_size);
                }
                if let Some(pty) = &mut self.pty {
                    // Use grid_height_px() from the renderer when available;
                    // fall back to the same formula so PTY stays out of the
                    // status bar strip.
                    let grid_h = self.renderer.as_ref().map_or_else(
                        || {
                            new_size
                                .height
                                .saturating_sub(status_bar_height_for_font(self.config.font.size))
                        },
                        st_render::Renderer::grid_height_px,
                    );
                    let (cols, rows) =
                        st_glyph::pixel_size_to_grid(new_size.width, grid_h, self.config.font.size);
                    // Resize errors are non-fatal; the PTY may have exited.
                    if let Err(e) = pty.resize(cols, rows) {
                        debug!("PTY resize error: {}", e);
                    }
                }
            }

            WindowEvent::RedrawRequested => {
                // If the PTY has new data, update the renderer cells.
                if let (Some(pty), Some(renderer)) = (&self.pty, &mut self.renderer) {
                    if pty.dirty.load(Ordering::Acquire) {
                        pty.dirty.store(false, Ordering::Release);
                        let grid = pty.grid.lock();
                        let cells: Vec<st_render::Cell> = grid
                            .cells
                            .iter()
                            .flat_map(|row| {
                                row.iter().map(|c| st_render::Cell {
                                    ch: c.ch,
                                    fg: c.fg,
                                    bg: c.bg,
                                    col: c.col,
                                    row: c.row,
                                })
                            })
                            .collect();
                        drop(grid);
                        renderer.update_cells(&cells);
                    }

                    // Evaluate status bar modules and update the renderer.
                    // The modules run in parallel (rayon + per-module threads)
                    // within an 8 ms budget.  Live agent state comes from the
                    // st-agent bridge running in its own thread.
                    let (tier, model, active_task) = {
                        // Non-blocking try_read: if the lock is contended (agent
                        // event writing) skip the update this frame.
                        if let Ok(s) = self.pane_state.0.try_read() {
                            (s.tier.clone(), s.model.clone(), s.active_task.clone())
                        } else {
                            (None, None, None)
                        }
                    };
                    let sb_ctx = st_statusbar::ModuleContext {
                        tier,
                        model,
                        context_used: 0,
                        context_window: 0,
                        active_task,
                    };
                    let sb_modules: Vec<Box<dyn st_statusbar::StatusModule>> = vec![
                        Box::new(st_statusbar::TierModule),
                        Box::new(st_statusbar::ModelModule),
                        Box::new(st_statusbar::GitBranchModule),
                        Box::new(st_statusbar::TimeModule),
                    ];
                    let segments =
                        st_statusbar::render_status_bar_parallel(&sb_modules, &sb_ctx, 8);
                    renderer.set_status_bar_segments(&segments);

                    if let Err(e) = renderer.render() {
                        debug!("render error: {}", e);
                    }
                }

                // Request another frame for each open window.
                for w in self.windows.values() {
                    w.request_redraw();
                }
            }

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                // ── Phase 6 multiplexer bindings ──────────────────────────────
                //
                // These are checked before passing input to the PTY so the
                // terminal application never sees the control sequences.

                if self.launch_menu_open {
                    // When the launch menu is visible, intercept navigation keys.
                    match &logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.launch_menu_open = false;
                            return;
                        }
                        Key::Named(NamedKey::Enter) => {
                            self.activate_launch_entry();
                            return;
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if !self.launch_entries.is_empty() {
                                self.launch_menu_selection =
                                    (self.launch_menu_selection + 1) % self.launch_entries.len();
                            }
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if !self.launch_entries.is_empty() {
                                self.launch_menu_selection = self
                                    .launch_menu_selection
                                    .checked_sub(1)
                                    .unwrap_or(self.launch_entries.len() - 1);
                            }
                            return;
                        }
                        _ => {}
                    }
                }

                // Ctrl+N → open new window
                if self.ctrl() && !self.shift() {
                    if let Key::Character(s) = &logical_key {
                        if s.to_lowercase() == "n" {
                            self.open_window(event_loop);
                            return;
                        }
                    }
                }

                // Ctrl+T → open new tab
                if self.ctrl() && !self.shift() {
                    if let Key::Character(s) = &logical_key {
                        if s.to_lowercase() == "t" {
                            self.open_tab();
                            return;
                        }
                        // Ctrl+W → close active tab
                        if s.to_lowercase() == "w" {
                            self.close_active_tab();
                            return;
                        }
                    }
                    // Ctrl+Tab → next tab
                    if logical_key == Key::Named(NamedKey::Tab) {
                        self.tab_bar.next_tab();
                        return;
                    }
                }

                // Ctrl+Shift+Tab → prev tab
                if self.ctrl() && self.shift() {
                    if logical_key == Key::Named(NamedKey::Tab) {
                        self.tab_bar.prev_tab();
                        return;
                    }
                    if let Key::Character(s) = &logical_key {
                        match s.to_lowercase().as_str() {
                            // Ctrl+Shift+H → horizontal split
                            "h" => {
                                self.split_active_pane(SplitDirection::Horizontal);
                                return;
                            }
                            // Ctrl+Shift+V → vertical split
                            "v" => {
                                self.split_active_pane(SplitDirection::Vertical);
                                return;
                            }
                            // Ctrl+Shift+Z → toggle zoom
                            "z" => {
                                self.toggle_zoom();
                                return;
                            }
                            // Ctrl+Shift+L → launch menu
                            "l" => {
                                self.toggle_launch_menu();
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                // ── Pass remaining input to the PTY ───────────────────────────
                if let Some(pty) = &mut self.pty {
                    let bytes: Option<Vec<u8>> = match &logical_key {
                        Key::Character(s) => Some(s.as_str().as_bytes().to_vec()),
                        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
                        Key::Named(NamedKey::Backspace) => Some(b"\x7f".to_vec()),
                        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
                        Key::Named(NamedKey::Escape) => Some(b"\x1b".to_vec()),
                        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
                        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
                        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
                        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
                        _ => None,
                    };
                    if let Some(data) = bytes {
                        // Write errors are non-fatal; PTY may have exited.
                        if let Err(e) = pty.write_input(&data) {
                            debug!("PTY write error: {}", e);
                        }
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Request a redraw every frame — the renderer will throttle via vsync.
        for w in self.windows.values() {
            w.request_redraw();
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the status bar height in pixels for a given font size.
///
/// Mirrors the formula in `st_render::Renderer::status_bar_height_px` so that
/// callers without a renderer can compute the same value.
fn status_bar_height_for_font(font_size: f32) -> u32 {
    // Clamp to zero before truncating so negative font sizes don't wrap.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let px = font_size.max(0.0) as u32;
    px.min(18)
}

// ── Subcommand handlers ────────────────────────────────────────────────────────

fn cmd_replay(block_id: uuid::Uuid) -> anyhow::Result<()> {
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

fn cmd_block_export(block_id: uuid::Uuid) -> anyhow::Result<()> {
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
fn cmd_ssh(host: String, port: u16) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new().context("creating Tokio runtime")?;
    rt.block_on(async move {
        let (username, hostname) = ssh_mux::parse_host_user(&host);
        let client = ssh_mux::connect(&hostname, port, &username).await?;
        client.ensure_mux_daemon().await?;

        let local_sock = std::env::temp_dir().join("smedja-term-mux.sock");
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
    base.join("smedja-term").join("blocks.db")
}

// ── Launch menu config loading ─────────────────────────────────────────────────

/// Loads `[[launch_menu]]` entries from the smedja-term config file.
///
/// The TOML format is:
/// ```toml
/// [[launch_menu]]
/// label   = "htop"
/// command = "htop"
///
/// [[launch_menu]]
/// label   = "neovim"
/// command = "nvim"
/// ```
///
/// Returns an empty `Vec` when the file is absent or the section is missing.
fn load_launch_entries() -> Vec<LaunchEntry> {
    #[derive(serde::Deserialize)]
    struct RawEntry {
        label: String,
        command: String,
    }

    #[derive(serde::Deserialize)]
    struct RawLaunchConfig {
        #[serde(default)]
        launch_menu: Vec<RawEntry>,
    }

    let path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("smedja-term")
        .join("config.toml");

    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    let raw: RawLaunchConfig = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            debug!("launch_menu parse error: {}", e);
            return Vec::new();
        }
    };

    raw.launch_menu
        .into_iter()
        .map(|e| LaunchEntry {
            label: e.label,
            command: e.command,
        })
        .collect()
}

// ── Agent bridge ─────────────────────────────────────────────────────────────

/// Spawns a background thread that connects to smdjad and streams pane events
/// into `state`, which the status bar modules read each render frame.
///
/// The thread is fire-and-forget: if smdjad is absent or the connection drops,
/// it exits silently and the status bar simply shows no agent context.
fn spawn_agent_bridge(state: SharedPaneState, pane_id: String) {
    std::thread::Builder::new()
        .name("st-agent".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                if !st_agent::socket_exists().await {
                    debug!("agent bridge: smdjad socket absent — skipping");
                    return;
                }
                let Ok(mut client) = st_agent::SmdjadClient::connect().await else {
                    return;
                };
                if client.subscribe_pane(&pane_id).await.is_err() {
                    return;
                }
                while let Ok(Some(ev)) = client.next_event().await {
                    let mut s = state.0.write().await;
                    match ev {
                        st_agent::PaneEvent::TurnStart { tier, model, .. } => {
                            s.tier = Some(tier);
                            s.model = Some(model);
                            s.is_agent_turn = true;
                        }
                        st_agent::PaneEvent::TurnEnd { .. } => {
                            s.is_agent_turn = false;
                        }
                        st_agent::PaneEvent::ToolCall { tool_name, .. } => {
                            s.active_task = Some(tool_name);
                        }
                        _ => {}
                    }
                }
            });
        })
        .ok();
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

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

    let shell = args
        .shell
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));

    let launch_entries = load_launch_entries();
    info!("loaded {} launch menu entries", launch_entries.len());

    info!("starting smedja-term with shell={}", shell);

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("creating event loop")?;
    let mut app = App::new(config, shell, launch_entries);
    event_loop.run_app(&mut app).context("running event loop")?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::App;

    fn make_app() -> App {
        let config = st_config::Config::default();
        App::new(config, "/bin/sh".to_owned(), Vec::new())
    }

    #[test]
    fn app_initialises_with_empty_windows() {
        let app = make_app();
        assert!(app.windows.is_empty());
    }

    #[test]
    fn app_initialises_with_one_tab() {
        let app = make_app();
        assert_eq!(app.tab_bar.tabs.len(), 1);
        assert_eq!(app.split_layouts.len(), 1);
    }
}
