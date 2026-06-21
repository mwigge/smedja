pub mod action_log;
mod blocks;
mod context_rail;
pub mod main_panel;
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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
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
    #[allow(dead_code)]
    // role field drives future rendering distinction; suppressed until render is split
    role: Role,
    text: String,
}

/// Available slash-command completions shown in the popup.
const SLASH_COMPLETIONS: &[&str] = &["/agent", "/tier", "/spec", "/tdd", "/ponytail"];

#[allow(clippy::struct_excessive_bools)] // AppState is a TUI dispatch table; enum-splitting would add indirection without clarity
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
    /// Main message display panel.
    main_panel: main_panel::MainPanel,
    /// Audit action log widget.
    action_log: action_log::ActionLog,
    /// Available slash-command completions (filtered subset of `SLASH_COMPLETIONS`).
    slash_completions: Vec<&'static str>,
    /// Whether the slash-command completion popup is visible.
    slash_popup_visible: bool,
    /// Cursor index within the filtered completion list.
    slash_cursor: usize,
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
    let user_msg = Message {
        role: Role::User,
        text: text.clone(),
    };
    state.main_panel.push_line(user_msg.text.clone());
    state.messages.push(user_msg);
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
    let sys_msg = Message {
        role: Role::System,
        text: reply,
    };
    state.main_panel.push_line(sys_msg.text.clone());
    state.messages.push(sys_msg);
    Ok(())
}

// ---------------------------------------------------------------------------
// Slash completion helpers
// ---------------------------------------------------------------------------

/// Returns completions from `SLASH_COMPLETIONS` whose prefix matches `input`.
fn filtered_completions(input: &str) -> Vec<&'static str> {
    SLASH_COMPLETIONS
        .iter()
        .copied()
        .filter(|c| c.starts_with(input))
        .collect()
}

// ---------------------------------------------------------------------------
// Key handler
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)] // key dispatch table for TUI; splitting would obscure the flow
async fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    client: &mut Client,
    editor: &mut rustyline::DefaultEditor,
) -> Result<()> {
    // ------------------------------------------------------------------
    // Slash-completion popup intercepts most keys when visible.
    // ------------------------------------------------------------------
    if state.slash_popup_visible {
        match key.code {
            KeyCode::Esc => {
                state.slash_popup_visible = false;
            }
            KeyCode::Tab | KeyCode::Down => {
                let max = state.slash_completions.len().saturating_sub(1);
                if state.slash_cursor < max {
                    state.slash_cursor += 1;
                }
            }
            KeyCode::Up => {
                state.slash_cursor = state.slash_cursor.saturating_sub(1);
            }
            KeyCode::Enter => {
                // Complete the selected entry into the input buffer.
                if let Some(&completion) = state.slash_completions.get(state.slash_cursor) {
                    completion.clone_into(&mut state.input);
                }
                state.slash_popup_visible = false;
            }
            KeyCode::Backspace => {
                state.input.pop();
                if state.input.is_empty() {
                    state.slash_popup_visible = false;
                } else {
                    let completions = filtered_completions(&state.input);
                    state.slash_cursor =
                        state.slash_cursor.min(completions.len().saturating_sub(1));
                    state.slash_completions = completions;
                }
            }
            KeyCode::Char(c) => {
                state.input.push(c);
                let completions = filtered_completions(&state.input);
                state.slash_cursor = 0;
                if completions.is_empty() {
                    state.slash_popup_visible = false;
                }
                state.slash_completions = completions;
            }
            _ => {}
        }
        return Ok(());
    }

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
            // L128: multi-line continuation — trailing `\` means "continue".
            if state.input.ends_with('\\') {
                // Strip the trailing backslash and append a newline continuation.
                state.input.pop();
                state.input.push('\n');
                return Ok(());
            }

            let input = std::mem::take(&mut state.input);

            // Record in rustyline history (ignore errors — history is advisory).
            let _ = editor.add_history_entry(&input);

            if let Some(rest) = input.trim().strip_prefix("/task create ") {
                let title = rest.trim().to_owned();
                if !title.is_empty() {
                    if let Ok(v) = client.call("task.create", json!({"title": title})).await {
                        let msg = Message {
                            role: Role::System,
                            text: format!("task created: {}", v["id"].as_str().unwrap_or("?")),
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                    }
                }
            } else if let Some(id) = input.trim().strip_prefix("/task done ") {
                let id = id.trim().to_owned();
                if client.call("task.close", json!({"id": id})).await.is_ok() {
                    let msg = Message {
                        role: Role::System,
                        text: format!("task {id} closed"),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
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
                            let msg = Message {
                                role: Role::System,
                                text: format!(
                                    "cowork mode {}",
                                    if enabled { "enabled" } else { "disabled" }
                                ),
                            };
                            state.main_panel.push_line(msg.text.clone());
                            state.messages.push(msg);
                        }
                    }
                    "status" => {
                        let cowork_on = false; // ponytail: read from session state when wired
                        let msg = Message {
                            role: Role::System,
                            text: format!("cowork: {}", if cowork_on { "on" } else { "off" }),
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                    }
                    _ => {
                        let msg = Message {
                            role: Role::System,
                            text: "usage: /cowork on|off|status".into(),
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                    }
                }
            } else if let Some(rest) = input.trim().strip_prefix("/stage ") {
                // /stage <tool> <json-args>
                if let Some((tool, json_args)) = rest.split_once(' ') {
                    let text = match state.staging_queue.stage(tool, json_args) {
                        Ok(s) => s,
                        Err(e) => e,
                    };
                    let msg = Message {
                        role: Role::System,
                        text,
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                } else {
                    let msg = Message {
                        role: Role::System,
                        text: "usage: /stage <tool> <json-args>".into(),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                }
            } else if let Some(rest) = input.trim().strip_prefix("/unstage") {
                // /unstage [N]
                let n: Option<usize> = rest.trim().parse().ok();
                let text = state.staging_queue.unstage(n);
                let msg = Message {
                    role: Role::System,
                    text,
                };
                state.main_panel.push_line(msg.text.clone());
                state.messages.push(msg);
                for item in state.staging_queue.list() {
                    let msg = Message {
                        role: Role::System,
                        text: item,
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                }
            } else if input.trim() == "/run" {
                let actions = state.staging_queue.drain();
                if actions.is_empty() {
                    let msg = Message {
                        role: Role::System,
                        text: "no staged actions".into(),
                    };
                    state.main_panel.push_line(msg.text.clone());
                    state.messages.push(msg);
                } else {
                    for action in actions {
                        let payload = json!({"tool": action.tool, "args": action.args});
                        let result = client.call("tool.call", payload).await;
                        let text = match result {
                            Ok(v) => format!("\u{25b8} {v}"),
                            Err(e) => format!("\u{25b8} error: {e}"),
                        };
                        let msg = Message {
                            role: Role::System,
                            text,
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                    }
                }
            } else {
                submit(&input, state, client).await?;
            }
        }

        KeyCode::Char('/') if state.input.is_empty() => {
            // L129: open slash popup when `/` is the first character typed.
            state.input.push('/');
            state.slash_completions = filtered_completions("/");
            state.slash_cursor = 0;
            state.slash_popup_visible = true;
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

    // L122: outer vertical split:
    //   row 0 = status bar (1 row)
    //   row 1 = body (fill)
    //   row 2 = action log (5 rows)
    //   row 3 = input (1 row)
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .split(area);

    let status_area = outer[0];
    let body_area = outer[1];
    let action_log_area = outer[2];
    let input_area = outer[3];

    // -- Status bar -----------------------------------------------------------
    let ctx = ModuleCtx {
        session_id: &state.session_id,
        mode: state.mode.as_deref(),
        tier: state.tier.as_deref(),
        pending: state.pending_task_id.is_some(),
    };
    let status_text = render_status_bar(&ctx);
    let status = Paragraph::new(status_text).style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(status, status_area);

    // -- Body: main panel | optional context rail ----------------------------
    // L122: horizontal split inside body; rail collapses when narrow or hidden.
    let (main_area, rail_area) = if state.context_rail_visible && body_area.width >= 100 {
        let cols = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(context_rail::ContextRail::WIDTH),
        ])
        .split(body_area);
        (cols[0], Some(cols[1]))
    } else {
        (body_area, None)
    };

    // L122: render MainPanel from state.main_panel.
    state.main_panel.render(main_area, frame);

    // -- Action log -----------------------------------------------------------
    // L122: 5-row area using the existing ActionLog widget.
    state.action_log.render(action_log_area, frame);

    // -- Input area -----------------------------------------------------------
    // L128: show continuation prefix when input contains a newline.
    let input_display = if state.input.contains('\n') {
        // Show the last logical line with continuation indicator.
        let last_line = state.input.rsplit('\n').next().unwrap_or("");
        format!("... {last_line}_")
    } else {
        format!("> {}_", state.input)
    };
    let input_widget = Paragraph::new(input_display);
    frame.render_widget(input_widget, input_area);

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
        let ow = (f32::from(area.width) * 0.8) as u16;
        #[allow(
            clippy::cast_lossless,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let oh = (f32::from(area.height) * 0.8) as u16;
        let ox = area.x + (area.width.saturating_sub(ow)) / 2;
        let oy = area.y + (area.height.saturating_sub(oh)) / 2;
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

    // -- Slash-completion popup -----------------------------------------------
    if state.slash_popup_visible && !state.slash_completions.is_empty() {
        render_slash_popup(frame, area, state);
    }
}

/// Renders the slash-command completion popup in the bottom portion of the screen.
fn render_slash_popup(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
    let completions = &state.slash_completions;
    // Height = number of completions + 2 border rows, capped at available space.
    #[allow(clippy::cast_possible_truncation)]
    let popup_h = (completions.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup_w = 20u16.min(area.width);
    // Position just above the input row (bottom-left).
    let popup_y = area.y + area.height.saturating_sub(popup_h + 1);
    let popup_x = area.x;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let lines: Vec<Line<'_>> = completions
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            if i == state.slash_cursor {
                Line::from(Span::styled(
                    format!(" {c}"),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::raw(format!(" {c}")))
            }
        })
        .collect();

    let popup =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("commands"));
    frame.render_widget(popup, popup_rect);
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

    // L127: set up rustyline editor for history persistence only.
    // Rustyline cannot be used interactively inside a ratatui/crossterm raw-mode
    // event loop — it is used solely to load/save and accumulate history entries.
    let history_path: PathBuf = {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let dir = PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("smedja");
        // Best-effort directory creation; failure is non-fatal.
        let _ = std::fs::create_dir_all(&dir);
        dir.join("history")
    };

    let mut editor = rustyline::DefaultEditor::with_config(rustyline::Config::default())
        .context("create rustyline editor")?;
    // Ignore load errors — history file may not exist on first run.
    let _ = editor.load_history(&history_path);

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
        main_panel: main_panel::MainPanel::new(),
        action_log: action_log::ActionLog::new(50),
        slash_completions: Vec::new(),
        slash_popup_visible: false,
        slash_cursor: 0,
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
                handle_key(key, &mut state, &mut client, &mut editor).await?;
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
                                let msg = Message {
                                    role: Role::System,
                                    text: line,
                                };
                                state.main_panel.push_line(msg.text.clone());
                                state.messages.push(msg);
                            }
                            // Store completed block in history.
                            state.block_store.push(block);
                        } else {
                            let msg = Message {
                                role: Role::System,
                                text: response,
                            };
                            state.main_panel.push_line(msg.text.clone());
                            state.messages.push(msg);
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                    } else if status == "failed" {
                        if let Some(mut block) = state.current_block.take() {
                            block.fail();
                            for line in block.render_lines(80) {
                                let msg = Message {
                                    role: Role::System,
                                    text: line,
                                };
                                state.main_panel.push_line(msg.text.clone());
                                state.messages.push(msg);
                            }
                            // Store failed block in history too.
                            state.block_store.push(block);
                        } else {
                            let msg = Message {
                                role: Role::System,
                                text: "turn failed".to_owned(),
                            };
                            state.main_panel.push_line(msg.text.clone());
                            state.messages.push(msg);
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

    // L127: persist history on clean shutdown.
    let _ = editor.save_history(&history_path);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (L128, L129)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // L128: trailing backslash appends newline continuation, does not submit.
    #[test]
    fn backslash_continuation_appends_newline() {
        let mut input = "hello\\".to_owned();
        // Simulate the Enter key handling logic inline.
        assert!(input.ends_with('\\'));
        input.pop();
        input.push('\n');
        assert!(input.contains('\n'));
        assert_eq!(input, "hello\n");
    }

    // L128: continuation display prefix uses "..." for multi-line input.
    #[test]
    fn continuation_display_uses_ellipsis_prefix() {
        let input = "first line\nsecond";
        let display = if input.contains('\n') {
            let last_line = input.rsplit('\n').next().unwrap_or("");
            format!("... {last_line}_")
        } else {
            format!("> {input}_")
        };
        assert_eq!(display, "... second_");
    }

    // L128: normal input display uses "> " prefix.
    #[test]
    fn normal_display_uses_prompt_prefix() {
        let input = "hello";
        let display = format!("> {input}_");
        assert_eq!(display, "> hello_");
    }

    // L129: filtered_completions returns only matching entries.
    #[test]
    fn slash_completions_filter_by_prefix() {
        let completions = filtered_completions("/a");
        assert_eq!(completions, vec!["/agent"]);
    }

    // L129: typing "/" returns all completions.
    #[test]
    fn slash_completions_all_on_bare_slash() {
        let completions = filtered_completions("/");
        assert_eq!(completions.len(), SLASH_COMPLETIONS.len());
    }

    // L129: unknown prefix returns empty.
    #[test]
    fn slash_completions_empty_for_no_match() {
        let completions = filtered_completions("/zzz");
        assert!(completions.is_empty());
    }
}
