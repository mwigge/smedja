//! `smedja-term` — GPU-accelerated terminal emulator entry point.
//!
//! Initialises the winit event loop, wgpu surface, PTY session, and block model.
//! Dispatches keyboard input to the PTY and cell-grid updates to the renderer.

use std::sync::{atomic::Ordering, Arc};

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::{debug, error, info};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

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
}

#[derive(Debug, Subcommand)]
enum BlockAction {
    /// Export the output of a block to stdout.
    Export {
        /// The UUID of the block to export.
        block_id: uuid::Uuid,
    },
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Application state threaded through the winit event loop.
///
/// `PtySession` is owned directly (not behind `Arc`) because the event loop
/// runs on the main thread and the PTY reader thread only accesses the session
/// through the cloned `Arc<Mutex<CellGrid>>` and `Arc<AtomicBool>` that are
/// fields of `PtySession` — not through the session itself.
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<st_render::Renderer>,
    pty: Option<st_pty::PtySession>,
    config: st_config::Config,
    shell: String,
}

impl App {
    fn new(config: st_config::Config, shell: String) -> Self {
        Self {
            window: None,
            renderer: None,
            pty: None,
            config,
            shell,
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Create the window on first resume.
        if self.window.is_some() {
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
                    self.window = Some(window);
                    return;
                }
            };

        // Compute initial grid size from window dimensions and font metrics.
        let size = window.inner_size();
        let (cols, rows) =
            st_glyph::pixel_size_to_grid(size.width, size.height, self.config.font.size);

        // Spawn PTY session.
        let mut pty = match st_pty::PtySession::spawn(cols, rows, &self.shell) {
            Ok(p) => p,
            Err(e) => {
                error!("PTY spawn failed: {}", e);
                self.window = Some(window);
                self.renderer = Some(renderer);
                return;
            }
        };
        pty.start_reader_detached();

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.pty = Some(pty);
        info!("smedja-term initialised");
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested");
                event_loop.exit();
            }

            WindowEvent::Resized(new_size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(new_size);
                }
                if let Some(pty) = &mut self.pty {
                    let (cols, rows) = st_glyph::pixel_size_to_grid(
                        new_size.width,
                        new_size.height,
                        self.config.font.size,
                    );
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

                    if let Err(e) = renderer.render() {
                        debug!("render error: {}", e);
                    }
                }

                // Request another frame.
                if let Some(w) = &self.window {
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
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
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
        None => {}
    }

    let config = st_config::Config::load().unwrap_or_default();

    let shell = args
        .shell
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));

    info!("starting smedja-term with shell={}", shell);

    let event_loop = EventLoop::new().context("creating event loop")?;
    let mut app = App::new(config, shell);
    event_loop.run_app(&mut app).context("running event loop")?;

    Ok(())
}
