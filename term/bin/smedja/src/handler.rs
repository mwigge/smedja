//! The winit `ApplicationHandler` implementation for [`App`].
//!
//! Owns the lifecycle callbacks — `resumed` (window + renderer + PTY init),
//! `user_event`, `window_event` (the dispatcher), and `about_to_wait`. The
//! heavier event arms delegate to inherent methods in sibling modules.

use std::sync::{atomic::Ordering, Arc};

use tracing::{debug, error, info};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, WindowEvent},
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};

// Set Wayland app_id and X11 WM_CLASS so the desktop environment matches the
// window to smedja.desktop and shows the correct icon from the icon theme.
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;

use crate::agent_bridge::spawn_agent_bridge;
use crate::app::{App, UserEvent};
use crate::render::{load_window_icon, status_bar_height_for_font};

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
        // Reserve only the bottom status bar (there is no top bar anymore),
        // matching Renderer::grid_height_px (top_bar_height_px() == 0). Reserving
        // a phantom top row here would give the initial PTY one row too few, so
        // the client's (ratatui) layout would not match the visible area until a
        // resize corrected it.
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

            WindowEvent::RedrawRequested => self.redraw_requested(),

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => self.handle_key_input(event_loop, logical_key),

            WindowEvent::CursorMoved { position, .. } => {
                self.handle_cursor_moved(position.x, position.y);
            }

            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input(state, button);
            }

            WindowEvent::MouseWheel { delta, .. } => self.handle_mouse_wheel(delta, window_id),

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // When the child program (the dashboard / shell) exits, close the
        // terminal instead of leaving a dead, grey-blank grid on screen.
        if self
            .pty
            .as_ref()
            .is_some_and(|p| p.exited.load(Ordering::Acquire))
        {
            event_loop.exit();
            return;
        }
        // Always request a redraw — stopping on occluded causes the compositor
        // to show grey when the window is unfocused (it shows its fallback
        // background when the app stops presenting frames).
        for w in self.windows.values() {
            w.request_redraw();
        }
    }
}
