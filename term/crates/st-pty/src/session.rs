//! The live PTY session: child-process lifecycle, the background reader thread
//! that drives the VT parser, and keyboard-driven copy mode.

use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use parking_lot::Mutex;
use tracing::{debug, warn};

use st_glyph::GlyphRegistry;

use crate::grid::CellGrid;
use crate::vt::{ApcScanner, VtHandler};
use crate::PtyError;

// ── Copy mode ────────────────────────────────────────────────────────────────

/// Copy mode state for keyboard-driven selection.
#[derive(Debug, Default)]
pub struct CopyMode {
    /// Whether copy mode is active.
    pub active: bool,
    /// Current cursor position in copy mode `(col, row)`.
    pub cursor: (u16, u16),
    /// Visual selection anchor `(col, row)`, if any.
    pub anchor: Option<(u16, u16)>,
    /// Current search query.
    pub search_query: String,
    /// Matching cell positions `(col, row)`.
    pub search_matches: Vec<(u16, u16)>,
}

impl CopyMode {
    /// Creates a new [`CopyMode`] anchored at `(0, 0)`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters copy mode, positioning the copy cursor at the current terminal
    /// cursor position.
    pub fn enter(&mut self, terminal_cursor: (u16, u16)) {
        self.active = true;
        self.cursor = terminal_cursor;
        self.anchor = None;
        self.search_matches.clear();
    }

    /// Exits copy mode.
    pub fn exit(&mut self) {
        self.active = false;
        self.anchor = None;
        self.search_matches.clear();
    }

    /// Copies the selected region of `grid` to the system clipboard.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Clipboard`] if the clipboard cannot be opened or
    /// written to.
    pub fn copy_selection(&mut self, grid: &CellGrid) -> Result<(), PtyError> {
        let Some(anchor) = self.anchor else {
            return Ok(());
        };
        let (ac, ar) = anchor;
        let (cc, cr) = self.cursor;

        // Normalise selection bounds.
        let (r_start, r_end) = if ar <= cr { (ar, cr) } else { (cr, ar) };
        let (c_start, c_end) = if ar == cr {
            if ac <= cc {
                (ac, cc)
            } else {
                (cc, ac)
            }
        } else {
            (0, grid.cols.saturating_sub(1))
        };

        let mut text = String::new();
        for row in r_start..=r_end {
            let start_col = if row == r_start { c_start } else { 0 };
            let end_col = if row == r_end {
                c_end
            } else {
                grid.cols.saturating_sub(1)
            };

            if let Some(r) = grid.cells.get(row as usize) {
                for col in start_col..=end_col {
                    if let Some(cell) = r.get(col as usize) {
                        text.push(cell.ch);
                    }
                }
            }
            if row < r_end {
                text.push('\n');
            }
        }

        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| PtyError::Clipboard(e.to_string()))?;
        clipboard
            .set_text(text)
            .map_err(|e| PtyError::Clipboard(e.to_string()))?;
        Ok(())
    }

    /// Searches for `query` in the grid and populates `search_matches`.
    pub fn search(&mut self, query: &str, grid: &CellGrid) {
        self.search_query = query.to_owned();
        self.search_matches.clear();

        if query.is_empty() {
            return;
        }

        // Build a flat string per row and search for matches.
        for (r, row) in grid.cells.iter().enumerate() {
            let line: String = row.iter().map(|c| c.ch).collect();
            let mut start = 0;
            while let Some(pos) = line[start..].find(query) {
                let abs = start + pos;
                // The match column is the number of cells (chars) before `abs`,
                // not the raw byte offset — one grid cell holds exactly one char.
                let col = line[..abs].chars().count();
                self.search_matches.push((col as u16, r as u16));
                // Advance past the first char of this match. Stepping a single
                // byte could land mid-codepoint and panic the next `line[start..]`
                // slice on multibyte content (emoji/CJK/accented text).
                let step = line[abs..].chars().next().map_or(1, char::len_utf8);
                start = abs + step;
            }
        }
    }
}

// ── PtySession ────────────────────────────────────────────────────────────────

/// An active PTY session: child shell + cell grid + dirty flag.
pub struct PtySession {
    master: Box<dyn portable_pty::MasterPty + Send>,
    // Child process handle; retained so teardown can hang up (SIGHUP) and
    // reap it, and so a normal-exit child is `wait()`ed (no zombie).
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    /// Join handle for the background reader thread, retained so teardown can
    /// join it instead of leaking the thread (and its cloned master fd).
    reader: Option<std::thread::JoinHandle<()>>,
    /// The shared terminal cell grid.
    pub grid: Arc<Mutex<CellGrid>>,
    /// Set to `true` whenever the grid changes.  Renderers poll this flag.
    pub dirty: Arc<AtomicBool>,
    /// Set to `true` once the child process exits (reader hits EOF). The host
    /// app polls this to close the window instead of showing a dead/blank grid.
    pub exited: Arc<AtomicBool>,
    /// Copy-mode state.
    pub copy_mode: CopyMode,
    /// Glyph registry: maps glyph IDs to PUA codepoints.
    pub glyph_registry: Arc<Mutex<GlyphRegistry>>,
}

impl Drop for PtySession {
    /// Full teardown so closing a split (Ctrl+W) while the child is still
    /// running does not leak a reader thread, a master fd, and a zombie child.
    ///
    /// `child.kill()` sends `SIGHUP` to the child. Because `portable_pty`
    /// spawns the child as a session leader owning the pty as its controlling
    /// terminal, the hangup terminates the session and propagates to the
    /// foreground process group, which closes the pty slave. The reader
    /// thread's blocking read on the cloned master then returns EOF and the
    /// thread exits, so the join below cannot hang. `child.wait()` reaps the
    /// process — for both the killed child here and a child that already
    /// exited on its own (the normal-exit path), leaving no zombie.
    fn drop(&mut self) {
        // Best effort: the child may already be gone, in which case kill fails
        // harmlessly and wait returns its status immediately.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl PtySession {
    /// Spawns a new PTY session running `shell` at `cols×rows`.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Pty`] if the PTY or child process cannot be created.
    pub fn spawn(cols: u16, rows: u16, shell: &str) -> Result<Self, PtyError> {
        Self::spawn_with_env(cols, rows, shell, &[])
    }

    /// Spawns a PTY session with additional environment variables injected
    /// alongside the standard `TERM` and `COLORTERM` entries.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Pty`] if the PTY cannot be opened or the shell
    /// cannot be spawned, or [`PtyError::Io`] if the master writer cannot be
    /// extracted.
    pub fn spawn_with_env(
        cols: u16,
        rows: u16,
        shell: &str,
        extra_env: &[(&str, &str)],
    ) -> Result<Self, PtyError> {
        use portable_pty::{CommandBuilder, PtySize};

        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let grid = Arc::new(Mutex::new(CellGrid::new(cols, rows)));
        let dirty = Arc::new(AtomicBool::new(false));
        let glyph_registry = Arc::new(Mutex::new(GlyphRegistry::new()));

        Ok(Self {
            master: pair.master,
            child,
            writer,
            reader: None,
            grid,
            dirty,
            exited: Arc::new(AtomicBool::new(false)),
            copy_mode: CopyMode::new(),
            glyph_registry,
        })
    }

    /// Returns the OS process id of the child, if it is still known.
    #[must_use]
    pub fn child_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Sends raw bytes to the child's stdin.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Io`] if the write fails.
    pub fn write_input(&mut self, data: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resizes the PTY master to `cols×rows`.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Pty`] if the resize syscall fails.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        use portable_pty::PtySize;
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Pty(e.to_string()))?;
        let mut grid = self.grid.lock();
        grid.resize(cols, rows);
        Ok(())
    }

    /// Starts a background reader thread that feeds PTY output into the cell grid.
    ///
    /// The caller must wrap `self` in an `Arc` before calling this method.  The
    /// reader thread sets the `dirty` flag after each batch of bytes processed.
    pub fn start_reader(self: Arc<Self>) {
        let grid = Arc::clone(&self.grid);
        let dirty = Arc::clone(&self.dirty);
        let exited = Arc::clone(&self.exited);
        let glyph_registry = Arc::clone(&self.glyph_registry);
        // ponytail: master.try_clone() is synchronous I/O; reader lives on a
        // dedicated thread so it never blocks the async runtime.
        let mut reader = match self.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to clone PTY reader: {}", e);
                return;
            }
        };

        std::thread::spawn(move || {
            let mut parser = vte::Parser::new();
            let mut handler = VtHandler {
                grid,
                glyph_registry,
            };
            let mut apc_scanner = ApcScanner::new();
            let mut buf = [0u8; 4096];

            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF — child exited
                    Ok(n) => {
                        for &byte in &buf[..n] {
                            if let Some(payload) = apc_scanner.advance(byte) {
                                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                                    let mut registry = handler.glyph_registry.lock();
                                    let cp =
                                        registry.register_shape(&reg.id, reg.format, &reg.data);
                                    if registry.bitmap(cp).is_some() {
                                        debug!(glyph_id = %reg.id, "registered glyph via APC");
                                    } else {
                                        warn!(glyph_id = %reg.id, "glyph registered without a bitmap (rasterisation failed)");
                                    }
                                }
                            }
                            parser.advance(&mut handler, byte);
                        }
                        // Drain any OSC 52 clipboard write queued during parsing.
                        let pending = handler.grid.lock().pending_clipboard_write.take();
                        if let Some(text) = pending {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Err(e) = cb.set_text(text) {
                                    debug!("OSC 52 clipboard write error: {}", e);
                                }
                            }
                        }
                        dirty.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        debug!("PTY read error: {}", e);
                        break;
                    }
                }
            }
            // Child exited — signal the host app (and wake it for a final poll).
            exited.store(true, Ordering::Release);
            dirty.store(true, Ordering::Release);
            debug!("PTY reader thread exited");
        });
    }

    /// Starts a background reader thread that feeds PTY output into the cell grid.
    ///
    /// This variant takes `&mut self` and does not require an `Arc` wrapper.
    /// The reader thread captures clones of the grid and dirty flag handles so
    /// `self` can be moved or used freely after this call.
    ///
    /// # ponytail
    ///
    /// `master.try_clone_reader()` is synchronous I/O; the reader lives on a
    /// dedicated OS thread so it never blocks the async runtime.
    pub fn start_reader_detached(&mut self) {
        let grid = Arc::clone(&self.grid);
        let dirty = Arc::clone(&self.dirty);
        let exited = Arc::clone(&self.exited);
        let glyph_registry = Arc::clone(&self.glyph_registry);
        let mut reader = match self.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to clone PTY reader: {}", e);
                return;
            }
        };
        let handle = std::thread::spawn(move || {
            let mut parser = vte::Parser::new();
            let mut handler = VtHandler {
                grid,
                glyph_registry,
            };
            let mut apc_scanner = ApcScanner::new();
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &byte in &buf[..n] {
                            if let Some(payload) = apc_scanner.advance(byte) {
                                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                                    let mut registry = handler.glyph_registry.lock();
                                    let cp =
                                        registry.register_shape(&reg.id, reg.format, &reg.data);
                                    if registry.bitmap(cp).is_some() {
                                        debug!(glyph_id = %reg.id, "registered glyph via APC");
                                    } else {
                                        warn!(glyph_id = %reg.id, "glyph registered without a bitmap (rasterisation failed)");
                                    }
                                }
                            }
                            parser.advance(&mut handler, byte);
                        }
                        // Drain any OSC 52 clipboard write queued during parsing.
                        let pending = handler.grid.lock().pending_clipboard_write.take();
                        if let Some(text) = pending {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Err(e) = cb.set_text(text) {
                                    debug!("OSC 52 clipboard write error: {}", e);
                                }
                            }
                        }
                        dirty.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        debug!("PTY read error: {}", e);
                        break;
                    }
                }
            }
            // Child exited — signal the host app (and wake it for a final poll).
            exited.store(true, Ordering::Release);
            dirty.store(true, Ordering::Release);
            debug!("PTY reader thread exited");
        });
        self.reader = Some(handle);
    }

    /// Enters copy mode, anchoring at the current cursor position.
    pub fn enter_copy_mode(&mut self) {
        let cursor = self.grid.lock().cursor;
        self.copy_mode.enter(cursor);
    }

    /// Copies the current selection to the clipboard.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Clipboard`] if the clipboard write fails.
    pub fn copy_selection(&mut self) -> Result<(), PtyError> {
        let grid = self.grid.lock();
        self.copy_mode.copy_selection(&grid)
    }

    /// Searches `query` in the grid and stores matches in copy mode state.
    pub fn search(&mut self, query: &str) {
        let grid = self.grid.lock();
        self.copy_mode.search(query, &grid);
    }
}
