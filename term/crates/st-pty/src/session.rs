//! [`PtySession`]: child shell process, cell grid, and reader thread.

use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use parking_lot::Mutex;
use tracing::{debug, warn};

use st_glyph::GlyphRegistry;

use crate::apc::ApcScanner;
use crate::copy::CopyMode;
use crate::error::PtyError;
use crate::grid::CellGrid;
use crate::vt::VtHandler;

/// An active PTY session: child shell + cell grid + dirty flag.
pub struct PtySession {
    master: Box<dyn portable_pty::MasterPty + Send>,
    #[allow(dead_code)] // child process handle — drop closes the PTY
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
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
            grid,
            dirty,
            exited: Arc::new(AtomicBool::new(false)),
            copy_mode: CopyMode::new(),
            glyph_registry,
        })
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
