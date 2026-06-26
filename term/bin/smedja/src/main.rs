//! `smedja` — GPU-accelerated terminal emulator entry point.
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
    event::{ElementState, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

// Set Wayland app_id and X11 WM_CLASS so the desktop environment matches the
// window to smedja.desktop and shows the correct icon from the icon theme.
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;

use crate::split::{SplitDirection, SplitLayout};
use crate::tab::TabBar;

use st_agent::{AgentChunk, SharedAgentManager, SharedPaneState};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(name = "smedja", about = "GPU-accelerated terminal emulator")]
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
/// `~/.config/smedja/config.toml`.
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
    /// Subset of `~/.config/starship.toml` used to configure status bar modules.
    starship_config: Option<st_statusbar::StarshipConfig>,
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
    /// Accumulated agent session text, fed by the bridge and read each frame.
    agent_manager: SharedAgentManager,
    /// Current working directory, updated on startup and after each agent turn.
    cwd: Option<String>,
    /// Pane UUID string (used as `session_id` in statusbar / window title).
    pane_id: String,
    /// Last known cursor position in logical pixels.
    cursor_pos: (f64, f64),
    /// Which mouse buttons are currently held down.
    mouse_buttons: u8,
    /// True while the window is fully occluded by another window on Wayland.
    /// Used to suppress redraws that would burn GPU for invisible frames.
    occluded: bool,
    /// After a PTY resize, suppress blank frames until this instant (or None).
    /// Prevents the compositor showing grey during the terminal's clear+redraw
    /// cycle that ratatui emits on resize.
    suppress_clear_until: Option<std::time::Instant>,
}

impl App {
    fn new(config: st_config::Config, shell: String, launch_entries: Vec<LaunchEntry>) -> Self {
        // Initialise with one tab and a split layout for its root pane.
        let tab_bar = TabBar::new();
        let root_pane_id = tab_bar.tabs[0].panes[0].id;
        let split_layouts = vec![SplitLayout::new(root_pane_id)];

        let starship_config = dirs::config_dir()
            .map(|d| d.join("starship.toml"))
            .and_then(|p| st_statusbar::load_starship_fallback(&p));

        Self {
            windows: HashMap::new(),
            renderer: None,
            pty: None,
            config,
            shell,
            starship_config,
            tab_bar,
            split_layouts,
            modifiers: Modifiers::default(),
            launch_entries,
            launch_menu_open: false,
            launch_menu_selection: 0,
            pane_state: SharedPaneState::new(),
            agent_manager: SharedAgentManager::new(),
            cwd: std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(str::to_owned)),
            pane_id: String::new(),
            cursor_pos: (0.0, 0.0),
            mouse_buttons: 0,
            occluded: false,
            suppress_clear_until: None,
        }
    }

    // ── Window helpers ────────────────────────────────────────────────────────

    /// Opens a new terminal window and registers it in `self.windows`.
    fn open_window(&mut self, event_loop: &ActiveEventLoop) {
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("smedja")
            .with_inner_size(winit::dpi::LogicalSize::new(1200u32, 800u32));
        #[cfg(target_os = "linux")]
        {
            attrs = attrs.with_name("smedja", "smedja");
        }

        match event_loop.create_window(attrs) {
            Ok(w) => {
                let w = Arc::new(w);
                #[cfg(target_os = "linux")]
                if let Some(icon) = load_window_icon() {
                    w.set_window_icon(Some(icon));
                }
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
        let launch_cmd = self
            .launch_entries
            .get(self.launch_menu_selection)
            .map(|e| e.command.clone());

        if let Some(cmd) = launch_cmd {
            info!("launch: {}", cmd);
            // Write the command to the PTY as if the user typed it.
            if let Some(pty) = &mut self.pty {
                let mut input = cmd.into_bytes();
                input.push(b'\r');
                if let Err(e) = pty.write_input(&input) {
                    debug!("PTY launch write error: {}", e);
                }
            }
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

    fn alt(&self) -> bool {
        self.modifiers.state().alt_key()
    }

    fn superkey(&self) -> bool {
        self.modifiers.state().super_key()
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Create the first window on initial resume.
        if !self.windows.is_empty() {
            return;
        }

        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("smedja")
            .with_inner_size(winit::dpi::LogicalSize::new(1200u32, 800u32));
        #[cfg(target_os = "linux")]
        {
            attrs = attrs.with_name("smedja", "smedja");
        }

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!("failed to create window: {}", e);
                event_loop.exit();
                return;
            }
        };

        #[cfg(target_os = "linux")]
        if let Some(icon) = load_window_icon() {
            window.set_window_icon(Some(icon));
        }

        // Initialise wgpu renderer — this blocks briefly; in production we'd
        // do this async but pollster makes it tractable here.
        let mut renderer =
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
        // Font size is scaled by scale_factor so the PTY grid matches the
        // physical cell size used by the renderer on HiDPI displays.
        let size = window.inner_size();
        let scale_factor = window.scale_factor();
        #[allow(clippy::cast_possible_truncation)]
        let eff_font = self.config.font.size * scale_factor as f32;
        let sb_h = status_bar_height_for_font(eff_font);
        let grid_h = size.height.saturating_sub(sb_h);
        let (cols, rows) = st_glyph::pixel_size_to_grid(size.width, grid_h, eff_font);

        // Each pane gets a stable UUID injected as SMEDJA_TERM_PANE so smdjad
        // can route agent events back to the correct window.
        let pane_id = self.tab_bar.tabs[0].panes[0].id;
        let pane_id_str = pane_id.to_string();
        self.pane_id.clone_from(&pane_id_str);

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

        // Populate the registry with built-in glyphs so PUA codepoints are
        // assigned before the renderer starts drawing glyph cells.
        {
            let mut reg = pty.glyph_registry.lock();
            st_glyph::register_builtin_glyphs(&mut reg);
        }

        // Share the PTY's glyph registry with the renderer so the atlas can
        // resolve registered PUA codepoints to their cached bitmaps.
        renderer.set_glyph_registry(Arc::clone(&pty.glyph_registry));

        spawn_agent_bridge(
            self.pane_state.clone(),
            self.agent_manager.clone(),
            pane_id_str,
        );

        self.windows.insert(window.id(), window);
        self.renderer = Some(renderer);
        self.pty = Some(pty);
        info!("smedja initialised (pane {pane_id})");
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
                    // Drop the renderer before exiting so we never call
                    // get_current_texture() on a surface whose underlying
                    // Wayland surface has been destroyed (→ SIGSEGV).
                    self.renderer = None;
                    self.pty = None;
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
                    let sf = self.renderer.as_ref().map_or(1.0_f64, |r| r.scale_factor);
                    #[allow(clippy::cast_possible_truncation)]
                    let eff_font = self.config.font.size * sf as f32;
                    let grid_h = self.renderer.as_ref().map_or_else(
                        || {
                            new_size
                                .height
                                .saturating_sub(status_bar_height_for_font(eff_font))
                        },
                        st_render::Renderer::grid_height_px,
                    );
                    let (cols, rows) =
                        st_glyph::pixel_size_to_grid(new_size.width, grid_h, eff_font);
                    let same_size = {
                        let g = pty.grid.lock();
                        g.cols == cols && g.rows == rows
                    };
                    if !same_size {
                        if let Err(e) = pty.resize(cols, rows) {
                            debug!("PTY resize error: {}", e);
                        }
                        // Suppress blank frames for up to 200ms while the child
                        // process (e.g. ratatui TUI) clears and redraws after
                        // receiving SIGWINCH.  Keeps old content visible instead
                        // of showing a grey flash between clear and redraw.
                        self.suppress_clear_until =
                            Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
                    }
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.update_scale_factor(scale_factor);
                    // Trigger a resize with the new physical size.
                    let new_size = renderer.size;
                    renderer.resize(new_size);
                }
            }

            WindowEvent::Focused(focused) => {
                info!("Focused({}) occluded_before={}", focused, self.occluded);
                if focused {
                    self.occluded = false;
                    // Do NOT call renderer.resize() here — reconfiguring an
                    // already-visible Wayland surface briefly detaches the buffer,
                    // making the compositor show grey until the next present.
                    // SurfaceError::Lost/Outdated in the render path handles the
                    // rare case where the surface is actually stale.
                    // Force a cell re-upload so content is redrawn even when the
                    // PTY has been quiet.
                    if let Some(pty) = &self.pty {
                        pty.dirty.store(true, Ordering::Release);
                    }
                    if let Some(w) = self.windows.get(&window_id) {
                        w.request_redraw();
                    }
                }
                if let Some(pty) = &mut self.pty {
                    let send_focus = pty.grid.lock().focus_events;
                    if send_focus {
                        // CSI I = focus gained, CSI O = focus lost (xterm focus protocol)
                        let bytes: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
                        if let Err(e) = pty.write_input(bytes) {
                            debug!("PTY focus event write error: {}", e);
                        }
                    }
                }
            }

            WindowEvent::Occluded(occluded) => {
                info!("Occluded({})", occluded);
                self.occluded = occluded;
                if !occluded {
                    // Window became visible — reconfigure surface and request a
                    // frame so content appears without waiting for the next vsync.
                    if let Some(renderer) = &mut self.renderer {
                        renderer.resize(renderer.size);
                    }
                    // Surface was reconfigured — force cell re-upload.
                    if let Some(pty) = &self.pty {
                        pty.dirty.store(true, Ordering::Release);
                    }
                    if let Some(w) = self.windows.get(&window_id) {
                        w.request_redraw();
                    }
                }
            }

            // Show an I-beam cursor over the terminal content area.
            WindowEvent::CursorEntered { .. } => {
                if let Some(w) = self.windows.get(&window_id) {
                    w.set_cursor(winit::window::CursorIcon::Text);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(w) = self.windows.get(&window_id) {
                    w.set_cursor(winit::window::CursorIcon::Default);
                }
            }

            WindowEvent::RedrawRequested => {
                // Flush any bytes the VT parser queued to write back to the
                // application (e.g. the kitty keyboard protocol query response).
                if let Some(pty) = &mut self.pty {
                    let resp = std::mem::take(&mut pty.grid.lock().pending_responses);
                    if !resp.is_empty() {
                        if let Err(e) = pty.write_input(&resp) {
                            debug!("PTY response write error: {}", e);
                        }
                    }
                }
                if let (Some(pty), Some(renderer)) = (&self.pty, &mut self.renderer) {
                    let dirty = pty.dirty.load(Ordering::Acquire);
                    let sync_active = pty.grid.lock().synchronized_output;
                    let occluded = self.occluded;
                    debug!(dirty, sync_active, occluded, "RedrawRequested");
                    if dirty && !sync_active {
                        pty.dirty.store(false, Ordering::Release);
                        let grid = pty.grid.lock();
                        // Live screen (offset 0): use the cells' own stored
                        // row/col exactly as before — no behaviour change to the
                        // common path. Only when scrolled back do we stamp
                        // positions, since scrollback rows carry stale indices.
                        let cells: Vec<st_render::Cell> = if grid.scroll_offset <= 0 {
                            grid.cells
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
                                .collect()
                        } else {
                            grid.visible_rows(grid.scroll_offset)
                                .iter()
                                .enumerate()
                                .flat_map(|(r, row)| {
                                    row.iter().enumerate().map(move |(col, c)| st_render::Cell {
                                        ch: c.ch,
                                        fg: c.fg,
                                        bg: c.bg,
                                        col: u16::try_from(col).unwrap_or(u16::MAX),
                                        row: u16::try_from(r).unwrap_or(u16::MAX),
                                    })
                                })
                                .collect()
                        };
                        let non_blank = cells.iter().filter(|c| c.ch != ' ').count();
                        drop(grid);
                        debug!(
                            "update_cells: total={} non_blank={}",
                            cells.len(),
                            non_blank
                        );

                        // If all cells just went blank and we're inside the
                        // post-resize suppress window, skip this frame.  The
                        // child (ratatui) sends clear+redraw atomically; keeping
                        // the old cell content avoids the grey flash while waiting
                        // for the redraw to arrive.
                        let in_suppress_window = self
                            .suppress_clear_until
                            .is_some_and(|t| std::time::Instant::now() < t);
                        if non_blank == 0 && in_suppress_window {
                            // Keep dirty=true so we process the next PTY batch
                            // (the redraw content) without waiting for a new event.
                            pty.dirty.store(true, Ordering::Release);
                        } else {
                            if non_blank > 0 {
                                self.suppress_clear_until = None;
                            }
                            renderer.update_cells(&cells);
                        }
                    }

                    // Evaluate status bar modules and update the renderer.
                    // The modules run in parallel (rayon + per-module threads)
                    // within an 8 ms budget.  Live agent state comes from the
                    // st-agent bridge running in its own thread.
                    let (
                        tier,
                        model,
                        active_task,
                        input_tokens,
                        output_tokens,
                        latency_ms,
                        traceparent,
                        tokens_saved,
                        efficiency_ratio,
                    ) = {
                        // Non-blocking try_read: if the lock is contended (agent
                        // event writing) skip the update this frame.
                        if let Ok(s) = self.pane_state.0.try_read() {
                            (
                                s.tier.clone(),
                                s.model.clone(),
                                s.active_task.clone(),
                                s.last_input_tokens,
                                s.last_output_tokens,
                                s.last_latency_ms,
                                s.last_traceparent.clone(),
                                s.tokens_saved,
                                s.efficiency_ratio,
                            )
                        } else {
                            (None, None, None, None, None, None, None, None, None)
                        }
                    };

                    // Read last exit code from OSC 133 D markers in the PTY grid.
                    let last_exit_code = {
                        let grid = pty.grid.lock();
                        grid.block_markers.iter().rev().find_map(|m| {
                            if let st_pty::MarkerKind::CommandDone { exit_code } = m.kind {
                                exit_code
                            } else {
                                None
                            }
                        })
                    };

                    // Read the most recent OSC 7 CWD marker from the PTY grid.
                    let pty_cwd = {
                        let grid = pty.grid.lock();
                        grid.block_markers.iter().rev().find_map(|m| {
                            if let st_pty::MarkerKind::Osc7Cwd { ref path } = m.kind {
                                Some(path.clone())
                            } else {
                                None
                            }
                        })
                    };
                    let cwd = pty_cwd.or_else(|| self.cwd.clone());

                    let sb_ctx = st_statusbar::ModuleContext {
                        tier,
                        model,
                        context_used: 0,
                        context_window: 0,
                        active_task,
                        last_exit_code,
                        input_tokens,
                        output_tokens,
                        latency_ms,
                        traceparent,
                        session_id: Some(self.pane_id.clone()),
                        cwd,
                        interface: Some("tui".to_owned()),
                        tokens_saved,
                        efficiency_ratio,
                    };

                    let git_branch_disabled = self
                        .starship_config
                        .as_ref()
                        .is_some_and(|c| c.git_branch_disabled);
                    let git_branch_symbol = self
                        .starship_config
                        .as_ref()
                        .and_then(|c| c.git_branch_symbol.clone());

                    let mut sb_modules: Vec<Box<dyn st_statusbar::StatusModule>> = vec![
                        Box::new(st_statusbar::TierModule),
                        Box::new(st_statusbar::ModelModule),
                        Box::new(st_statusbar::TokensModule),
                        Box::new(st_statusbar::EfficiencyModule),
                        Box::new(st_statusbar::LatencyModule),
                        Box::new(st_statusbar::TraceModule),
                        Box::new(st_statusbar::ExitCodeModule),
                        Box::new(st_statusbar::TimeModule),
                    ];
                    if !git_branch_disabled {
                        sb_modules.push(Box::new(st_statusbar::GitBranchModule::with_symbol(
                            git_branch_symbol,
                        )));
                    }
                    let mut segments =
                        st_statusbar::render_status_bar_parallel(&sb_modules, &sb_ctx, 8);
                    // Resolve the tier badge to a registered PUA glyph (or plain
                    // fallback text) using the shared registry.
                    if let Some(tier) = sb_ctx.tier.as_deref() {
                        let term = std::env::var("TERM").unwrap_or_default();
                        let badge = {
                            let reg = pty.glyph_registry.lock();
                            tier_badge_text(&reg, tier, &term)
                        };
                        if let Some(seg) = segments.iter_mut().find(|s| s.name == "tier") {
                            seg.text = badge;
                        }
                    }
                    renderer.set_status_bar_segments(&segments);

                    // Build top-bar segments and push them to the renderer.
                    let top_modules: Vec<Box<dyn st_statusbar::StatusModule>> = vec![
                        Box::new(st_statusbar::AppNameModule),
                        Box::new(st_statusbar::SessionIdModule),
                        Box::new(st_statusbar::CwdModule),
                    ];
                    let top_segments =
                        st_statusbar::render_status_bar_parallel(&top_modules, &sb_ctx, 8);
                    renderer.set_top_bar_segments(&top_segments);

                    // Update window title.
                    let title = build_window_title(
                        sb_ctx.tier.as_deref(),
                        sb_ctx.active_task.as_deref(),
                        sb_ctx.session_id.as_deref(),
                        sb_ctx.cwd.as_deref(),
                    );
                    for w in self.windows.values() {
                        w.set_title(&title);
                    }

                    // Snapshot agent session content and push to renderer.
                    if let Ok(mgr) = self.agent_manager.0.try_lock() {
                        let blocks: Vec<st_render::AgentBlockView> = mgr
                            .sessions()
                            .enumerate()
                            .map(|(i, session)| st_render::AgentBlockView {
                                start_row: u16::try_from(i * 4).unwrap_or(u16::MAX),
                                model: session.model.clone(),
                                content_lines: session.content_lines(),
                                approval_pending: session.approval
                                    == st_agent::ApprovalState::Pending,
                            })
                            .collect();
                        renderer.set_agent_blocks(&blocks);
                    }

                    if let Err(e) = renderer.render() {
                        match e.downcast_ref::<st_render::RenderError>() {
                            Some(st_render::RenderError::Frame(
                                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated,
                            )) => {
                                info!("render: surface Lost/Outdated — reconfiguring");
                                renderer.resize(renderer.size);
                                pty.dirty.store(true, Ordering::Release);
                            }
                            Some(st_render::RenderError::Frame(wgpu::SurfaceError::Timeout)) => {
                                debug!("render: surface Timeout (vsync skip)");
                            }
                            _ => info!("render error: {}", e),
                        }
                    }
                }

                // Always request another frame — stopping on occluded caused
                // Hyprland to show grey when the window lost focus, because
                // the compositor shows its fallback when the app stops presenting.
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

                // Ctrl+V (no shift) → paste from clipboard.
                // Mirrors Ctrl+Shift+V so both common conventions work.
                if self.ctrl() && !self.shift() {
                    if let Key::Character(s) = &logical_key {
                        if s.to_lowercase() == "v" {
                            if let Some(pty) = &mut self.pty {
                                let bracketed = pty.grid.lock().bracketed_paste;
                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                    if let Ok(text) = cb.get_text() {
                                        let payload = text.into_bytes();
                                        let data = if bracketed {
                                            let mut w = b"\x1b[200~".to_vec();
                                            w.extend_from_slice(&payload);
                                            w.extend_from_slice(b"\x1b[201~");
                                            w
                                        } else {
                                            payload
                                        };
                                        if let Err(e) = pty.write_input(&data) {
                                            debug!("PTY paste write error: {}", e);
                                        }
                                    }
                                }
                            }
                            return;
                        }
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
                            // Ctrl+Shift+V → paste from clipboard (with bracketed paste support)
                            "v" => {
                                if let Some(pty) = &mut self.pty {
                                    let bracketed = pty.grid.lock().bracketed_paste;
                                    if let Ok(mut cb) = arboard::Clipboard::new() {
                                        if let Ok(text) = cb.get_text() {
                                            let payload = text.into_bytes();
                                            let data = if bracketed {
                                                let mut w = b"\x1b[200~".to_vec();
                                                w.extend_from_slice(&payload);
                                                w.extend_from_slice(b"\x1b[201~");
                                                w
                                            } else {
                                                payload
                                            };
                                            if let Err(e) = pty.write_input(&data) {
                                                debug!("PTY paste write error: {}", e);
                                            }
                                        }
                                    }
                                }
                                return;
                            }
                            // Ctrl+Shift+B → vertical split (was Ctrl+Shift+V before paste)
                            "b" => {
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
                // Encode the key with its modifiers. When the running app has
                // negotiated the kitty keyboard protocol (`CSI > flags u`), keys
                // are emitted in CSI-u form so Shift+Enter, Ctrl+letter, Alt+key
                // etc. are unambiguous; otherwise legacy control bytes / ESC
                // prefixes are generated.
                let (shift, alt, ctrl, sup) =
                    (self.shift(), self.alt(), self.ctrl(), self.superkey());
                if let Some(pty) = &mut self.pty {
                    let kbd_flags = pty.grid.lock().kbd_flags();
                    let bytes = encode_key(&logical_key, shift, alt, ctrl, sup, kbd_flags);
                    debug!(
                        "encode_key: key={:?} shift={} alt={} ctrl={} sup={} kbd_flags={} -> {:?}",
                        logical_key, shift, alt, ctrl, sup, kbd_flags, bytes
                    );
                    if let Some(data) = bytes {
                        // Typing snaps the viewport back to the live screen so
                        // input is always visible.
                        {
                            let mut grid = pty.grid.lock();
                            if grid.scroll_offset != 0 {
                                grid.scroll_offset = 0;
                                pty.dirty.store(true, Ordering::Release);
                            }
                        }
                        // Write errors are non-fatal; PTY may have exited.
                        if let Err(e) = pty.write_input(&data) {
                            debug!("PTY write error: {}", e);
                        }
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
                // Send mouse motion events when a button is held (ButtonEvent mode)
                // or unconditionally (AnyEvent mode).
                if let Some(pty) = &mut self.pty {
                    let (mode, sgr, col, row) = {
                        let grid = pty.grid.lock();
                        let sf = self.renderer.as_ref().map_or(1.0_f64, |r| r.scale_factor);
                        #[allow(clippy::cast_possible_truncation)]
                        let eff_font = self.config.font.size * sf as f32;
                        let top_bar_h = self.renderer.as_ref().map_or(0, |r| r.top_bar_height_px());
                        let phys_x = (position.x * sf) as u32;
                        let phys_y = (position.y * sf) as u32;
                        let grid_y = phys_y.saturating_sub(top_bar_h);
                        let cw = st_glyph::char_advance_width(eff_font).max(1.0);
                        let ch = st_glyph::line_height(eff_font).max(1.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let col = ((phys_x as f32 / cw) as u16).min(grid.cols.saturating_sub(1));
                        #[allow(clippy::cast_possible_truncation)]
                        let row = ((grid_y as f32 / ch) as u16).min(grid.rows.saturating_sub(1));
                        (grid.mouse_mode, grid.mouse_sgr, col, row)
                    };

                    let should_send = match mode {
                        st_pty::MouseMode::AnyEvent => true,
                        st_pty::MouseMode::ButtonEvent => self.mouse_buttons != 0,
                        _ => false,
                    };

                    if should_send {
                        // Motion button code: base 32 + held button (0=left,1=mid,2=right).
                        let held = if self.mouse_buttons & 1 != 0 {
                            0u8
                        } else if self.mouse_buttons & 2 != 0 {
                            1
                        } else if self.mouse_buttons & 4 != 0 {
                            2
                        } else {
                            0
                        };
                        // Bit 5 (32) signals motion in the button encoding.
                        let motion_btn = 32u8 + held;
                        let bytes = if sgr {
                            encode_mouse_sgr(col, row, motion_btn, true)
                        } else {
                            encode_mouse_x10(col, row, motion_btn)
                        };
                        if let Err(e) = pty.write_input(&bytes) {
                            debug!("PTY mouse motion write error: {}", e);
                        }
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                debug!(
                    "MouseInput {:?} {:?} occluded={}",
                    state, button, self.occluded
                );
                if let Some(pty) = &mut self.pty {
                    let btn_code: u8 = match button {
                        MouseButton::Left => 0,
                        MouseButton::Middle => 1,
                        MouseButton::Right => 2,
                        _ => return, // unknown button (Back/Forward)
                    };
                    let pressed = state == ElementState::Pressed;
                    if pressed {
                        self.mouse_buttons |= 1 << btn_code;
                    } else {
                        self.mouse_buttons &= !(1 << btn_code);
                    }

                    let (col, row, mode, sgr) = {
                        let grid = pty.grid.lock();
                        let sf = self.renderer.as_ref().map_or(1.0_f64, |r| r.scale_factor);
                        #[allow(clippy::cast_possible_truncation)]
                        let eff_font = self.config.font.size * sf as f32;
                        let top_bar_h = self.renderer.as_ref().map_or(0, |r| r.top_bar_height_px());
                        let phys_x = (self.cursor_pos.0 * sf) as u32;
                        let phys_y = (self.cursor_pos.1 * sf) as u32;
                        let grid_y = phys_y.saturating_sub(top_bar_h);
                        let cw = st_glyph::char_advance_width(eff_font).max(1.0);
                        let ch = st_glyph::line_height(eff_font).max(1.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let col = ((phys_x as f32 / cw) as u16).min(grid.cols.saturating_sub(1));
                        #[allow(clippy::cast_possible_truncation)]
                        let row = ((grid_y as f32 / ch) as u16).min(grid.rows.saturating_sub(1));
                        (col, row, grid.mouse_mode, grid.mouse_sgr)
                    };

                    debug!(
                        "MouseInput mode={:?} sgr={} col={} row={}",
                        mode, sgr, col, row
                    );
                    if mode == st_pty::MouseMode::None {
                        debug!("MouseInput: mode=None, not forwarding to PTY");
                        return;
                    }
                    let bytes = if sgr {
                        encode_mouse_sgr(col, row, btn_code, pressed)
                    } else {
                        encode_mouse_x10(col, row, if pressed { btn_code } else { 3 })
                    };
                    if let Err(e) = pty.write_input(&bytes) {
                        debug!("PTY mouse write error: {}", e);
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Number of lines scrolled (positive = wheel up = into history).
                let lines: i32 = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => {
                        let sf = self.renderer.as_ref().map_or(1.0_f64, |r| r.scale_factor);
                        #[allow(clippy::cast_possible_truncation)]
                        let eff_font = self.config.font.size * sf as f32;
                        let line_h = f64::from(st_glyph::line_height(eff_font).max(1.0));
                        (pos.y / line_h).round() as i32
                    }
                };
                if lines == 0 {
                    return;
                }
                if let Some(pty) = &mut self.pty {
                    // When an application is in a mouse-reporting mode, forward
                    // the wheel as SGR/X10 button 64 (up) / 65 (down) so it can
                    // scroll its own viewport. Otherwise scroll the terminal's
                    // local scrollback buffer.
                    let (mode, sgr, col, row) = {
                        let grid = pty.grid.lock();
                        let sf = self.renderer.as_ref().map_or(1.0_f64, |r| r.scale_factor);
                        #[allow(clippy::cast_possible_truncation)]
                        let eff_font = self.config.font.size * sf as f32;
                        let top_bar_h = self.renderer.as_ref().map_or(0, |r| r.top_bar_height_px());
                        let phys_x = (self.cursor_pos.0 * sf) as u32;
                        let phys_y = (self.cursor_pos.1 * sf) as u32;
                        let grid_y = phys_y.saturating_sub(top_bar_h);
                        let cw = st_glyph::char_advance_width(eff_font).max(1.0);
                        let ch = st_glyph::line_height(eff_font).max(1.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let col = ((phys_x as f32 / cw) as u16).min(grid.cols.saturating_sub(1));
                        #[allow(clippy::cast_possible_truncation)]
                        let row = ((grid_y as f32 / ch) as u16).min(grid.rows.saturating_sub(1));
                        (grid.mouse_mode, grid.mouse_sgr, col, row)
                    };

                    if mode == st_pty::MouseMode::None {
                        // Local scrollback. Positive lines scroll up into history.
                        let changed = pty.grid.lock().scroll_by(lines);
                        if changed {
                            pty.dirty.store(true, Ordering::Release);
                            if let Some(w) = self.windows.get(&window_id) {
                                w.request_redraw();
                            }
                        }
                    } else {
                        let btn: u8 = if lines > 0 { 64 } else { 65 };
                        let mut data = Vec::new();
                        for _ in 0..lines.abs() {
                            let bytes = if sgr {
                                encode_mouse_sgr(col, row, btn, true)
                            } else {
                                encode_mouse_x10(col, row, btn)
                            };
                            data.extend_from_slice(&bytes);
                        }
                        if let Err(e) = pty.write_input(&data) {
                            debug!("PTY wheel write error: {}", e);
                        }
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Always request a redraw — stopping on occluded causes the compositor
        // to show grey when the window is unfocused (it shows its fallback
        // background when the app stops presenting frames).
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
    px.min(36)
}

// ── Tier badge resolution ──────────────────────────────────────────────────────

/// Resolves a status-bar tier badge to its display text.
///
/// Maps `tier` to its built-in glyph ID via [`st_glyph::glyph_id_for_tier`],
/// then resolves it against `registry` and `term`: when the terminal supports
/// APC sequences and the glyph is registered, returns the single PUA codepoint
/// as a `String`; otherwise returns the plain-text fallback (e.g. `[deep]`).
///
/// An unknown tier returns `[<tier>]` unchanged so existing behaviour is kept.
#[must_use]
fn tier_badge_text(registry: &st_glyph::GlyphRegistry, tier: &str, term: &str) -> String {
    let Some(glyph_id) = st_glyph::glyph_id_for_tier(tier) else {
        return format!("[{tier}]");
    };
    match st_glyph::resolve_badge(registry, glyph_id, term) {
        st_glyph::BadgeRender::Glyph(cp) => cp.to_string(),
        st_glyph::BadgeRender::Text(text) => text.to_owned(),
    }
}

// ── Window title helpers ───────────────────────────────────────────────────────

#[must_use]
fn build_window_title(
    tier: Option<&str>,
    mode: Option<&str>,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> String {
    let mut parts = vec!["smedja".to_owned()];
    if let Some(t) = tier {
        parts.push(format!("[{t}]"));
    }
    if let Some(m) = mode {
        parts.push(format!("[{m}]"));
    }
    if let Some(s) = session_id {
        parts.push(s[..s.len().min(8)].to_owned());
    }
    if let Some(c) = cwd {
        parts.push(truncate_cwd(c, 40));
    }
    parts.join("  ")
}

#[must_use]
fn truncate_cwd(cwd: &str, max: usize) -> String {
    if cwd.len() <= max {
        return cwd.to_owned();
    }
    format!("\u{2026}{}", &cwd[cwd.len() - max..])
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

// ── Launch menu config loading ─────────────────────────────────────────────────

/// Loads `[[launch_menu]]` entries from the smedja config file.
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
        .join("smedja")
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
#[allow(clippy::too_many_lines)]
fn spawn_agent_bridge(state: SharedPaneState, agent_manager: SharedAgentManager, pane_id: String) {
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
                let Ok(mut client) = st_agent::SmdjadClient::connect_agent().await else {
                    return;
                };
                if client.subscribe_pane(&pane_id).await.is_err() {
                    return;
                }
                // Current turn identifier, used as the AgentSession block_id.
                let mut current_turn_id = String::new();
                let mut current_model = String::new();
                while let Ok(Some(ev)) = client.next_event().await {
                    let mut s = state.0.write().await;
                    match ev {
                        st_agent::PaneEvent::TurnStart {
                            tier,
                            model,
                            turn_id,
                            ..
                        } => {
                            if !tier.is_empty() {
                                s.tier = Some(tier);
                            }
                            if !model.is_empty() {
                                s.model = Some(model.clone());
                                current_model = model;
                            }
                            s.is_agent_turn = true;
                            current_turn_id = turn_id;
                        }
                        ref turn_end @ st_agent::PaneEvent::TurnEnd { .. } => {
                            // Accumulate token/latency counters and the cumulative
                            // token-economy figures into pane state (logic lives in
                            // st-agent so it stays unit-testable without a GPU).
                            s.apply_turn_end(turn_end);
                            // Mark the session done.
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let session = mgr.session_mut(&current_turn_id, &current_model);
                                session.push_chunk(&AgentChunk {
                                    block_id: current_turn_id.clone(),
                                    text: String::new(),
                                    done: true,
                                    approval_required: false,
                                });
                            }
                        }
                        st_agent::PaneEvent::ToolCall { tool_name, .. } => {
                            s.active_task = Some(tool_name.clone());
                            // Record tool call as a content line.
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text: format!("[tool: {tool_name}]"),
                                        done: false,
                                        approval_required: false,
                                    });
                            }
                        }
                        st_agent::PaneEvent::StreamDelta { text } => {
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text,
                                        done: false,
                                        approval_required: false,
                                    });
                            }
                        }
                        st_agent::PaneEvent::ToolResult { tool_name, outcome } => {
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text: format!("[{tool_name}: {outcome}]"),
                                        done: false,
                                        approval_required: false,
                                    });
                            }
                        }
                        st_agent::PaneEvent::ApprovalPrompt {
                            tool_name, prompt, ..
                        } => {
                            if !current_turn_id.is_empty() {
                                let mut mgr = agent_manager
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                mgr.session_mut(&current_turn_id, &current_model)
                                    .push_chunk(&AgentChunk {
                                        block_id: current_turn_id.clone(),
                                        text: format!(
                                            "[approval required: {tool_name} — {prompt}]"
                                        ),
                                        done: false,
                                        approval_required: true,
                                    });
                            }
                        }
                    }
                }
            });
        })
        .ok();
}

// ── PTY key dispatch ──────────────────────────────────────────────────────────

/// Maps a winit logical key to the byte sequence to write to the PTY.
///
/// Returns `None` for keys that have no PTY representation (modifier-only keys,
/// unhandled media keys, etc.).  The caller is responsible for writing the
/// returned bytes to the PTY's stdin.
#[must_use]
fn key_to_pty_bytes(key: &Key) -> Option<Vec<u8>> {
    match key {
        Key::Character(s) => Some(s.as_str().as_bytes().to_vec()),
        Key::Named(NamedKey::Space) => Some(b" ".to_vec()),
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(b"\x7f".to_vec()),
        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => Some(b"\x1b".to_vec()),
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
        Key::Named(NamedKey::Home) => Some(b"\x1b[H".to_vec()),
        Key::Named(NamedKey::End) => Some(b"\x1b[F".to_vec()),
        Key::Named(NamedKey::Delete) => Some(b"\x1b[3~".to_vec()),
        Key::Named(NamedKey::PageUp) => Some(b"\x1b[5~".to_vec()),
        Key::Named(NamedKey::PageDown) => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

/// Kitty keyboard-protocol modifier parameter: `1 + bitfield`, where the
/// bitfield is `shift=1, alt=2, ctrl=4, super=8`.
fn kitty_modifier(shift: bool, alt: bool, ctrl: bool, sup: bool) -> u8 {
    let mut bits = 0u8;
    if shift {
        bits |= 1;
    }
    if alt {
        bits |= 2;
    }
    if ctrl {
        bits |= 4;
    }
    if sup {
        bits |= 8;
    }
    1 + bits
}

/// Functional-key codepoints used by the kitty keyboard protocol's `CSI cp ; mod u`
/// encoding for the keys we disambiguate. Arrows/navigation use the legacy
/// `CSI 1 ; mod <final>` form instead and are not listed here.
fn kitty_functional_codepoint(named: &NamedKey) -> Option<u32> {
    Some(match named {
        NamedKey::Enter => 13,
        NamedKey::Tab => 9,
        NamedKey::Backspace => 127,
        NamedKey::Escape => 27,
        NamedKey::Space => 32,
        _ => return None,
    })
}

/// Maps a Ctrl+<char> chord to its C0 control byte (legacy encoding).
///
/// Covers the standard `^A..^Z` letters plus the `@ [ \ ] ^ _ ?` punctuation
/// control codes. Returns `None` for characters with no control mapping.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        ' ' | '@' => Some(0x00),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// Legacy escape sequence for a modified navigation/named key using the xterm
/// `CSI 1 ; <mod> <final>` (cursor) or `CSI <n> ; <mod> ~` (tilde) form.
/// Returns `None` for keys without a navigation encoding.
fn modified_named_legacy(named: &NamedKey, modifier: u8) -> Option<Vec<u8>> {
    let cursor = |final_byte: char| Some(format!("\x1b[1;{modifier}{final_byte}").into_bytes());
    let tilde = |n: u8| Some(format!("\x1b[{n};{modifier}~").into_bytes());
    match named {
        NamedKey::ArrowUp => cursor('A'),
        NamedKey::ArrowDown => cursor('B'),
        NamedKey::ArrowRight => cursor('C'),
        NamedKey::ArrowLeft => cursor('D'),
        NamedKey::Home => cursor('H'),
        NamedKey::End => cursor('F'),
        NamedKey::Delete => tilde(3),
        NamedKey::PageUp => tilde(5),
        NamedKey::PageDown => tilde(6),
        _ => None,
    }
}

/// Encodes a key press (with modifiers) into the bytes written to the PTY.
///
/// Two regimes:
/// - **Kitty mode** (`kbd_flags != 0`): a modified key whose base has a known
///   codepoint is emitted as `CSI <cp> ; <mod> u`, so the application can tell
///   Shift+Enter from Enter, Ctrl+G from `g`, and so on. Modified navigation
///   keys use the legacy `CSI 1 ; mod <final>` form. Unmodified keys fall
///   through to the legacy base encoding.
/// - **Legacy mode** (`kbd_flags == 0`): Ctrl+<char> becomes a C0 control byte,
///   Alt+<char> is ESC-prefixed, modified navigation keys use the xterm
///   `CSI 1 ; mod` form, and everything else is the unmodified base encoding.
fn encode_key(
    key: &Key,
    shift: bool,
    alt: bool,
    ctrl: bool,
    sup: bool,
    kbd_flags: u8,
) -> Option<Vec<u8>> {
    let any_mod = shift || alt || ctrl || sup;
    let modifier = kitty_modifier(shift, alt, ctrl, sup);

    // ── Kitty enhanced encoding (disambiguated) ──────────────────────────────
    if kbd_flags != 0 && any_mod {
        match key {
            Key::Character(s) => {
                if let Some(c) = s.chars().next() {
                    // Report the base (unshifted) codepoint; the shift state is
                    // carried in the modifier field.
                    let cp = c.to_ascii_lowercase() as u32;
                    return Some(format!("\x1b[{cp};{modifier}u").into_bytes());
                }
            }
            Key::Named(named) => {
                if let Some(cp) = kitty_functional_codepoint(named) {
                    return Some(format!("\x1b[{cp};{modifier}u").into_bytes());
                }
                if let Some(bytes) = modified_named_legacy(named, modifier) {
                    return Some(bytes);
                }
            }
            _ => {}
        }
    }

    // ── Legacy encoding ──────────────────────────────────────────────────────
    match key {
        Key::Character(s) => {
            let c = s.chars().next()?;
            // Ctrl(+Alt)+<char> → C0 control byte (Alt adds an ESC prefix).
            if ctrl {
                if let Some(b) = ctrl_byte(c) {
                    return Some(if alt { vec![0x1b, b] } else { vec![b] });
                }
            }
            // Alt+<char> → ESC-prefixed text (meta).
            if alt {
                let mut out = vec![0x1b];
                out.extend_from_slice(s.as_str().as_bytes());
                return Some(out);
            }
            Some(s.as_str().as_bytes().to_vec())
        }
        Key::Named(named) => {
            // Alt+Enter / Alt+Tab-style meta on named keys: ESC-prefix the base.
            if any_mod {
                if let Some(bytes) = modified_named_legacy(named, modifier) {
                    return Some(bytes);
                }
                if alt {
                    if let Some(mut base) = key_to_pty_bytes(key) {
                        let mut out = vec![0x1b];
                        out.append(&mut base);
                        return Some(out);
                    }
                }
            }
            key_to_pty_bytes(key)
        }
        _ => None,
    }
}

// ── Mouse encoding ────────────────────────────────────────────────────────────

/// Encodes a mouse event as an SGR sequence (`\x1b[<Cb;Px;PyM` / `m`).
///
/// SGR mode (`?1006h`) uses decimal coordinates and is unambiguous for large
/// terminals.  `col` and `row` are 0-based; the escape sequence uses 1-based.
fn encode_mouse_sgr(col: u16, row: u16, button: u8, pressed: bool) -> Vec<u8> {
    let suffix = if pressed { b'M' } else { b'm' };
    format!("\x1b[<{};{};{}{}", button, col + 1, row + 1, suffix as char).into_bytes()
}

/// Encodes a mouse event as an X10 sequence (`\x1b[M` + 3 bytes).
///
/// Coordinates are 1-based and clamped to 223 (X10 limit).
fn encode_mouse_x10(col: u16, row: u16, button: u8) -> Vec<u8> {
    let cb = button.saturating_add(32);
    // Clamp in u16 space first to avoid silent truncation when col/row >= 255.
    let cx = (col + 1).min(223) as u8 + 32;
    let cy = (row + 1).min(223) as u8 + 32;
    vec![b'\x1b', b'[', b'M', cb, cx, cy]
}

// ── Window icon ───────────────────────────────────────────────────────────────

/// Loads the smedja brand icon from the embedded PNG and returns a winit `Icon`.
///
/// Only called on Linux; macOS uses the `.icns` bundle resource. Returns `None`
/// on decode or icon-creation failure so the caller can skip silently.
fn load_window_icon() -> Option<winit::window::Icon> {
    let png_bytes = include_bytes!("../../../../assets/brand/smedja-256.png");
    let img = image::load_from_memory(png_bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
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
mod tests {
    use super::{tier_badge_text, App};

    #[test]
    fn tier_badge_resolves_to_pua_glyph_when_apc_supported() {
        let mut reg = st_glyph::GlyphRegistry::new();
        st_glyph::register_builtin_glyphs(&mut reg);
        let badge = tier_badge_text(&reg, "deep", "xterm-wezterm");
        let cp = reg.lookup("smedja.tier.deep").expect("deep is registered");
        assert_eq!(badge, cp.to_string());
    }

    #[test]
    fn tier_badge_falls_back_to_text_when_apc_unsupported() {
        let mut reg = st_glyph::GlyphRegistry::new();
        st_glyph::register_builtin_glyphs(&mut reg);
        let badge = tier_badge_text(&reg, "deep", "xterm-256color");
        assert_eq!(badge, "[deep]");
    }

    #[test]
    fn tier_badge_unknown_tier_is_unchanged() {
        let reg = st_glyph::GlyphRegistry::new();
        assert_eq!(tier_badge_text(&reg, "cloud", "xterm-wezterm"), "[cloud]");
    }

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

    #[test]
    fn build_window_title_contains_required_parts() {
        use super::build_window_title;
        let title = build_window_title(
            Some("fast"),
            Some("impl"),
            Some("abc12345xyz"),
            Some("/home/u/proj"),
        );
        assert!(title.contains("smedja"), "must contain app name");
        assert!(title.contains("[fast]"), "must contain tier");
        assert!(title.contains("[impl]"), "must contain mode");
        assert!(
            title.contains("abc12345"),
            "must contain first 8 chars of session_id"
        );
        assert!(title.contains("/home/u/proj"), "must contain cwd");
    }

    #[test]
    fn build_window_title_all_none_returns_smedja() {
        use super::build_window_title;
        let title = build_window_title(None, None, None, None);
        assert_eq!(title, "smedja");
    }

    #[test]
    fn truncate_cwd_long_path_starts_with_ellipsis() {
        use super::truncate_cwd;
        let long = "/very/long/path/that/exceeds/limit";
        let result = truncate_cwd(long, 10);
        assert!(
            result.starts_with('\u{2026}'),
            "expected … prefix, got '{result}'"
        );
        assert!(
            result.chars().count() <= 11,
            "result must be at most max+1 chars"
        );
    }

    #[test]
    fn truncate_cwd_short_path_unchanged() {
        use super::truncate_cwd;
        let short = "/short";
        let result = truncate_cwd(short, 40);
        assert_eq!(result, "/short");
    }

    #[test]
    fn space_key_produces_space_byte() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, NamedKey};
        let bytes = key_to_pty_bytes(&Key::Named(NamedKey::Space));
        assert_eq!(bytes, Some(b" ".to_vec()), "space must produce 0x20");
    }

    #[test]
    fn named_keys_produce_correct_escape_sequences() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, NamedKey};
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Enter)),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Backspace)),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Tab)),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::ArrowUp)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::ArrowDown)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Delete)),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn character_key_produces_utf8_bytes() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, SmolStr};
        let bytes = key_to_pty_bytes(&Key::Character(SmolStr::new("a")));
        assert_eq!(bytes, Some(b"a".to_vec()));
    }

    #[test]
    fn unknown_key_produces_none() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, NamedKey};
        assert_eq!(key_to_pty_bytes(&Key::Named(NamedKey::F1)), None);
    }

    // ── encode_key: legacy control / Alt / modifier handling ─────────────────

    #[test]
    fn ctrl_letter_produces_c0_control_byte_in_legacy_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        // Ctrl+C → 0x03, Ctrl+G → 0x07 — the bug that broke all control keys.
        let c = encode_key(&Key::Character(SmolStr::new("c")), false, false, true, false, 0);
        assert_eq!(c, Some(vec![0x03]));
        let g = encode_key(&Key::Character(SmolStr::new("g")), false, false, true, false, 0);
        assert_eq!(g, Some(vec![0x07]));
    }

    #[test]
    fn alt_char_is_esc_prefixed_in_legacy_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        let b = encode_key(&Key::Character(SmolStr::new("b")), false, true, false, false, 0);
        assert_eq!(b, Some(vec![0x1b, b'b']));
    }

    #[test]
    fn plain_char_is_literal_in_legacy_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        let a = encode_key(&Key::Character(SmolStr::new("a")), false, false, false, false, 0);
        assert_eq!(a, Some(b"a".to_vec()));
    }

    #[test]
    fn modified_arrow_uses_xterm_csi_form() {
        use super::encode_key;
        use winit::keyboard::{Key, NamedKey};
        // Ctrl+ArrowRight → CSI 1 ; 5 C
        let r = encode_key(&Key::Named(NamedKey::ArrowRight), false, false, true, false, 0);
        assert_eq!(r, Some(b"\x1b[1;5C".to_vec()));
    }

    // ── encode_key: kitty enhanced (disambiguated) encoding ──────────────────

    #[test]
    fn shift_enter_is_csi_u_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, NamedKey};
        // Shift+Enter with flags active → CSI 13 ; 2 u (Enter=13, shift mod=2).
        let bytes = encode_key(&Key::Named(NamedKey::Enter), true, false, false, false, 1);
        assert_eq!(bytes, Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn ctrl_g_is_csi_u_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        // Ctrl+G with flags active → CSI 103 ; 5 u (g=103, ctrl mod=5).
        let bytes = encode_key(&Key::Character(SmolStr::new("g")), false, false, true, false, 1);
        assert_eq!(bytes, Some(b"\x1b[103;5u".to_vec()));
    }

    #[test]
    fn unmodified_enter_is_plain_cr_even_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, NamedKey};
        // No modifiers → falls through to the legacy base encoding.
        let bytes = encode_key(&Key::Named(NamedKey::Enter), false, false, false, false, 1);
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }
}
