mod events;
mod keyboard;
mod redraw;

use std::collections::HashMap;
use std::sync::{atomic::Ordering, Arc};

use tracing::{debug, error, info};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;

use crate::agent_bridge::spawn_agent_bridge;
use crate::clipboard::read_clipboard_text;
use crate::input::encode_key;
use crate::launch::LaunchEntry;
use crate::mouse::{encode_mouse_sgr, encode_mouse_x10};
use crate::render::render_cell;
use crate::split::{SplitDirection, SplitLayout};
use crate::status::{build_window_title, status_bar_height_for_font, tier_badge_text};
use crate::tab::TabBar;
use st_agent::{SharedAgentManager, SharedPaneState};

// ── User events (sent from async tasks to the event loop) ────────────────────

/// Events that async background tasks can post into the winit event loop.
#[allow(dead_code)] // variants are constructed via EventLoopProxy from background tasks
#[derive(Debug)]
pub(crate) enum UserEvent {
    /// Request that a new terminal window be opened.
    OpenWindow,
}

// ── Launch menu entry ─────────────────────────────────────────────────────────

// ── App state ─────────────────────────────────────────────────────────────────

/// Application state threaded through the winit event loop.
///
/// `PtySession` is owned directly (not behind `Arc`) because the event loop
/// runs on the main thread and the PTY reader thread only accesses the session
/// through the cloned `Arc<Mutex<CellGrid>>` and `Arc<AtomicBool>` that are
/// fields of `PtySession` — not through the session itself.
pub(crate) struct App {
    /// All open windows, keyed by `WindowId`.
    pub(crate) windows: HashMap<WindowId, Arc<Window>>,
    renderer: Option<st_render::Renderer>,
    pty: Option<st_pty::PtySession>,
    config: st_config::Config,
    shell: String,
    /// Subset of `~/.config/starship.toml` used to configure status bar modules.
    starship_config: Option<st_statusbar::StarshipConfig>,
    /// Tab bar — owns all tabs and the active tab index.
    pub(crate) tab_bar: TabBar,
    /// Per-tab split layout.  Keyed by tab index (positional, not UUID) for
    /// simplicity; rebuilt when tabs are opened or closed.
    pub(crate) split_layouts: Vec<SplitLayout>,
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
    pub(crate) fn new(
        config: st_config::Config,
        shell: String,
        launch_entries: Vec<LaunchEntry>,
    ) -> Self {
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

    /// Pastes the clipboard into the active PTY, wrapping it in bracketed-paste
    /// markers when the application has that mode enabled. No-op when there is
    /// no PTY, no clipboard, or the clipboard holds no text.
    fn paste_from_clipboard(&mut self) {
        let Some(pty) = &mut self.pty else { return };
        let bracketed = pty.grid.lock().bracketed_paste;
        let Some(text) = read_clipboard_text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
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

    /// Maps a window pixel position to the grid cell under it, returning
    /// `(col, row, mouse_mode, mouse_sgr)`. Locks the PTY grid; `None` when
    /// there is no PTY. Single source of the pointer→cell arithmetic shared by
    /// the `CursorMoved`, `MouseInput`, and `MouseWheel` handlers.
    fn pointer_cell(&self, win_x: f64, win_y: f64) -> Option<(u16, u16, st_pty::MouseMode, bool)> {
        let pty = self.pty.as_ref()?;
        let grid = pty.grid.lock();
        let sf = self.renderer.as_ref().map_or(1.0_f64, |r| r.scale_factor);
        #[allow(clippy::cast_possible_truncation)]
        let eff_font = self.config.font.size * sf as f32;
        let top_bar_h = self
            .renderer
            .as_ref()
            .map_or(0, st_render::Renderer::top_bar_height_px);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let phys_x = (win_x * sf) as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let phys_y = (win_y * sf) as u32;
        let grid_y = phys_y.saturating_sub(top_bar_h);
        let cw = st_glyph::char_advance_width(eff_font).max(1.0);
        let ch = st_glyph::line_height(eff_font).max(1.0);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let col = ((phys_x as f32 / cw) as u16).min(grid.cols.saturating_sub(1));
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let row = ((grid_y as f32 / ch) as u16).min(grid.rows.saturating_sub(1));
        Some((col, row, grid.mouse_mode, grid.mouse_sgr))
    }
}
