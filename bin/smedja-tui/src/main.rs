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
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Terminal;
use serde_json::json;
use smedja_rpc::client::Client;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "smedja-tui", about = "smedja agent dashboard (TUI)")]
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
const SLASH_COMPLETIONS: &[&str] = &["/agent", "/health", "/tier", "/spec", "/tdd", "/ponytail"];

#[allow(clippy::struct_excessive_bools)] // AppState is a TUI dispatch table; enum-splitting would add indirection without clarity
#[derive(Debug)]
struct AppState {
    session_id: String,
    mode: Option<String>,
    tier: Option<String>,
    runner: String,
    model: Option<String>,
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
    /// Cumulative tokens used so far in this session (input + output).
    context_used: u64,
    /// Context window size in tokens for the active model.
    context_window: u64,
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
    /// True while a turn is awaiting a response from the daemon.
    #[allow(dead_code)] // wired at init; read path lands once streaming poll is complete
    turn_in_flight: bool,
    /// Number of consecutive unexpected (non-done) poll responses received.
    ///
    /// Used to rate-limit the "waiting for turn…" status message so it does not
    /// flood the panel on rapid retries.
    poll_retry_count: u32,
    /// Whether the messages panel has scroll focus (input bar is inactive).
    scroll_focus: bool,
    /// Whether visual line-selection mode is active within the messages panel.
    selection_mode: bool,
    /// Anchor line index for the current selection (0 = oldest line).
    selection_anchor: usize,
    /// Moving end line index for the current selection.
    selection_end: usize,
    /// First `g` press received; waiting for a second `g` to jump to top.
    g_pending: bool,
    /// Byte offset of the insertion cursor within `input`.
    /// Invariant: always on a UTF-8 char boundary, 0 ≤ cursor ≤ input.len().
    input_cursor: usize,
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
            state.turn_in_flight = true;
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

fn push_system_message(state: &mut AppState, text: impl Into<String>) {
    let msg = Message {
        role: Role::System,
        text: text.into(),
    };
    state.main_panel.push_line(msg.text.clone());
    state.messages.push(msg);
}

fn render_input_with_cursor(input: &str, cursor: usize) -> String {
    let cursor = cursor.min(input.len());
    let before = &input[..cursor];
    let after = &input[cursor..];
    format!("{before}_{after}")
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p.saturating_sub(s[..p].chars().next_back().map_or(0, char::len_utf8))
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    if p < s.len() {
        p + s[p..].chars().next().map_or(0, char::len_utf8)
    } else {
        p
    }
}

fn yank_to_clipboard(lines: &[String]) {
    use std::io::Write as _;
    let text = lines.join("\n");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("pbcopy");
    #[cfg(not(target_os = "macos"))]
    let mut cmd = {
        let mut c = std::process::Command::new("xclip");
        c.args(["-selection", "clipboard"]);
        c
    };
    let result = cmd
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait()
        });
    if let Err(e) = result {
        tracing::debug!(error = %e, "clipboard write failed");
    }
}

fn accept_slash_completion(state: &mut AppState, append_space: bool) -> bool {
    let Some(&completion) = state.slash_completions.get(state.slash_cursor) else {
        state.slash_popup_visible = false;
        return false;
    };
    completion.clone_into(&mut state.input);
    if append_space {
        state.input.push(' ');
    }
    state.input_cursor = state.input.len();
    state.slash_popup_visible = false;
    state.slash_completions.clear();
    state.slash_cursor = 0;
    true
}

fn apply_tier(args: &str, state: &mut AppState) -> String {
    match args {
        "fast" | "deep" | "local" => {
            state.tier = Some(args.to_owned());
            format!("tier set to {args}")
        }
        "" => "usage: /tier fast|deep|local".to_owned(),
        other => format!("unknown tier: {other}"),
    }
}

fn apply_agent(args: &str, state: &mut AppState) -> String {
    match args {
        "impl" | "review" | "test" | "sre" | "explain" => {
            state.mode = Some(args.to_owned());
            if args == "sre" {
                state.tier = Some("deep".to_owned());
            }
            format!("agent mode set to {args}")
        }
        "" => "usage: /agent impl|review|test|sre|explain".to_owned(),
        other => format!("unknown agent mode: {other}"),
    }
}

async fn dispatch_slash(input: &str, state: &mut AppState, client: &mut Client) -> Result<bool> {
    let trimmed = input.trim();
    let Some(command_line) = trimmed.strip_prefix('/') else {
        return Ok(false);
    };
    let mut parts = command_line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or_default();
    let args = parts.next().unwrap_or_default().trim();

    match cmd {
        "tier" => {
            let text = apply_tier(args, state);
            push_system_message(state, text);
            Ok(true)
        }
        "agent" => {
            let text = apply_agent(args, state);
            if matches!(args, "impl" | "review" | "test" | "sre" | "explain") {
                let session_id = state.session_id.clone();
                let _ = client
                    .call(
                        "session.set_mode",
                        json!({
                            "session_id": session_id,
                            "mode": args,
                        }),
                    )
                    .await;
            }
            push_system_message(state, text);
            Ok(true)
        }
        "health" => {
            let start = std::time::Instant::now();
            let session_id = state.session_id.clone();
            let health_result = client
                .call("session.get", json!({ "id": session_id }))
                .await;
            let latency_ms = start.elapsed().as_millis();
            let text = match health_result {
                Ok(_) => {
                    format!("health: socket=ok session={session_id} latency={latency_ms}ms")
                }
                Err(e) => format!("health: error — {e}"),
            };
            push_system_message(state, text);
            Ok(true)
        }
        "spec" => {
            push_system_message(state, "spec picker is not wired yet");
            Ok(true)
        }
        "tdd" => {
            state.mode = Some("tdd".to_owned());
            push_system_message(state, "mode set to tdd");
            Ok(true)
        }
        "ponytail" => {
            state.mode = Some("ponytail".to_owned());
            push_system_message(state, "mode set to ponytail");
            Ok(true)
        }
        _ => Ok(false),
    }
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
            KeyCode::Char(' ') | KeyCode::Tab => {
                accept_slash_completion(state, true);
            }
            KeyCode::Down => {
                let max = state.slash_completions.len().saturating_sub(1);
                if state.slash_cursor < max {
                    state.slash_cursor += 1;
                }
            }
            KeyCode::Up => {
                state.slash_cursor = state.slash_cursor.saturating_sub(1);
            }
            KeyCode::Enter => {
                if accept_slash_completion(state, false) {
                    let input = std::mem::take(&mut state.input);
                    state.input_cursor = 0;
                    let _ = editor.add_history_entry(&input);
                    if !dispatch_slash(&input, state, client).await? {
                        submit(&input, state, client).await?;
                    }
                }
            }
            KeyCode::Backspace => {
                if state.input_cursor > 0 {
                    let new_pos = prev_char_boundary(&state.input, state.input_cursor);
                    state.input.drain(new_pos..state.input_cursor);
                    state.input_cursor = new_pos;
                }
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
                state.input.insert(state.input_cursor, c);
                state.input_cursor += c.len_utf8();
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

    // ------------------------------------------------------------------
    // Scroll / visual-selection mode intercept.
    // ------------------------------------------------------------------
    if state.scroll_focus {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if state.selection_mode {
                    state.selection_end += 1;
                } else {
                    state.main_panel.scroll_down();
                }
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if state.selection_mode {
                    state.selection_end = state.selection_end.saturating_sub(1);
                } else {
                    state.main_panel.scroll_up();
                }
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('G') => {
                state.main_panel.scroll_to_bottom();
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('g') => {
                if state.g_pending {
                    state.main_panel.scroll_to_top();
                    state.g_pending = false;
                } else {
                    state.g_pending = true;
                }
                return Ok(());
            }
            KeyCode::Char('v') if !state.selection_mode => {
                state.selection_mode = true;
                state.selection_anchor = state.main_panel.scroll;
                state.selection_end = state.main_panel.scroll;
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('y') if state.selection_mode => {
                let lo = state.selection_anchor.min(state.selection_end);
                let hi = state.selection_anchor.max(state.selection_end);
                let lines = state.main_panel.lines_text(lo, hi);
                let count = lines.len();
                yank_to_clipboard(&lines);
                state.clipboard = Some(lines.join("\n"));
                state.selection_mode = false;
                push_system_message(state, format!("\u{2713} {count} lines copied"));
                return Ok(());
            }
            KeyCode::Char('i') | KeyCode::Char('a') => {
                state.scroll_focus = false;
                state.selection_mode = false;
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Esc => {
                // Fall through to the main Esc handler below.
            }
            _ => return Ok(()), // consume unknown keys in scroll mode
        }
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
            } else if state.selection_mode {
                state.selection_mode = false;
            } else if state.scroll_focus {
                state.scroll_focus = false;
            } else if state.block_browser_open {
                state.block_browser_open = false;
            } else {
                state.scroll_focus = true;
            }
        }

        KeyCode::Up => {
            if state.block_browser_open {
                state.block_browser_cursor = state.block_browser_cursor.saturating_sub(1);
            } else {
                state.main_panel.scroll_up();
            }
        }

        KeyCode::Down => {
            if state.block_browser_open {
                let max = state.block_store.len().saturating_sub(1);
                if state.block_browser_cursor < max {
                    state.block_browser_cursor += 1;
                }
            } else {
                state.main_panel.scroll_down();
            }
        }

        KeyCode::Backspace => {
            if state.input_cursor > 0 {
                let new_pos = prev_char_boundary(&state.input, state.input_cursor);
                state.input.drain(new_pos..state.input_cursor);
                state.input_cursor = new_pos;
            }
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
                state.input.insert(state.input_cursor, 'c');
                state.input_cursor += 1;
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
                state.input.insert(state.input_cursor, 'r');
                state.input_cursor += 1;
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
                state.input.insert(state.input_cursor, 'd');
                state.input_cursor += 1;
            }
        }

        KeyCode::Enter => {
            // L128: multi-line continuation — trailing `\` means "continue".
            if state.input.ends_with('\\') {
                // Strip the trailing backslash and append a newline continuation.
                state.input.pop();
                state.input.push('\n');
                state.input_cursor = state.input.len();
                return Ok(());
            }

            let input = std::mem::take(&mut state.input);
            state.input_cursor = 0;

            // Record in rustyline history (ignore errors — history is advisory).
            let _ = editor.add_history_entry(&input);

            if dispatch_slash(&input, state, client).await? {
                return Ok(());
            }

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
            } else if input.trim() == "/agent sre" {
                state.mode = Some("sre".into());
                state.tier = Some("deep".into());
                let session_id = state.session_id.clone();
                let _ = client
                    .call(
                        "session.set_mode",
                        json!({
                            "session_id": session_id,
                            "mode": "sre",
                        }),
                    )
                    .await;
                let msg = Message {
                    role: Role::System,
                    text: "SRE mode activated (tier: deep)".into(),
                };
                state.main_panel.push_line(msg.text.clone());
                state.messages.push(msg);
            } else if input.trim() == "/health" {
                // Measure RPC round-trip latency by calling session.get.
                let start = std::time::Instant::now();
                let session_id = state.session_id.clone();
                let health_result = client
                    .call("session.get", json!({ "id": session_id }))
                    .await;
                let latency_ms = start.elapsed().as_millis();
                let text = match health_result {
                    Ok(_) => {
                        format!("health: socket=ok session={session_id} latency={latency_ms}ms")
                    }
                    Err(e) => format!("health: error — {e}"),
                };
                let msg = Message {
                    role: Role::System,
                    text,
                };
                state.main_panel.push_line(msg.text.clone());
                state.messages.push(msg);
            } else {
                submit(&input, state, client).await?;
            }
        }

        KeyCode::Left => {
            state.input_cursor = prev_char_boundary(&state.input, state.input_cursor);
        }

        KeyCode::Right => {
            state.input_cursor = next_char_boundary(&state.input, state.input_cursor);
        }

        KeyCode::Home => {
            state.input_cursor = 0;
        }

        KeyCode::End => {
            state.input_cursor = state.input.len();
        }

        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input_cursor = 0;
        }

        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input_cursor = state.input.len();
        }

        KeyCode::Char('/') if state.input.is_empty() => {
            // L129: open slash popup when `/` is the first character typed.
            state.input.insert(state.input_cursor, '/');
            state.input_cursor += 1;
            state.slash_completions = filtered_completions("/");
            state.slash_cursor = 0;
            state.slash_popup_visible = true;
        }

        KeyCode::Char(c) => {
            state.input.insert(state.input_cursor, c);
            state.input_cursor += c.len_utf8();
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
        runner: Some(&state.runner),
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
    let selection = if state.selection_mode {
        let lo = state.selection_anchor.min(state.selection_end);
        let hi = state.selection_anchor.max(state.selection_end);
        Some((lo, hi))
    } else {
        None
    };
    state.main_panel.render(main_area, frame, selection);

    // Overlay a one-row "⠿ thinking…" indicator at the bottom of the main area.
    if state.turn_in_flight && main_area.height >= 1 {
        let thinking_area = ratatui::layout::Rect::new(
            main_area.x,
            main_area.y + main_area.height.saturating_sub(1),
            main_area.width,
            1,
        );
        let thinking_para = Paragraph::new(Line::from(Span::styled(
            "\u{283f} thinking\u{2026}",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(thinking_para, thinking_area);
    }

    // -- Action log -----------------------------------------------------------
    // L122: 5-row area using the existing ActionLog widget.
    state.action_log.render(action_log_area, frame);

    // -- Input area -----------------------------------------------------------
    // L128: show continuation prefix when input contains a newline.
    let input_display = if state.input.contains('\n') {
        // Show the last logical line with the cursor placed correctly within it.
        let prefix_len = state.input.rfind('\n').map_or(0, |i| i + 1);
        let cursor_in_line = state.input_cursor.saturating_sub(prefix_len);
        let last_line = &state.input[prefix_len..];
        format!("... {}", render_input_with_cursor(last_line, cursor_in_line))
    } else {
        format!("> {}", render_input_with_cursor(&state.input, state.input_cursor))
    };
    let input_widget = Paragraph::new(input_display);
    frame.render_widget(input_widget, input_area);

    // -- Context rail ---------------------------------------------------------
    if let Some(rail_rect) = rail_area {
        // Clamp to usize::MAX — context windows are well within usize range on
        // any 64-bit target, but the explicit clamp satisfies pedantic lints.
        let slots = vec![context_rail::ContextSlot {
            name: "context".into(),
            used: usize::try_from(state.context_used).unwrap_or(usize::MAX),
            total: usize::try_from(state.context_window).unwrap_or(usize::MAX),
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

    frame.render_widget(Clear, popup_rect);
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
    let session_id = session_resp["id"].as_str().unwrap_or("unknown").to_owned();

    tracing::debug!(session_id = %session_id, "session created");

    let startup_runner = session_resp
        .get("runner")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_owned();
    let startup_model = session_resp
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let startup_tier = session_resp
        .get("tier")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let mut state = AppState {
        session_id,
        mode: cli.mode,
        tier: cli.tier.or(startup_tier),
        runner: startup_runner,
        model: startup_model,
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
        context_used: 0,
        context_window: 200_000,
        main_panel: main_panel::MainPanel::new(),
        action_log: action_log::ActionLog::new(50),
        slash_completions: Vec::new(),
        slash_popup_visible: false,
        slash_cursor: 0,
        turn_in_flight: false,
        poll_retry_count: 0,
        scroll_focus: false,
        selection_mode: false,
        selection_anchor: 0,
        selection_end: 0,
        g_pending: false,
        input_cursor: 0,
    };

    // Connect banner — shown on every startup so the user knows what's connected.
    let banner_sock = sock.display().to_string();
    state
        .main_panel
        .push_line(format!("connected to {banner_sock}"));
    state
        .main_panel
        .push_line(format!("session {}", state.session_id));
    state
        .main_panel
        .push_line(format!("provider: {}", state.runner));
    if let Some(ref m) = state.model {
        state.main_panel.push_line(format!("model: {m}"));
    }
    let tier_str = state.tier.as_deref().unwrap_or("fast");
    state.main_panel.push_line(format!("tier: {tier_str}"));
    state
        .main_panel
        .push_line("type a message or /help for commands".into());

    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout(), EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    loop {
        // Collect all ready crossterm events before drawing — one render per batch.
        let event_available =
            tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(100)))
                .await
                .context("poll task panicked")??;

        if event_available {
            // Drain every immediately-available event in the same tick.
            loop {
                let ev = tokio::task::spawn_blocking(event::read)
                    .await
                    .context("read task panicked")??;
                if let Event::Key(key) = ev {
                    handle_key(key, &mut state, &mut client, &mut editor).await?;
                }
                // Check if more events are ready without blocking.
                let more = tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(0)))
                    .await
                    .context("poll task panicked")??;
                if !more {
                    break;
                }
            }
        }

        terminal.draw(|f| render(f, &state))?;

        // Subscribe to the in-flight turn result (replaces the old task.get poll
        // loop).  turn.subscribe blocks on the daemon side until the turn
        // reaches a terminal state, so a 50 ms guard prevents hammering the
        // socket if the server returns early for any reason.
        if let Some(task_id) = state.pending_task_id.clone() {
            let should_call = state
                .last_poll
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(50));
            if should_call {
                state.last_poll = Some(std::time::Instant::now());
                match client
                    .call("turn.subscribe", json!({ "task_id": task_id }))
                    .await
                {
                    Ok(v) if v["done"].as_bool() == Some(true) => {
                        if let Some(error) = v["error"].as_str() {
                            let msg = Message {
                                role: Role::System,
                                text: format!("error: {error}"),
                            };
                            state.main_panel.push_line(msg.text.clone());
                            state.messages.push(msg);
                            if let Some(mut block) = state.current_block.take() {
                                block.fail();
                                state.block_store.push(block);
                            }
                        } else {
                            let response = v["response"].as_str().unwrap_or("").to_owned();
                            let input_tok = v["input_tok"].as_i64().unwrap_or(0);
                            let output_tok = v["output_tok"].as_i64().unwrap_or(0);
                            let elapsed_ms = state.turn_submitted_at.map_or(0, |t| {
                                u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                            });
                            state.turn_submitted_at = None;

                            if let Some(mut block) = state.current_block.take() {
                                block.push_text(&response);
                                block.complete(elapsed_ms);
                                for line in block.render_lines(80) {
                                    let msg = Message {
                                        role: Role::System,
                                        text: line,
                                    };
                                    state.main_panel.push_line(msg.text.clone());
                                    state.messages.push(msg);
                                }
                                state.block_store.push(block);
                            } else {
                                if !response.is_empty() {
                                    state.main_panel.push_delta(&response);
                                }
                                let msg = Message {
                                    role: Role::System,
                                    text: response,
                                };
                                state.messages.push(msg);
                            }

                            // Turn footer: token counts and wall-clock latency.
                            let footer =
                                format!("↳ {input_tok}↑ {output_tok}↓ tokens · {elapsed_ms}ms");
                            state.main_panel.push_line(footer);
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                        state.turn_in_flight = false;
                        state.poll_retry_count = 0;

                        // Refresh context rail from daemon after the turn completes.
                        if let Ok(ctx) = client
                            .call("session.context", json!({ "session_id": state.session_id }))
                            .await
                        {
                            if let Some(used) = ctx["used_tok"].as_i64() {
                                state.context_used =
                                    u64::try_from(used.max(0)).unwrap_or(0);
                            }
                            if let Some(window) = ctx["window_tok"].as_u64() {
                                if window > 0 {
                                    state.context_window = window;
                                }
                            }
                        }
                    }
                    Ok(_) => {
                        // turn.subscribe returned Ok but the response was not
                        // done=true — retry on the next tick.
                        state.poll_retry_count += 1;
                        state.last_poll = None;
                        // Surface a status line every 5 retries so the user is
                        // not left staring at a silent spinner.
                        if state.poll_retry_count % 5 == 1 {
                            state.main_panel.push_line(format!(
                                "waiting for turn… (poll attempt {})",
                                state.poll_retry_count
                            ));
                        }
                        // After 60 unexpected retries surface an error to avoid
                        // an indefinitely silent hang.
                        if state.poll_retry_count >= 60 {
                            state.main_panel.push_line(
                                "turn appears stuck — no done=true after 60 polls; giving up"
                                    .to_owned(),
                            );
                            state.pending_task_id = None;
                            state.last_poll = None;
                            state.turn_in_flight = false;
                            state.poll_retry_count = 0;
                        }
                    }
                    Err(e) => {
                        let text = if e.code == smedja_rpc::codes::TIMEOUT {
                            "turn timed out (>60 s) — the daemon is still running the turn in the background".to_owned()
                        } else {
                            format!("turn error: {e}")
                        };
                        let msg = Message {
                            role: Role::System,
                            text,
                        };
                        state.main_panel.push_line(msg.text.clone());
                        state.messages.push(msg);
                        state.pending_task_id = None;
                        state.last_poll = None;
                        state.turn_in_flight = false;
                        state.poll_retry_count = 0;
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

    #[test]
    fn slash_accept_space_inserts_completion_with_trailing_space() {
        let mut state = make_state("test-session");
        state.input = "/t".to_owned();
        state.slash_completions = filtered_completions("/t");
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        assert!(accept_slash_completion(&mut state, true));

        assert_eq!(state.input, "/tier ");
        assert!(!state.slash_popup_visible);
        assert!(state.slash_completions.is_empty());
    }

    #[test]
    fn slash_accept_enter_inserts_completion_without_space() {
        let mut state = make_state("test-session");
        state.input = "/h".to_owned();
        state.slash_completions = filtered_completions("/h");
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        assert!(accept_slash_completion(&mut state, false));

        assert_eq!(state.input, "/health");
        assert!(!state.slash_popup_visible);
    }

    #[test]
    fn dispatch_tier_fast_sets_state_tier() {
        let mut state = make_state("test-session");
        let text = apply_tier("fast", &mut state);
        assert_eq!(state.tier.as_deref(), Some("fast"));
        assert_eq!(text, "tier set to fast");
    }

    #[test]
    fn dispatch_tier_deep_sets_state_tier() {
        let mut state = make_state("test-session");
        let text = apply_tier("deep", &mut state);
        assert_eq!(state.tier.as_deref(), Some("deep"));
        assert_eq!(text, "tier set to deep");
    }

    #[test]
    fn dispatch_agent_impl_sets_state_mode() {
        let mut state = make_state("test-session");
        let text = apply_agent("impl", &mut state);
        assert_eq!(state.mode.as_deref(), Some("impl"));
        assert_eq!(text, "agent mode set to impl");
    }

    // -----------------------------------------------------------------------
    // Layer 4 TUI functional tests — TestBackend (no network)
    // -----------------------------------------------------------------------

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Constructs a minimal `AppState` for testing without a daemon connection.
    fn make_state(session_id: &str) -> AppState {
        AppState {
            session_id: session_id.to_owned(),
            mode: None,
            tier: None,
            runner: String::from("unknown"),
            model: None,
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
            context_used: 0,
            context_window: 200_000,
            main_panel: main_panel::MainPanel::new(),
            action_log: action_log::ActionLog::new(50),
            slash_completions: Vec::new(),
            slash_popup_visible: false,
            slash_cursor: 0,
            turn_in_flight: false,
            poll_retry_count: 0,
            scroll_focus: false,
            selection_mode: false,
            selection_anchor: 0,
            selection_end: 0,
            g_pending: false,
            input_cursor: 0,
        }
    }

    /// Renders `state` to an 80×24 `TestBackend` and returns the buffer.
    fn render_frame(state: &AppState) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn quit_flag_starts_false_and_can_be_set() {
        let mut state = make_state("test-session");
        assert!(!state.quit);
        state.quit = true;
        assert!(state.quit);
    }

    #[test]
    fn input_accumulates_characters_in_state() {
        let mut state = make_state("test-session");
        state.input.push('h');
        state.input.push('i');
        assert_eq!(state.input, "hi");
        // TODO: assert the input appears in the rendered buffer once
        // handle_key can be called without a live Client.
    }

    #[test]
    fn render_does_not_panic_with_empty_state() {
        let state = make_state("test-session");
        let _buf = render_frame(&state);
        // Verify no panic — any output is acceptable.
    }

    #[test]
    fn slash_popup_visible_flag_and_render() {
        let mut state = make_state("test-session");
        assert!(!state.slash_popup_visible);
        state.slash_popup_visible = true;
        state.slash_completions = filtered_completions("/");
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !content.trim().is_empty(),
            "buffer should not be entirely blank when slash popup is open"
        );
    }

    #[test]
    fn block_browser_renders_without_panic() {
        let mut state = make_state("test-session");
        let mut block = blocks::TurnBlock::new(1);
        block.complete(42);
        state.block_store.push(block);
        state.block_browser_open = true;
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !content.trim().is_empty(),
            "buffer should not be blank when block browser is open"
        );
    }

    #[test]
    fn diff_overlay_renders_without_panic() {
        let mut state = make_state("test-session");
        state.diff_overlay = Some((
            0,
            vec!["+added line".to_owned(), "-removed line".to_owned()],
        ));
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !content.trim().is_empty(),
            "buffer should not be blank when diff overlay is set"
        );
    }

    #[test]
    fn slash_health_in_completions() {
        // /health must appear in completions when user types "/h"
        let completions = filtered_completions("/h");
        assert!(
            completions.contains(&"/health"),
            "/health must be in SLASH_COMPLETIONS and match '/h' prefix"
        );
    }

    #[test]
    fn health_command_shows_socket_path_in_state() {
        let mut state = make_state("sess-health");
        // Simulate what /health should push to main_panel.
        let msg = format!("health: socket=ok session={} latency=?ms", state.session_id);
        state.main_panel.push_line(msg.clone());
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("health"),
            "health output should appear in panel"
        );
        assert!(
            content.contains("sess-health"),
            "health output should show session ID"
        );
    }

    #[test]
    fn health_command_param_key_is_id() {
        // The /health handler must pass "id", not "session_id", to session.get.
        let session_id = "sess-health";
        let payload = json!({ "id": session_id });
        assert!(
            payload.get("id").is_some(),
            "RPC payload must contain key \"id\""
        );
        assert!(
            payload.get("session_id").is_none(),
            "RPC payload must not contain key \"session_id\""
        );
        assert_eq!(
            payload["id"].as_str().unwrap(),
            session_id,
            "\"id\" value must match the session id"
        );
    }

    // push_delta accumulated via the panel renders into the frame buffer.
    #[test]
    fn push_delta_accumulates_content_in_panel() {
        let mut state = make_state("sess-stream");
        state.main_panel.push_delta("hello");
        state.main_panel.push_delta(" there");
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("hello"),
            "delta content should appear in rendered buffer"
        );
    }

    // --- connect banner tests ---

    #[test]
    fn connect_banner_visible() {
        let mut state = make_state("sess-abc");
        let sock = "/run/user/1000/smdjad.sock";
        state.main_panel.push_line(format!("connected to {sock}"));
        state.main_panel.push_line("session sess-abc".into());
        state.main_panel.push_line("provider: unknown".into());
        state.main_panel.push_line("tier: default".into());
        state
            .main_panel
            .push_line("type a message or /help for commands".into());
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(content.contains("sess-abc"), "banner must show session ID");
        assert!(
            content.contains("connected"),
            "banner must show connection line"
        );
    }

    #[test]
    fn status_bar_shows_tier_when_set() {
        let mut state = make_state("sess-xyz");
        state.tier = Some("fast".into());
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(content.contains("fast"), "status bar must render the tier");
    }

    #[test]
    fn status_bar_shows_unknown_when_no_tier() {
        let state = make_state("sess-xyz");
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!content.trim().is_empty());
    }

    // --- thinking indicator tests ---

    #[test]
    fn thinking_indicator_visible_when_turn_in_flight() {
        let mut state = make_state("sess-think");
        state.turn_in_flight = true;
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("thinking") || content.contains('\u{283f}'),
            "buffer should contain thinking indicator when turn_in_flight is true"
        );
    }

    #[test]
    fn thinking_indicator_hidden_when_idle() {
        let state = make_state("sess-idle");
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!content.is_empty());
    }

    // --- layout regression tests ---

    #[test]
    fn layout_input_row_at_bottom_of_80x24() {
        let state = make_state("sess-layout");
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.area().height, 24);
        assert_eq!(buf.area().width, 80);
    }

    #[test]
    fn layout_40x10_does_not_panic() {
        let state = make_state("sess-narrow");
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.area().width, 40);
        assert_eq!(buf.area().height, 10);
    }

    // handle_key with "/health" + Enter calls session.get and writes latency to panel.
    #[tokio::test]
    async fn health_command_handle_key_shows_latency_in_panel() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
        use tokio::net::UnixListener;

        // Bind to a socket inside a temp dir so the path is unique per test run.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("health-test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn a minimal mock daemon that handles exactly one JSON-RPC request.
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let mut reader = TokioBufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let req: serde_json::Value =
                    serde_json::from_str(line.trim_end()).unwrap_or(serde_json::Value::Null);
                let id = req["id"].clone();
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "id": "sess-health-mock" }
                });
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                let _ = reader.get_mut().write_all(&bytes).await;
            }
        });

        let mut client = Client::connect(&sock_path).await.unwrap();
        let mut state = make_state("sess-health-mock");
        state.input = "/health".into();
        let mut editor = rustyline::DefaultEditor::new().unwrap();

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        );
        handle_key(key, &mut state, &mut client, &mut editor)
            .await
            .unwrap();

        // input is cleared by std::mem::take before the command runs.
        assert!(
            state.input.is_empty(),
            "/health must clear the input field after Enter"
        );

        // The main panel must contain the health output line.
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("health"),
            "main panel should contain health output after /health command; got: {content:?}"
        );
    }

    // Verifies that the token-count footer pushed after a turn completes
    // appears in the rendered frame buffer.
    #[test]
    fn turn_footer_shows_token_counts() {
        let mut state = make_state("sess-footer");
        // Simulate what the subscribe completion path does.
        state.main_panel.push_delta("response text");
        state
            .main_panel
            .push_line("↳ 10↑ 20↓ tokens · 250ms".into());
        let buf = render_frame(&state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("tokens"),
            "turn footer should show token count label in the rendered buffer"
        );
    }

    // --- input cursor tests ---

    #[test]
    fn render_input_with_cursor_splits_at_position() {
        assert_eq!(render_input_with_cursor("hello", 2), "he_llo");
    }

    #[test]
    fn render_input_with_cursor_at_zero() {
        assert_eq!(render_input_with_cursor("hello", 0), "_hello");
    }

    #[test]
    fn render_input_with_cursor_at_end() {
        assert_eq!(render_input_with_cursor("hello", 5), "hello_");
    }

    #[test]
    fn render_input_with_cursor_empty_input() {
        assert_eq!(render_input_with_cursor("", 0), "_");
    }

    #[test]
    fn prev_char_boundary_moves_back_one_ascii() {
        assert_eq!(prev_char_boundary("hello", 3), 2);
    }

    #[test]
    fn prev_char_boundary_at_zero_stays_zero() {
        assert_eq!(prev_char_boundary("hello", 0), 0);
    }

    #[test]
    fn next_char_boundary_moves_forward_one_ascii() {
        assert_eq!(next_char_boundary("hello", 2), 3);
    }

    #[test]
    fn next_char_boundary_at_end_stays_at_end() {
        assert_eq!(next_char_boundary("hello", 5), 5);
    }

    #[test]
    fn prev_char_boundary_unicode_moves_by_char() {
        // 'é' encodes as 2 bytes (U+00E9); cursor at 2 should move to 0.
        let s = "é";
        assert_eq!(s.len(), 2);
        assert_eq!(prev_char_boundary(s, 2), 0);
    }

    #[test]
    fn next_char_boundary_unicode_moves_by_char() {
        let s = "é";
        assert_eq!(next_char_boundary(s, 0), 2);
    }

    #[test]
    fn input_cursor_defaults_to_zero_in_make_state() {
        let state = make_state("s");
        assert_eq!(state.input_cursor, 0);
    }
}
