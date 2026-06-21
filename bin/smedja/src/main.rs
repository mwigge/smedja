pub mod action_log;
mod blocks;
mod context_rail;
mod staging;
mod statusbar;
pub mod theme;

use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use statusbar::{render_status_bar, ModuleCtx};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{event, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::json;
use smedja_rpc::client::Client;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "smedja", about = "smedja terminal client")]
struct Cli {
    /// smdjad socket path (default: `$XDG_RUNTIME_DIR/smdjad.sock`)
    #[arg(long, env = "SMEDJA_SOCK")]
    sock: Option<PathBuf>,

    /// Agent mode (impl|review|test|sre|explain)
    #[arg(long, short = 'm')]
    mode: Option<String>,

    /// Tier override (local|fast|deep)
    #[arg(long, short = 't')]
    tier: Option<String>,
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Role {
    User,
    System,
}

#[derive(Debug, Clone)]
struct Message {
    role: Role,
    text: String,
}

#[derive(Debug)]
struct AppState {
    session_id: String,
    mode: Option<String>,
    tier: Option<String>,
    messages: Vec<Message>,
    input: String,
    quit: bool,
    /// Task ID of an in-flight turn being polled for a response.
    pending_task_id: Option<String>,
    /// Timestamp of the last poll attempt.
    last_poll: Option<std::time::Instant>,
    /// Monotonically increasing turn counter.
    turn_n: u32,
    /// Timestamp when the current turn was submitted (used to compute `elapsed_ms`).
    turn_submitted_at: Option<std::time::Instant>,
    /// The turn block being assembled for the current in-flight turn.
    current_block: Option<blocks::TurnBlock>,
    /// Completed turn block history.
    block_store: blocks::BlockStore,
    /// Whether the block browser overlay is open.
    block_browser_open: bool,
    /// Cursor position within the block browser.
    block_browser_cursor: usize,
    /// In-memory clipboard (no system clipboard).
    clipboard: Option<String>,
    /// Full diff overlay: (`tool_entry_idx`, `diff_lines`).
    diff_overlay: Option<(usize, Vec<String>)>,
    /// Scroll offset within the diff overlay.
    diff_scroll: usize,
    /// Staging queue for batched tool dispatch.
    staging_queue: staging::StagingQueue,
    /// Whether the context rail sidebar is visible.
    context_rail_visible: bool,
}

// ---------------------------------------------------------------------------
// Socket path resolution
// ---------------------------------------------------------------------------

fn socket_path(override_path: Option<PathBuf>) -> PathBuf {
    override_path.unwrap_or_else(|| {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(base).join("smdjad.sock")
    })
}

// ---------------------------------------------------------------------------
// Submit a user turn to the daemon
// ---------------------------------------------------------------------------

async fn submit(input: &str, state: &mut AppState, client: &mut Client) -> Result<()> {
    let text = input.trim().to_owned();
    if text.is_empty() {
        return Ok(());
    }
    state.messages.push(Message {
        role: Role::User,
        text: text.clone(),
    });
    state.turn_n += 1;
    state.turn_submitted_at = Some(std::time::Instant::now());
    state.current_block = Some(blocks::TurnBlock::new(state.turn_n));
    let resp = client
        .call(
            "turn.submit",
            json!({
                "session_id": state.session_id,
                "content": text,
            }),
        )
        .await;
    let reply = match resp {
        Ok(ref v) => {
            let task_id = v["task_id"].as_str().unwrap_or("?").to_owned();
            state.pending_task_id = Some(task_id.clone());
            state.last_poll = Some(std::time::Instant::now());
            format!("queued (task: {task_id})")
        }
        Err(ref e) => format!("error: {e}"),
    };
    state.messages.push(Message {
        role: Role::System,
        text: reply,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Key handler
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)] // key dispatch table for TUI; splitting would obscure the flow
async fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    client: &mut Client,
) -> Result<()> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.quit = true;
        }

        // Ctrl-R: toggle context rail
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.context_rail_visible = !state.context_rail_visible;
        }

        KeyCode::Esc => {
            if state.diff_overlay.is_some() {
                state.diff_overlay = None;
            } else if state.block_browser_open {
                state.block_browser_open = false;
            } else {
                state.quit = true;
            }
        }

        KeyCode::Up => {
            if state.block_browser_open {
                state.block_browser_cursor = state.block_browser_cursor.saturating_sub(1);
            }
        }

        KeyCode::Down => {
            if state.block_browser_open {
                let max = state.block_store.len().saturating_sub(1);
                if state.block_browser_cursor < max {
                    state.block_browser_cursor += 1;
                }
            }
        }

        KeyCode::Backspace => {
            state.input.pop();
        }

        KeyCode::Char('b') => {
            // Toggle block browser.
            state.block_browser_open = !state.block_browser_open;
            if state.block_browser_open {
                state.block_browser_cursor = 0;
            }
        }

        KeyCode::Char('c') => {
            if state.block_browser_open {
                // Copy selected block text to in-memory clipboard.
                if let Some(block) = state.block_store.blocks().nth(state.block_browser_cursor) {
                    state.clipboard = Some(block.render_lines(80).join("\n"));
                }
            } else {
                state.input.push('c');
            }
        }

        KeyCode::Char('r') => {
            if state.block_browser_open {
                // Resubmit the selected block's first user-visible content.
                // Extract the content string while the borrow on block_store is
                // limited to this scope so the mutable borrow for submit() can
                // follow.
                let content: Option<String> = {
                    let cursor = state.block_browser_cursor;
                    state
                        .block_store
                        .blocks()
                        .nth(cursor)
                        .map(|b| b.content.clone())
                        .filter(|c| !c.is_empty())
                };
                if let Some(content) = content {
                    submit(&content, state, client).await?;
                }
            } else {
                state.input.push('r');
            }
        }

        KeyCode::Char('D') => {
            // Full diff overlay for first tool_call with a diff in selected block.
            if state.block_browser_open {
                if let Some(block) = state.block_store.blocks().nth(state.block_browser_cursor) {
                    let diff_lines: Option<Vec<String>> = block
                        .tool_calls
                        .iter()
                        .enumerate()
                        .find_map(|(i, entry)| {
                            entry
                                .diff
                                .as_ref()
                                .map(|d| (i, d.lines().map(str::to_owned).collect::<Vec<_>>()))
                        })
                        .map(|(i, lines)| {
                            state.diff_scroll = 0;
                            // Return sentinel index + lines; capture i via closure.
                            let _ = i;
                            lines
                        });
                    // Find the tool entry index separately.
                    let entry_idx = block
                        .tool_calls
                        .iter()
                        .position(|e| e.diff.is_some())
                        .unwrap_or(0);
                    if let Some(lines) = diff_lines {
                        state.diff_overlay = Some((entry_idx, lines));
                        state.diff_scroll = 0;
                    }
                }
            }
        }

        KeyCode::Char('d') => {
            // Toggle inline diff overlay (up to 20 lines).
            if state.diff_overlay.is_some() {
                state.diff_overlay = None;
            } else if state.block_browser_open {
                if let Some(block) = state.block_store.blocks().nth(state.block_browser_cursor) {
                    if let Some(lines) = block.inline_diff(0, 20) {
                        state.diff_overlay = Some((0, lines));
                        state.diff_scroll = 0;
                    }
                }
            } else {
                state.input.push('d');
            }
        }

        KeyCode::Enter => {
            let input = std::mem::take(&mut state.input);
            if let Some(rest) = input.trim().strip_prefix("/task create ") {
                let title = rest.trim().to_owned();
                if !title.is_empty() {
                    if let Ok(v) = client.call("task.create", json!({"title": title})).await {
                        state.messages.push(Message {
                            role: Role::System,
                            text: format!("task created: {}", v["id"].as_str().unwrap_or("?")),
                        });
                    }
                }
            } else if let Some(id) = input.trim().strip_prefix("/task done ") {
                let id = id.trim().to_owned();
                if client.call("task.close", json!({"id": id})).await.is_ok() {
                    state.messages.push(Message {
                        role: Role::System,
                        text: format!("task {id} closed"),
                    });
                }
            } else if let Some(arg) = input.trim().strip_prefix("/cowork ") {
                match arg.trim() {
                    "on" | "off" => {
                        let enabled = arg.trim() == "on";
                        let session_id = state.session_id.clone();
                        if client
                            .call(
                                "cowork.set",
                                json!({ "session_id": session_id, "enabled": enabled }),
                            )
                            .await
                            .is_ok()
                        {
                            state.messages.push(Message {
                                role: Role::System,
                                text: format!(
                                    "cowork mode {}",
                                    if enabled { "enabled" } else { "disabled" }
                                ),
                            });
                        }
                    }
                    "status" => {
                        let cowork_on = false; // ponytail: read from session state when wired
                        state.messages.push(Message {
                            role: Role::System,
                            text: format!("cowork: {}", if cowork_on { "on" } else { "off" }),
                        });
                    }
                    _ => {
                        state.messages.push(Message {
                            role: Role::System,
                            text: "usage: /cowork on|off|status".into(),
                        });
                    }
                }
            } else if let Some(rest) = input.trim().strip_prefix("/stage ") {
                // /stage <tool> <json-args>
                if let Some((tool, json_args)) = rest.split_once(' ') {
                    let msg = match state.staging_queue.stage(tool, json_args) {
                        Ok(s) => s,
                        Err(e) => e,
                    };
                    state.messages.push(Message {
                        role: Role::System,
                        text: msg,
                    });
                } else {
                    state.messages.push(Message {
                        role: Role::System,
                        text: "usage: /stage <tool> <json-args>".into(),
                    });
                }
            } else if let Some(rest) = input.trim().strip_prefix("/unstage") {
                // /unstage [N]
                let n: Option<usize> = rest.trim().parse().ok();
                let msg = state.staging_queue.unstage(n);
                state.messages.push(Message {
                    role: Role::System,
                    text: msg,
                });
                for item in state.staging_queue.list() {
                    state.messages.push(Message {
                        role: Role::System,
                        text: item,
                    });
                }
            } else if input.trim() == "/run" {
                let actions = state.staging_queue.drain();
                if actions.is_empty() {
                    state.messages.push(Message {
                        role: Role::System,
                        text: "no staged actions".into(),
                    });
                } else {
                    for action in actions {
                        let payload = json!({"tool": action.tool, "args": action.args});
                        let result = client.call("tool.call", payload).await;
                        let text = match result {
                            Ok(v) => format!("\u{25b8} {v}"),
                            Err(e) => format!("\u{25b8} error: {e}"),
                        };
                        state.messages.push(Message {
                            role: Role::System,
                            text,
                        });
                    }
                }
            } else {
                submit(&input, state, client).await?;
            }
        }

        KeyCode::Char(c) => {
            state.input.push(c);
        }

        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

fn render(frame: &mut ratatui::Frame, state: &AppState) {
    let area = frame.area();

    // Split horizontally if wide enough and context rail is visible.
    let (main_area, rail_area) = if state.context_rail_visible && area.width >= 100 {
        let cols = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(context_rail::ContextRail::WIDTH),
        ])
        .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(3),
    ])
    .split(main_area);

    // -- Status bar -----------------------------------------------------------
    let ctx = ModuleCtx {
        session_id: &state.session_id,
        mode: state.mode.as_deref(),
        tier: state.tier.as_deref(),
        pending: state.pending_task_id.is_some(),
    };
    let status_text = render_status_bar(&ctx);
    let status = Paragraph::new(status_text).style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(status, chunks[0]);

    // -- Messages area --------------------------------------------------------
    let inner_height = chunks[1].height.saturating_sub(2) as usize;

    let all_lines: Vec<Line> = state
        .messages
        .iter()
        .map(|m| {
            let prefix = match m.role {
                Role::User => Span::styled("you: ", Style::default().add_modifier(Modifier::BOLD)),
                Role::System => Span::raw("smdjad: "),
            };
            Line::from(vec![prefix, Span::raw(m.text.clone())])
        })
        .collect();

    let visible_lines: Vec<Line> = if all_lines.len() > inner_height {
        all_lines[all_lines.len() - inner_height..].to_vec()
    } else {
        all_lines
    };

    let messages = Paragraph::new(visible_lines)
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[1]);

    // -- Input box ------------------------------------------------------------
    let input_display = format!("> {}_", state.input);
    let input_widget = Paragraph::new(input_display).block(Block::default().borders(Borders::ALL));
    frame.render_widget(input_widget, chunks[2]);

    // -- Context rail ---------------------------------------------------------
    if let Some(rail_rect) = rail_area {
        // Build placeholder slots (real data wired when WorkingMemory is exposed).
        let slots = vec![context_rail::ContextSlot {
            name: "context".into(),
            used: 0,
            total: 200_000,
        }];
        let rail = context_rail::ContextRail::new(slots);
        frame.render_widget(rail, rail_rect);
    }

    // -- Diff overlay ---------------------------------------------------------
    if let Some((_idx, ref lines)) = state.diff_overlay {
        // Centre 80% of the main area.
        // Truncation is intentional: pixel-aligned terminal dimensions are
        // always well within u16 range; f32 precision is fine for rounding.
        #[allow(
            clippy::cast_lossless,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let ow = (f32::from(main_area.width) * 0.8) as u16;
        #[allow(
            clippy::cast_lossless,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let oh = (f32::from(main_area.height) * 0.8) as u16;
        let ox = main_area.x + (main_area.width.saturating_sub(ow)) / 2;
        let oy = main_area.y + (main_area.height.saturating_sub(oh)) / 2;
        let overlay_rect = ratatui::layout::Rect::new(ox, oy, ow, oh);

        let visible: Vec<Line<'_>> = lines
            .iter()
            .skip(state.diff_scroll)
            .take(oh as usize)
            .map(|l| Line::raw(l.clone()))
            .collect();

        let diff_widget =
            Paragraph::new(visible).block(Block::default().borders(Borders::ALL).title("diff"));
        frame.render_widget(diff_widget, overlay_rect);
    }
}

// ---------------------------------------------------------------------------
// Cleanup guard — always restores terminal even on panic
// ---------------------------------------------------------------------------

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
#[allow(clippy::too_many_lines)] // event loop + render + poll in a single binary entry point
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let sock = socket_path(cli.sock);

    let mut client = Client::connect(&sock).await.with_context(|| {
        format!(
            "smdjad is not running — start it with: smj daemon start\n(tried socket: {})",
            sock.display()
        )
    })?;

    let session_resp = client
        .call("session.create", json!({ "title": "smedja" }))
        .await
        .map_err(|e| anyhow::anyhow!("session.create failed: {e}"))?;
    let session_id = session_resp["session_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();

    tracing::debug!(session_id = %session_id, "session created");

    let mut state = AppState {
        session_id,
        mode: cli.mode,
        tier: cli.tier,
        messages: Vec::new(),
        input: String::new(),
        quit: false,
        pending_task_id: None,
        last_poll: None,
        turn_n: 0,
        turn_submitted_at: None,
        current_block: None,
        block_store: blocks::BlockStore::new(),
        block_browser_open: false,
        block_browser_cursor: 0,
        clipboard: None,
        diff_overlay: None,
        diff_scroll: 0,
        staging_queue: staging::StagingQueue::new(),
        context_rail_visible: true,
    };

    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout(), EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    loop {
        terminal.draw(|f| render(f, &state))?;

        let event_available =
            tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(100)))
                .await
                .context("poll task panicked")??;

        if event_available {
            let ev = tokio::task::spawn_blocking(event::read)
                .await
                .context("read task panicked")??;

            if let Event::Key(key) = ev {
                handle_key(key, &mut state, &mut client).await?;
            }
        }

        // Poll for pending task result (every 500 ms).
        if let Some(task_id) = state.pending_task_id.clone() {
            let should_poll = state
                .last_poll
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(500));
            if should_poll {
                state.last_poll = Some(std::time::Instant::now());
                if let Ok(v) = client.call("task.get", json!({"id": task_id})).await {
                    let status = v["status"].as_str().unwrap_or("");
                    if status == "complete" {
                        let response = v["response"].as_str().unwrap_or("(no response)").to_owned();
                        let elapsed_ms = state.turn_submitted_at.map_or(0, |t| {
                            u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                        });
                        state.turn_submitted_at = None;
                        if let Some(mut block) = state.current_block.take() {
                            block.push_text(&response);
                            block.complete(elapsed_ms);
                            let width = 80usize;
                            for line in block.render_lines(width) {
                                state.messages.push(Message {
                                    role: Role::System,
                                    text: line,
                                });
                            }
                            // Store completed block in history.
                            state.block_store.push(block);
                        } else {
                            state.messages.push(Message {
                                role: Role::System,
                                text: response,
                            });
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                    } else if status == "failed" {
                        if let Some(mut block) = state.current_block.take() {
                            block.fail();
                            for line in block.render_lines(80) {
                                state.messages.push(Message {
                                    role: Role::System,
                                    text: line,
                                });
                            }
                            // Store failed block in history too.
                            state.block_store.push(block);
                        } else {
                            state.messages.push(Message {
                                role: Role::System,
                                text: "turn failed".to_owned(),
                            });
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                    }
                }
            }
        }

        if state.quit {
            break;
        }
    }

    Ok(())
}
