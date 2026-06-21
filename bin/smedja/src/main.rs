mod blocks;
mod statusbar;

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

async fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    client: &mut Client,
) -> Result<()> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.quit = true;
        }
        KeyCode::Esc => {
            state.quit = true;
        }
        KeyCode::Backspace => {
            state.input.pop();
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

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(3),
    ])
    .split(area);

    // -- Status bar (replaces old title bar) --------------------------------
    let ctx = ModuleCtx {
        session_id: &state.session_id,
        mode: state.mode.as_deref(),
        tier: state.tier.as_deref(),
        pending: state.pending_task_id.is_some(),
    };
    let status_text = render_status_bar(&ctx);
    let status = Paragraph::new(status_text).style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(status, chunks[0]);

    // -- Messages area ------------------------------------------------------
    let inner_height = chunks[1].height.saturating_sub(2) as usize; // subtract borders

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

    // Pin to bottom: take the last `inner_height` lines.
    let visible_lines: Vec<Line> = if all_lines.len() > inner_height {
        all_lines[all_lines.len() - inner_height..].to_vec()
    } else {
        all_lines
    };

    let messages = Paragraph::new(visible_lines)
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[1]);

    // -- Input box ----------------------------------------------------------
    let input_display = format!("> {}_", state.input);
    let input_widget = Paragraph::new(input_display).block(Block::default().borders(Borders::ALL));
    frame.render_widget(input_widget, chunks[2]);
}

// ---------------------------------------------------------------------------
// Cleanup guard — always restores terminal even on panic
// ---------------------------------------------------------------------------

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort: ignore errors during cleanup.
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

    // Connect before entering raw mode so errors surface cleanly.
    let mut client = Client::connect(&sock).await.with_context(|| {
        format!(
            "smdjad is not running — start it with: smj daemon start\n(tried socket: {})",
            sock.display()
        )
    })?;

    // Create a session.
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
    };

    // Enter alternate screen and raw mode — guard restores on drop.
    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout(), EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    // Event loop — poll for crossterm events with a short timeout so the
    // loop stays responsive without burning CPU.
    loop {
        terminal.draw(|f| render(f, &state))?;

        // Block in a spawn_blocking call so the async runtime stays free.
        // poll() with 100 ms timeout gives ≤100 ms input latency.
        let event_available =
            tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(100)))
                .await
                .context("poll task panicked")??;

        if event_available {
            let ev = tokio::task::spawn_blocking(event::read)
                .await
                .context("read task panicked")??;

            // All other events (resize, focus, mouse, …) cause a redraw on the next tick.
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
                        } else {
                            state.messages.push(Message {
                                role: Role::System,
                                text: "turn failed".to_owned(),
                            });
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                    }
                    // else: still planned or in_progress — keep polling
                }
            }
        }

        if state.quit {
            break;
        }
    }

    Ok(())
}
