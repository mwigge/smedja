use super::*;

impl App {
    pub(super) fn handle_keyboard_input(&mut self, event_loop: &ActiveEventLoop, logical_key: Key) {
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
}
