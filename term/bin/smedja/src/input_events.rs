//! Keyboard and mouse window-event handlers.
//!
//! Extracted from the `KeyboardInput`, `CursorMoved`, `MouseInput`, and
//! `MouseWheel` arms of the window-event dispatcher: multiplexer key bindings,
//! PTY key encoding, and mouse reporting / local scrollback.

use std::sync::atomic::Ordering;

use tracing::debug;
use winit::{
    event::{ElementState, MouseButton, MouseScrollDelta},
    event_loop::ActiveEventLoop,
    keyboard::{Key, NamedKey},
    window::WindowId,
};

use crate::app::App;
use crate::input::{encode_key, encode_mouse_sgr, encode_mouse_x10};
use crate::split::SplitDirection;

impl App {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn handle_key_input(&mut self, event_loop: &ActiveEventLoop, logical_key: Key) {
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
                    self.paste_from_clipboard();
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
                        self.paste_from_clipboard();
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
        let (shift, alt, ctrl, sup) = (self.shift(), self.alt(), self.ctrl(), self.superkey());
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

    pub(crate) fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor_pos = (x, y);
        // Send mouse motion events when a button is held (ButtonEvent mode)
        // or unconditionally (AnyEvent mode).
        if self.pty.is_some() {
            let Some((col, row, mode, sgr)) = self.pointer_cell(x, y) else {
                return;
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
                if let Some(pty) = &mut self.pty {
                    if let Err(e) = pty.write_input(&bytes) {
                        debug!("PTY mouse motion write error: {}", e);
                    }
                }
            }
        }
    }

    pub(crate) fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        debug!(
            "MouseInput {:?} {:?} occluded={}",
            state, button, self.occluded
        );
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

        let Some((col, row, mode, sgr)) = self.pointer_cell(self.cursor_pos.0, self.cursor_pos.1)
        else {
            return;
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
        if let Some(pty) = &mut self.pty {
            if let Err(e) = pty.write_input(&bytes) {
                debug!("PTY mouse write error: {}", e);
            }
        }
    }

    pub(crate) fn handle_mouse_wheel(&mut self, delta: MouseScrollDelta, window_id: WindowId) {
        // Number of lines scrolled (positive = wheel up = into history).
        let lines: i32 = match delta {
            #[allow(clippy::cast_possible_truncation)]
            MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
            MouseScrollDelta::PixelDelta(pos) => {
                let sf = self.renderer.as_ref().map_or(1.0_f64, |r| r.scale_factor);
                #[allow(clippy::cast_possible_truncation)]
                let eff_font = self.config.font.size * sf as f32;
                let line_h = f64::from(st_glyph::line_height(eff_font).max(1.0));
                #[allow(clippy::cast_possible_truncation)]
                let lines = (pos.y / line_h).round() as i32;
                lines
            }
        };
        if lines == 0 {
            return;
        }
        // When an application is in a mouse-reporting mode, forward the
        // wheel as SGR/X10 button 64 (up) / 65 (down) so it can scroll
        // its own viewport. Otherwise scroll the terminal's local
        // scrollback buffer.
        let Some((col, row, mode, sgr)) = self.pointer_cell(self.cursor_pos.0, self.cursor_pos.1)
        else {
            return;
        };
        if mode == st_pty::MouseMode::None {
            // Local scrollback. Positive lines scroll up into history.
            if let Some(pty) = &mut self.pty {
                let changed = pty.grid.lock().scroll_by(lines);
                if changed {
                    pty.dirty.store(true, Ordering::Release);
                    if let Some(w) = self.windows.get(&window_id) {
                        w.request_redraw();
                    }
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
            if let Some(pty) = &mut self.pty {
                if let Err(e) = pty.write_input(&data) {
                    debug!("PTY wheel write error: {}", e);
                }
            }
        }
    }
}
