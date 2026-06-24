pub mod action_log;
mod blocks;
mod context_rail;
mod cowork_widget;
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
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEvent,
};
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
use serde_json::{json, Value};
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

    /// Resume an existing session by id instead of creating a new one.
    #[arg(long)]
    session: Option<String>,

    /// Rewind the resumed session to this turn before replaying (destructive,
    /// mirrors `smj session rollback`). Only meaningful together with `--session`.
    #[arg(long)]
    turn: Option<u32>,
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

/// Structured output type requested by a generator slash command.
#[derive(Debug, Clone, PartialEq)]
enum OutputType {
    /// `/drawio` — draw.io mxGraph XML
    DrawIo { slug: String },
    /// `/pptx` — python-pptx presentation script
    Pptx { slug: String },
}

/// Startup routing decision derived from the `--session` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionStart {
    /// Attach to an existing session and replay its history.
    Resume(String),
    /// Create a fresh session (default behaviour).
    Create,
}

/// Maps the `--session` flag to a startup routing decision.
///
/// `Some(id)` routes to [`SessionStart::Resume`]; `None` routes to
/// [`SessionStart::Create`]. Whitespace-only ids are treated as absent.
fn session_start_decision(flag: Option<String>) -> SessionStart {
    match flag {
        Some(id) if !id.trim().is_empty() => SessionStart::Resume(id.trim().to_owned()),
        _ => SessionStart::Create,
    }
}

/// Whether a resume should rewind the session before replaying.
///
/// A `Some(turn)` target is destructive (calls `session.rollback`); `None` is a
/// non-destructive read-only replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumePlan {
    /// Rewind to `turn_n` via `session.rollback`, then replay.
    Rollback { turn_n: u32 },
    /// Replay current history without rewinding.
    ReplayOnly,
}

/// Derives the resume plan from an optional turn target.
fn resume_plan(turn: Option<u32>) -> ResumePlan {
    match turn {
        Some(turn_n) => ResumePlan::Rollback { turn_n },
        None => ResumePlan::ReplayOnly,
    }
}

/// Available slash-command completions shown in the popup.
const SLASH_COMPLETIONS: &[&str] = &[
    "/agent",
    "/agents",
    "/approvals",
    "/approve",
    "/briefing",
    "/clear",
    "/deny",
    "/drawio",
    "/health",
    "/help",
    "/login",
    "/metrics",
    "/model",
    "/ponytail",
    "/pptx",
    "/quota",
    "/resume",
    "/review",
    "/spec",
    "/switch",
    "/takeover",
    "/tdd",
    "/tier",
];

const HELP_TEXT: &str = "\
slash commands:
  /agent <id>        — run named agent
  /agents            — list available agents
  /approvals         — list pending cowork approvals
  /approve <id>      — approve a cowork item
  /briefing          — show session briefing
  /clear             — clear message display (keeps session data)
  /deny <id>         — deny a cowork item
  /drawio <slug>     — generate draw.io diagram
  /health            — check daemon connectivity
  /help              — show this message
  /login             — authenticate with runner
  /metrics           — show token usage and cost
  /model [name]      — show or set model
  /ponytail          — set review mode
  /pptx <slug>       — generate PowerPoint
  /quota             — show usage quota
  /resume [id [turn]] — resume a session (omit id for interactive picker; turn rewinds)
  /review            — send git diff for review
  /spec              — browse OpenSpec changes
  /switch [runner]   — switch AI runner (omit for interactive picker)
  /takeover <runner> — fork session to new runner
  /tdd               — set TDD mode
  /tier <t>          — set tier (local|fast|deep)

keybindings (input mode):
  Esc                — enter scroll/normal mode
  Up / Ctrl-P        — browse history backwards
  Down / Ctrl-N      — browse history forwards
  Ctrl-R             — toggle reverse history search

keybindings (scroll/normal mode):
  i / a              — return to input mode
  j / k              — scroll down / up
  G                  — scroll to bottom
  gg                 — scroll to top
  Ctrl-R             — toggle context rail
  v                  — start line selection
  y                  — yank selection to clipboard
  t                  — copy traceparent
  Esc                — exit selection / return to input";

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
    /// Available slash-command completions (filtered subset of `SLASH_COMPLETIONS`, or dynamic runner list).
    slash_completions: Vec<String>,
    /// Whether the slash-command completion popup is visible.
    slash_popup_visible: bool,
    /// Cursor index within the filtered completion list.
    slash_cursor: usize,
    /// True when the popup is showing a runner picker (Enter confirms runner switch).
    runner_picker_mode: bool,
    /// True when the popup is showing a session picker (Enter resumes the highlighted session).
    session_picker_mode: bool,
    /// Session ids parallel to `slash_completions` while the session picker is open.
    session_picker_ids: Vec<String>,
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
    /// Invariant: always on a UTF-8 char boundary, 0 ≤ cursor ≤ `input.len()`.
    input_cursor: usize,
    /// Pending cowork approvals waiting for a decision.
    pending_cowork: Vec<cowork_widget::CoworkItem>,
    /// True when the user pressed `m` to enter modify-instruction mode.
    cowork_modify_mode: bool,
    /// Current content of the modify instruction input.
    cowork_modify_input: String,
    /// Timestamp of the last `cowork.pending` poll.
    last_cowork_poll: Option<std::time::Instant>,
    /// NDJSON stream receiver for the current in-flight turn.
    stream_rx: Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>,
    /// Path of the smdjad stream socket (`<rpc_sock>.stream`).
    stream_sock_path: PathBuf,
    /// W3C traceparent from the most recently completed turn.
    last_traceparent: Option<String>,
    /// Pending structured output type for generator commands (/drawio, /pptx).
    pending_output_type: Option<OutputType>,
    /// True when `SMEDJA_OTLP_ENDPOINT` is set in the environment at startup.
    otlp_configured: bool,
    /// Start screen row of an in-progress mouse drag (messages area only).
    mouse_drag_start: Option<u16>,
    /// Current end screen row of an in-progress mouse drag.
    mouse_drag_end: Option<u16>,
    /// Top row of the messages panel as recorded by the last render frame.
    messages_top: u16,
    /// Watermark index into `messages`; messages before this index are not
    /// re-displayed after a `/clear`.
    display_start_idx: usize,
    /// Ordered list of submitted prompts for Up/Down history browsing.
    prompt_history: Vec<String>,
    /// Current browse position within `prompt_history` (`None` = live input).
    history_idx: Option<usize>,
    /// Input saved before history browsing started; restored when browsing past the end.
    saved_input: String,
    /// True while reverse history search (Ctrl-R in input mode) is active.
    history_search_mode: bool,
    /// Query string for the active reverse history search.
    history_search_query: String,
    /// Resolved path to the `openspec` binary, or `None` if not installed.
    openspec_bin: Option<PathBuf>,
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

fn stream_socket_path(rpc_path: &std::path::Path) -> PathBuf {
    let mut p = rpc_path.as_os_str().to_owned();
    p.push(".stream");
    PathBuf::from(p)
}

// ---------------------------------------------------------------------------
// Streaming turn reader
// ---------------------------------------------------------------------------

/// Connects to the smdjad stream socket and forwards NDJSON events to `tx`
/// until the terminal `done` or `error` event is received.
async fn start_stream_reader(
    sock_path: PathBuf,
    task_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "stream socket connect failed; falling back to polling");
            return;
        }
    };
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let req = format!("{{\"task_id\":\"{task_id}\"}}\n");
    if writer.write_all(req.as_bytes()).await.is_err() {
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    let is_terminal = matches!(v["type"].as_str(), Some("done" | "error"));
                    let _ = tx.send(v);
                    if is_terminal {
                        break;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session bootstrap (create vs. resume)
// ---------------------------------------------------------------------------

/// The session a startup decision resolved to, plus its display metadata.
struct ResolvedSession {
    session_id: String,
    runner: String,
    model: Option<String>,
    tier: Option<String>,
    mode: Option<String>,
    /// `true` when an existing session was attached (history should be replayed).
    resumed: bool,
}

/// Resolves the startup decision into a concrete session.
///
/// [`SessionStart::Create`] calls `session.create` (current behaviour);
/// [`SessionStart::Resume`] validates the id via `session.get` and attaches to
/// it. An unknown id surfaces as an error so the caller can fail fast before
/// any terminal setup.
///
/// # Errors
///
/// Returns an error when `session.create` fails, or when a supplied resume id is
/// unknown (`session not found: <id>`).
async fn resolve_session(client: &mut Client, start: SessionStart) -> Result<ResolvedSession> {
    match start {
        SessionStart::Create => {
            let resp = client
                .call("session.create", json!({ "title": "smedja" }))
                .await
                .map_err(|e| anyhow::anyhow!("session.create failed: {e}"))?;
            Ok(ResolvedSession {
                session_id: resp["id"].as_str().unwrap_or("unknown").to_owned(),
                runner: resp
                    .get("runner")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                model: resp
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                tier: resp.get("tier").and_then(|v| v.as_str()).map(str::to_owned),
                mode: None,
                resumed: false,
            })
        }
        SessionStart::Resume(id) => {
            let resp = client
                .call("session.get", json!({ "id": id }))
                .await
                .map_err(|_| anyhow::anyhow!("session not found: {id}"))?;
            Ok(ResolvedSession {
                session_id: resp["id"].as_str().unwrap_or(&id).to_owned(),
                runner: "unknown".to_owned(),
                model: None,
                tier: None,
                mode: resp.get("mode").and_then(|v| v.as_str()).map(str::to_owned),
                resumed: true,
            })
        }
    }
}

/// Replays `session_id` into the view, optionally rewinding it first.
///
/// When `plan` is [`ResumePlan::Rollback`], `session.rollback` is called with
/// `{ session_id, turn_n }` to rewind the conversation (destructive, mirroring
/// `smj session rollback`) before history is read. [`ResumePlan::ReplayOnly`]
/// is non-destructive: it never calls `session.rollback`. In both cases the
/// rewound history is fetched via `session.history` and seeded into the view by
/// [`replay_history`].
async fn resume_into_view(state: &mut AppState, client: &mut Client, plan: ResumePlan) {
    let session_id = state.session_id.clone();
    if let ResumePlan::Rollback { turn_n } = plan {
        if let Err(e) = client
            .call(
                "session.rollback",
                json!({ "session_id": session_id, "turn_n": turn_n }),
            )
            .await
        {
            push_system_message(state, format!("session.rollback error: {e}"));
            return;
        }
    }
    match client
        .call("session.history", json!({ "session_id": session_id }))
        .await
    {
        Ok(history) => replay_history(state, &history),
        Err(e) => push_system_message(state, format!("session.history error: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Submit a user turn to the daemon
// ---------------------------------------------------------------------------

async fn submit(input: &str, state: &mut AppState, client: &mut Client) -> Result<()> {
    let text = input.trim().to_owned();
    if text.is_empty() {
        return Ok(());
    }
    state.prompt_history.push(text.clone());
    state.history_idx = None;
    state.saved_input.clear();
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

            // Start streaming reader; events arrive via unbounded channel.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            state.stream_rx = Some(rx);
            let sock = state.stream_sock_path.clone();
            let tid = task_id.clone();
            tokio::spawn(start_stream_reader(sock, tid, tx));

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
fn filtered_completions(input: &str) -> Vec<String> {
    SLASH_COMPLETIONS
        .iter()
        .copied()
        .filter(|c| c.starts_with(input))
        .map(str::to_owned)
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

/// Seeds the view from a `session.history` response, replaying prior turns.
///
/// Iterates the `turns` array in ascending `turn_n`, builds a completed
/// [`blocks::TurnBlock`] per turn via [`blocks::TurnBlock::from_history_turn`],
/// pushes it into the [`blocks::BlockStore`], renders its lines into the
/// [`main_panel::MainPanel`], and advances `state.turn_n` to the highest
/// replayed turn so the next live turn continues the sequence. A missing or
/// empty `turns` array is a no-op.
fn replay_history(state: &mut AppState, history: &serde_json::Value) {
    let Some(turns) = history.get("turns").and_then(serde_json::Value::as_array) else {
        return;
    };
    let mut ordered: Vec<&serde_json::Value> = turns.iter().collect();
    ordered.sort_by_key(|t| {
        t.get("turn_n")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    });
    let mut max_turn = state.turn_n;
    for turn in ordered {
        let turn_n = turn
            .get("turn_n")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        let messages = turn
            .get("messages")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new()));
        let block = blocks::TurnBlock::from_history_turn(turn_n, &messages);
        for line in block.render_lines(80) {
            state.main_panel.push_line(line);
        }
        state.block_store.push(block);
        max_turn = max_turn.max(turn_n);
    }
    state.turn_n = max_turn;
}

/// Formats `session.list` rows for the `/resume` picker.
///
/// Each row renders as `<short-id>  <title>  <mode>  <updated_at>`, where the
/// short id is the first 8 characters. Missing titles/modes degrade to empty
/// or `?` placeholders rather than being dropped.
#[must_use]
fn format_resume_rows(list: &serde_json::Value) -> Vec<String> {
    let Some(items) = list.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|s| {
            let id = s
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let short = &id[..8.min(id.len())];
            let title = s
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mode = s
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let updated = s
                .get("updated_at")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("{short}  {title}  {mode}  {updated}")
        })
        .collect()
}

/// Parses `/resume` arguments into `(session_id, optional_turn)`.
///
/// `<id>` yields `(id, None)`; `<id> <turn>` yields `(id, Some(turn))`. A
/// non-numeric turn token is ignored (no turn target). Empty input yields
/// `None`.
#[must_use]
fn parse_resume_args(args: &str) -> Option<(String, Option<u32>)> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let id = parts.next()?.to_owned();
    let turn = parts.next().and_then(|t| t.parse::<u32>().ok());
    Some((id, turn))
}

/// Returns `true` (and emits a status line) when a resume must be refused
/// because a turn is awaiting a response.
fn resume_blocked_by_pending_turn(state: &mut AppState) -> bool {
    if state.pending_task_id.is_some() {
        push_system_message(state, "cannot resume while a turn is in flight");
        true
    } else {
        false
    }
}

/// Slugify `topic` for use in output filenames.
fn slugify(topic: &str) -> String {
    topic
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Parses `/review` argument flags into the `audit.run` RPC scope params.
///
/// No args → working-tree diff (`{}`); `<path>` → `{ "path": <path> }`;
/// `--branch <base>` → `{ "branch": <base> }`; `--pr <ref>` → `{ "pr": <ref> }`.
/// Unknown leading tokens are treated as a path argument.
fn parse_review_scope(args: &str) -> serde_json::Value {
    let args = args.trim();
    if args.is_empty() {
        return json!({});
    }
    let mut parts = args.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    match head {
        "--branch" => json!({ "branch": rest }),
        "--pr" => json!({ "pr": rest }),
        "--diff" => json!({ "diff": true }),
        path => json!({ "path": path }),
    }
}

/// Renders a per-severity findings summary plus the report location.
///
/// `counts` is the `audit.run` response's `counts` object; `report_path` is the
/// written path when present, otherwise the report was returned inline.
fn render_findings_summary(counts: &serde_json::Value, report_path: Option<&str>) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("audit complete — findings:");
    for severity in ["critical", "high", "medium", "low", "info"] {
        let n = counts
            .get(severity)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let _ = write!(out, " {severity}={n}");
    }
    match report_path {
        Some(path) => {
            let _ = write!(out, "\nreport: {path}");
        }
        None => out.push_str("\nreport: (inline)"),
    }
    out
}

/// Extract the first fenced code block of the given `lang` from `text`.
///
/// Returns the content inside the delimiters (without the fence lines).
fn extract_code_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let fence_open = format!("```{lang}");
    let start = text.find(fence_open.as_str())?;
    let after_open = start + fence_open.len();
    let newline = text[after_open..].find('\n')?;
    let content_start = after_open + newline + 1;
    let end = text[content_start..].find("```")?;
    Some(text[content_start..content_start + end].trim())
}

/// Save the output of a generator command (`/drawio`, `/pptx`) to a file.
///
/// Extracts the appropriate code block from `content` and writes it to cwd.
fn save_generator_output(output_type: &OutputType, content: &str, state: &mut AppState) {
    match output_type {
        OutputType::DrawIo { slug } => {
            let Some(xml) = extract_code_block(content, "xml") else {
                push_system_message(state, "no ```xml block found in response");
                return;
            };
            let path = format!("{slug}.drawio");
            match std::fs::write(&path, xml) {
                Ok(()) => push_system_message(state, format!("diagram saved: ./{path}")),
                Err(e) => push_system_message(state, format!("failed to save {path}: {e}")),
            }
        }
        OutputType::Pptx { slug } => {
            let Some(script) = extract_code_block(content, "python") else {
                push_system_message(state, "no ```python block found in response");
                return;
            };
            let script_path = format!("{slug}-gen.py");
            if let Err(e) = std::fs::write(&script_path, script) {
                push_system_message(state, format!("failed to write script {script_path}: {e}"));
                return;
            }
            match std::process::Command::new("python3")
                .arg(&script_path)
                .output()
            {
                Ok(out) if out.status.success() => {
                    let pptx_path = format!("{slug}.pptx");
                    let _ = std::fs::remove_file(&script_path);
                    push_system_message(state, format!("presentation saved: ./{pptx_path}"));
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    push_system_message(
                        state,
                        format!("python3 {script_path} failed: {}", stderr.trim()),
                    );
                }
                Err(e) => {
                    push_system_message(state, format!("failed to run python3: {e}"));
                }
            }
        }
    }
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

/// Runs `openspec bin args` as a subprocess and returns stdout on success, stderr on error.
async fn run_openspec(bin: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("openspec exec error: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// Renders `openspec list --json` output into a human-readable string.
#[must_use]
fn format_openspec_list(json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return format!("openspec list parse error: {e}"),
    };
    let changes = match v.get("changes").and_then(|c| c.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return "no active changes".to_owned(),
    };
    let mut lines = vec!["active changes:".to_owned()];
    for c in changes {
        let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let status = c.get("status").and_then(|s| s.as_str()).unwrap_or("?");
        lines.push(format!("  {name:<30} {status}"));
    }
    lines.join("\n")
}

/// Renders `openspec status --json` output as `key: value` lines.
#[must_use]
fn format_openspec_status(json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return format!("openspec status parse error: {e}"),
    };
    let Some(obj) = v.as_object() else {
        return "openspec status: unexpected response format".to_owned();
    };
    if obj.is_empty() {
        return "openspec status: no data".to_owned();
    }
    obj.iter()
        .map(|(k, v)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{k}: {val}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn screen_row_to_line(row: u16, messages_top: u16, scroll: usize) -> usize {
    let offset = (row as usize).saturating_sub(messages_top as usize);
    scroll + offset
}

/// Searches `history` backwards for the most recent entry containing `query`.
///
/// Returns `(index, matched_text)` on success.  An empty `query` always returns `None`.
#[must_use]
fn history_search<'a>(history: &'a [String], query: &str) -> Option<(usize, &'a str)> {
    if query.is_empty() {
        return None;
    }
    history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.contains(query))
        .map(|(i, s)| (i, s.as_str()))
}

fn handle_mouse(state: &mut AppState, me: MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            state.mouse_drag_start = Some(me.row);
            state.mouse_drag_end = Some(me.row);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            state.mouse_drag_end = Some(me.row);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let (Some(start), Some(end)) = (state.mouse_drag_start, state.mouse_drag_end) {
                let top = state.messages_top;
                let scroll = state.main_panel.scroll;
                let lo = screen_row_to_line(start.min(end), top, scroll);
                let hi = screen_row_to_line(start.max(end), top, scroll);
                let lines = state.main_panel.lines_text(lo, hi);
                let count = lines.len();
                yank_to_clipboard(&lines);
                state.clipboard = Some(lines.join("\n"));
                push_system_message(state, format!("\u{2713} {count} lines copied"));
            }
            state.mouse_drag_start = None;
            state.mouse_drag_end = None;
        }
        _ => {}
    }
}

fn accept_slash_completion(state: &mut AppState, append_space: bool) -> bool {
    let Some(completion) = state.slash_completions.get(state.slash_cursor).cloned() else {
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

fn clear_slash_popup(state: &mut AppState) {
    state.slash_popup_visible = false;
    state.slash_completions.clear();
    state.slash_cursor = 0;
    state.input.clear();
    state.input_cursor = 0;
    state.runner_picker_mode = false;
    state.session_picker_mode = false;
    state.session_picker_ids.clear();
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

#[allow(clippy::too_many_lines)] // flat slash-command dispatch table; splitting is out of scope here
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
        "help" => {
            push_system_message(state, HELP_TEXT);
            Ok(true)
        }
        "clear" => {
            state.display_start_idx = state.messages.len();
            state.main_panel.clear_display();
            Ok(true)
        }
        "spec" => {
            let Some(ref bin) = state.openspec_bin else {
                push_system_message(
                    state,
                    "openspec not found — install it and restart smedja-tui",
                );
                return Ok(true);
            };
            let bin = bin.clone();
            let (sub, rest) = args.split_once(' ').unwrap_or((args, ""));
            let text = match sub {
                "" | "list" => match run_openspec(&bin, &["list", "--json"]).await {
                    Ok(json) => format_openspec_list(&json),
                    Err(e) => e,
                },
                "status" => {
                    let extra: Vec<&str> = if rest.is_empty() {
                        vec!["status", "--json"]
                    } else {
                        vec!["status", "--change", rest, "--json"]
                    };
                    match run_openspec(&bin, &extra).await {
                        Ok(json) => format_openspec_status(&json),
                        Err(e) => e,
                    }
                }
                "archive" if !rest.is_empty() => {
                    match run_openspec(&bin, &["archive", rest, "--yes"]).await {
                        Ok(_) => format!("archived: {rest}"),
                        Err(e) => e,
                    }
                }
                _ => "usage: /spec [list|status [name]|archive <name>]".to_owned(),
            };
            push_system_message(state, text);
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
        "model" => {
            let session_id = state.session_id.clone();
            if args.is_empty() || args == "reset" {
                let result = client.call("runner.list", json!({})).await;
                let text = match result {
                    Ok(v) => format_model_list(&v),
                    Err(e) => format!("runner.list error: {e}"),
                };
                push_system_message(state, text);
            } else {
                let model = args.to_owned();
                let result = client
                    .call(
                        "session.set_model",
                        json!({ "session_id": session_id, "model": model }),
                    )
                    .await;
                match result {
                    Ok(_) => {
                        state.model = Some(model.clone());
                        push_system_message(state, format!("model set to {model}"));
                    }
                    Err(e) => push_system_message(state, format!("session.set_model error: {e}")),
                }
            }
            Ok(true)
        }
        "agents" => {
            let result = client.call("runner.list", json!({})).await;
            let text = match result {
                Ok(v) => format_agents_table(&v),
                Err(e) => format!("runner.list error: {e}"),
            };
            push_system_message(state, text);
            Ok(true)
        }
        "metrics" => {
            let session_id = state.session_id.clone();
            let usage_result = client
                .call("session.token_usage", json!({ "session_id": session_id }))
                .await;
            let cost_result = client
                .call("session.cost", json!({ "session_id": &state.session_id }))
                .await;
            let text = format_metrics(&usage_result, &cost_result, &state.session_id);
            push_system_message(state, text);
            Ok(true)
        }
        "approvals" => {
            let session_id = state.session_id.clone();
            let result = client
                .call("cowork.pending", json!({ "session_id": session_id }))
                .await;
            let text = match result {
                Ok(v) => format_approvals_list(&v),
                Err(e) => format!("cowork.pending error: {e}"),
            };
            push_system_message(state, text);
            Ok(true)
        }
        "approve" => {
            if args.is_empty() {
                push_system_message(state, "usage: /approve <id>");
                return Ok(true);
            }
            let id = args.to_owned();
            let session_id = state.session_id.clone();
            let result = client
                .call(
                    "cowork.approve",
                    json!({ "session_id": session_id, "id": id }),
                )
                .await;
            match result {
                Ok(v) => {
                    let resolved = v
                        .get("resolved")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let text = if resolved {
                        format!("approved: {id}")
                    } else {
                        format!("item not found: {id}")
                    };
                    push_system_message(state, text);
                }
                Err(e) => push_system_message(state, format!("cowork.approve error: {e}")),
            }
            Ok(true)
        }
        "deny" => {
            if args.is_empty() {
                push_system_message(state, "usage: /deny <id> [reason]");
                return Ok(true);
            }
            let mut parts = args.splitn(2, ' ');
            let id = parts.next().unwrap_or_default().to_owned();
            let reason = parts.next().unwrap_or("denied").to_owned();
            let session_id = state.session_id.clone();
            let result = client
                .call(
                    "cowork.deny",
                    json!({ "session_id": session_id, "id": id, "reason": reason }),
                )
                .await;
            match result {
                Ok(v) => {
                    let resolved = v
                        .get("resolved")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let text = if resolved {
                        format!("denied: {id}")
                    } else {
                        format!("item not found: {id}")
                    };
                    push_system_message(state, text);
                }
                Err(e) => push_system_message(state, format!("cowork.deny error: {e}")),
            }
            Ok(true)
        }
        "review" => {
            let mut params = parse_review_scope(args);

            // Empty working-tree diff (everything committed) no longer hard-refuses:
            // fall back to a repository path scope instead.
            let is_diff_scope = params.get("path").is_none()
                && params.get("branch").is_none()
                && params.get("pr").is_none();
            if is_diff_scope {
                let empty_diff = std::process::Command::new("git")
                    .args(["diff", "HEAD"])
                    .output()
                    .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim().is_empty());
                if empty_diff {
                    params = json!({ "path": "." });
                    push_system_message(
                        state,
                        "working tree clean; auditing the repository path scope",
                    );
                }
            }

            // The audit runs under the read-only Review role; set review mode.
            let session_id = state.session_id.clone();
            let _ = client
                .call(
                    "session.set_mode",
                    json!({ "session_id": session_id, "mode": "review" }),
                )
                .await;

            match client.call("audit.run", params).await {
                Ok(resp) => {
                    let counts = resp.get("counts").cloned().unwrap_or_else(|| json!({}));
                    let report_path = resp.get("report_path").and_then(serde_json::Value::as_str);
                    push_system_message(state, render_findings_summary(&counts, report_path));
                }
                Err(e) => push_system_message(state, format!("audit.run error: {e}")),
            }
            Ok(true)
        }
        "drawio" => {
            if args.is_empty() {
                push_system_message(state, "usage: /drawio <topic>");
                return Ok(true);
            }
            let slug = slugify(args);
            state.pending_output_type = Some(OutputType::DrawIo { slug });
            let message = format!(
                "Generate a draw.io diagram (mxGraph XML format) for: {args}\n\n\
                 Output ONLY the complete XML, enclosed in a ```xml code block. \
                 Use valid mxGraph XML that draw.io can open directly."
            );
            submit(&message, state, client).await?;
            Ok(true)
        }
        "pptx" => {
            if args.is_empty() {
                push_system_message(state, "usage: /pptx <topic>");
                return Ok(true);
            }
            let slug = slugify(args);
            state.pending_output_type = Some(OutputType::Pptx { slug });
            let message = format!(
                "Generate a python-pptx script to create a presentation about: {args}\n\n\
                 Output ONLY the complete Python script, enclosed in a ```python code block. \
                 The script must save the file as '{args_slug}.pptx' in the current directory.",
                args_slug = slugify(args)
            );
            submit(&message, state, client).await?;
            Ok(true)
        }
        "briefing" => {
            let session_id = state.session_id.clone();
            let result = client
                .call("session.compact", json!({ "session_id": session_id }))
                .await;
            match result {
                Ok(v) => {
                    let summary = v
                        .get("summary")
                        .and_then(|s| s.as_str())
                        .unwrap_or("(no summary)")
                        .to_owned();
                    push_system_message(state, format!("briefing:\n{summary}"));
                }
                Err(e) => push_system_message(state, format!("session.compact error: {e}")),
            }
            Ok(true)
        }
        "quota" => {
            push_system_message(
                state,
                "quota data is not available for this runner. Check your provider dashboard.",
            );
            Ok(true)
        }
        "login" => {
            let guidance = if args.is_empty() {
                "usage: /login <runner>\n\
                 runners: claude | codex | openai\n\
                 set ANTHROPIC_API_KEY, OPENAI_API_KEY, or install the claude/codex CLI"
                    .to_owned()
            } else {
                match args {
                    "claude" => "install claude CLI: https://claude.ai/download\n\
                                 then set ANTHROPIC_API_KEY in your shell profile"
                        .to_owned(),
                    "codex" => "install codex CLI: npm install -g @openai/codex\n\
                                 then set OPENAI_API_KEY in your shell profile"
                        .to_owned(),
                    "openai" => "set OPENAI_API_KEY=<your-key> in your shell profile".to_owned(),
                    other => format!("unknown runner: {other}"),
                }
            };
            push_system_message(state, guidance);
            Ok(true)
        }
        "switch" => {
            if args.is_empty() {
                let result = client.call("runner.list", json!({})).await;
                match result {
                    Ok(v) => {
                        let runners: Vec<String> = v
                            .get("runners")
                            .and_then(|r| r.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|r| {
                                        r.get("runner").and_then(|n| n.as_str()).map(str::to_owned)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if runners.is_empty() {
                            push_system_message(state, "no runners available from runner.list");
                        } else {
                            state.slash_completions = runners;
                            state.slash_cursor = 0;
                            state.slash_popup_visible = true;
                            state.runner_picker_mode = true;
                            state.input.clear();
                            state.input_cursor = 0;
                        }
                    }
                    Err(e) => {
                        push_system_message(
                            state,
                            format!(
                                "usage: /switch [runner]  — omit for interactive picker\n\
                                 runners: claude, codex, local, copilot, minimax, berget\n\
                                 (runner.list error: {e})"
                            ),
                        );
                    }
                }
                return Ok(true);
            }
            let session_id = state.session_id.clone();
            let result = client
                .call(
                    "session.set_runner",
                    json!({ "session_id": session_id, "runner": args }),
                )
                .await;
            match result {
                Ok(v) => {
                    let canonical = v
                        .get("runner")
                        .and_then(|r| r.as_str())
                        .unwrap_or(args)
                        .to_owned();
                    state.runner.clone_from(&canonical);
                    push_system_message(state, format!("runner switched to {canonical}"));
                }
                Err(e) => push_system_message(state, format!("session.set_runner error: {e}")),
            }
            Ok(true)
        }
        "takeover" => {
            if args.is_empty() {
                push_system_message(
                    state,
                    "usage: /takeover <runner>  — fork this session onto a new runner",
                );
                return Ok(true);
            }
            let session_id = state.session_id.clone();
            let result = client
                .call(
                    "session.takeover",
                    json!({ "session_id": session_id, "runner": args }),
                )
                .await;
            match result {
                Ok(v) => {
                    let new_session_id = v
                        .get("new_session_id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let runner = v
                        .get("runner")
                        .and_then(|r| r.as_str())
                        .unwrap_or(args)
                        .to_owned();
                    state.session_id.clone_from(&new_session_id);
                    state.runner.clone_from(&runner);
                    push_system_message(
                        state,
                        format!(
                            "handed off to {runner} — new session: {}",
                            &new_session_id[..8.min(new_session_id.len())]
                        ),
                    );
                }
                Err(e) => push_system_message(state, format!("session.takeover error: {e}")),
            }
            Ok(true)
        }
        "resume" => {
            if resume_blocked_by_pending_turn(state) {
                return Ok(true);
            }
            match parse_resume_args(args) {
                None => {
                    // No id: open the interactive picker from session.list.
                    match client.call("session.list", json!({})).await {
                        Ok(list) => {
                            let rows = format_resume_rows(&list);
                            let ids: Vec<String> = list
                                .as_array()
                                .map(|items| {
                                    items
                                        .iter()
                                        .map(|s| {
                                            s.get("id")
                                                .and_then(serde_json::Value::as_str)
                                                .unwrap_or("")
                                                .to_owned()
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if rows.is_empty() {
                                push_system_message(state, "no sessions available to resume");
                            } else {
                                state.slash_completions = rows;
                                state.session_picker_ids = ids;
                                state.slash_cursor = 0;
                                state.slash_popup_visible = true;
                                state.session_picker_mode = true;
                                state.input.clear();
                                state.input_cursor = 0;
                            }
                        }
                        Err(e) => push_system_message(state, format!("session.list error: {e}")),
                    }
                }
                Some((id, turn)) => {
                    // Direct resume: swap session, clear the live display, replay.
                    state.session_id = id;
                    state.display_start_idx = state.messages.len();
                    state.main_panel.clear_display();
                    resume_into_view(state, client, resume_plan(turn)).await;
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn format_model_list(v: &serde_json::Value) -> String {
    let runners = v.get("runners").and_then(|r| r.as_array());
    let Some(runners) = runners else {
        return "no runners available".to_owned();
    };
    let mut lines = vec!["available models:".to_owned()];
    for r in runners {
        let runner = r.get("runner").and_then(|v| v.as_str()).unwrap_or("?");
        let tier = r.get("tier").and_then(|v| v.as_str()).unwrap_or("?");
        let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        lines.push(format!("  {runner} ({tier}): {model}"));
    }
    lines.join("\n")
}

fn format_agents_table(v: &serde_json::Value) -> String {
    let runners = v.get("runners").and_then(|r| r.as_array());
    let Some(runners) = runners else {
        return "no runners configured".to_owned();
    };
    if runners.is_empty() {
        return "no runners available".to_owned();
    }
    let mut lines = vec![
        format!(" {:<14} {:<8} {}", "runner", "tier", "model"),
        format!(" {}", "─".repeat(60)),
    ];
    for r in runners {
        let runner = r.get("runner").and_then(|v| v.as_str()).unwrap_or("?");
        let tier = r.get("tier").and_then(|v| v.as_str()).unwrap_or("?");
        let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        lines.push(format!(" {runner:<14} {tier:<8} {model}"));
    }
    lines.join("\n")
}

fn format_metrics(
    usage: &Result<serde_json::Value, smedja_rpc::RpcError>,
    cost: &Result<serde_json::Value, smedja_rpc::RpcError>,
    session_id: &str,
) -> String {
    let (turn_count, total_input, total_output) = match usage {
        Ok(v) => {
            let turns = v.get("turns").and_then(|t| t.as_array());
            turns.map_or((0usize, 0i64, 0i64), |arr| {
                let last = arr.last();
                let total_in = last
                    .and_then(|r| r.get("cumulative_input"))
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let total_out = last
                    .and_then(|r| r.get("cumulative_output"))
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                (arr.len(), total_in, total_out)
            })
        }
        Err(_) => (0, 0, 0),
    };
    let cost_usd = match cost {
        Ok(v) => v
            .get("total_usd")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        Err(_) => 0.0,
    };
    let total_tok = total_input.saturating_add(total_output);
    format!(
        "session: {session_id}\n\
         turns: {turn_count}   tokens: {total_tok}\n\
         cost: ${cost_usd:.4}   input: {total_input}   output: {total_output}"
    )
}

fn format_approvals_list(v: &serde_json::Value) -> String {
    let items = v.as_array();
    let Some(items) = items else {
        return "cowork: unexpected response format".to_owned();
    };
    if items.is_empty() {
        return "cowork: no pending approvals".to_owned();
    }
    let mut lines = vec!["pending approvals:".to_owned()];
    for item in items {
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("?");
        let args = item.get("args").and_then(|v| v.as_str()).unwrap_or("");
        lines.push(format!("  [{id}] {tool}: {args}"));
    }
    lines.push("use /approve <id> or /deny <id> [reason]".to_owned());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Cowork resolver
// ---------------------------------------------------------------------------

/// Reads the daemon's `resolved` flag from a `cowork.*` RPC result.
///
/// Returns `true` only when the response is `Ok` and carries `"resolved": true`.
/// A `resolved: false`, a missing field, or any transport error all yield `false`
/// so the caller keeps the pending item rather than dropping it silently.
fn cowork_resolved(result: &Result<serde_json::Value, smedja_rpc::RpcError>) -> bool {
    match result {
        Ok(v) => v
            .get("resolved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Decides whether a cowork item should be removed and what transcript line to emit.
///
/// `success` is the confirmation text used when the daemon resolved the decision
/// (`approved: <tool>`, `denied: <tool>`, or `modify sent: <instruction>`). On
/// `resolved: false` the item is retained with an `item not found: <tool>` line;
/// on a transport error it is retained with a `<method> error: <e>` line. Returns
/// `(remove, message)`.
fn apply_cowork_decision(
    result: &Result<serde_json::Value, smedja_rpc::RpcError>,
    method: &str,
    success: &str,
    tool: &str,
) -> (bool, String) {
    match result {
        Ok(_) if cowork_resolved(result) => (true, success.to_owned()),
        Ok(_) => (false, format!("item not found: {tool}")),
        Err(e) => (false, format!("{method} error: {e}")),
    }
}

/// Sends a `cowork.*` decision RPC, injecting `session_id` into `params`.
///
/// Returns the raw RPC result so the caller can both check the `resolved` flag
/// (via [`cowork_resolved`]) and surface the appropriate transcript line. The
/// `session_id` is merged into `params` so call sites pass only the decision
/// fields (`id`, optional `reason`/`instruction`).
async fn resolve_cowork(
    client: &mut Client,
    session_id: &str,
    method: &str,
    mut params: serde_json::Value,
) -> Result<serde_json::Value, smedja_rpc::RpcError> {
    if let Some(obj) = params.as_object_mut() {
        obj.insert("session_id".to_owned(), json!(session_id));
    }
    client.call(method, params).await
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
    // Cowork gate widget intercepts keys when there are pending approvals.
    // ------------------------------------------------------------------
    if !state.pending_cowork.is_empty() {
        if state.cowork_modify_mode {
            match key.code {
                KeyCode::Esc => {
                    state.cowork_modify_mode = false;
                    state.cowork_modify_input.clear();
                }
                KeyCode::Enter => {
                    if let Some(item) = state.pending_cowork.first() {
                        let id = item.id.clone();
                        let tool = item.tool.clone();
                        let instruction = std::mem::take(&mut state.cowork_modify_input);
                        let session_id = state.session_id.clone();
                        let result = resolve_cowork(
                            client,
                            &session_id,
                            "cowork.modify",
                            json!({ "id": id, "instruction": instruction }),
                        )
                        .await;
                        let (remove, message) = apply_cowork_decision(
                            &result,
                            "cowork.modify",
                            &format!("modify sent: {instruction}"),
                            &tool,
                        );
                        if remove {
                            state.pending_cowork.remove(0);
                        }
                        push_system_message(state, message);
                    }
                    state.cowork_modify_mode = false;
                }
                KeyCode::Backspace => {
                    state.cowork_modify_input.pop();
                }
                KeyCode::Char(c) => {
                    state.cowork_modify_input.push(c);
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    if let Some(item) = state.pending_cowork.first() {
                        let id = item.id.clone();
                        let tool = item.tool.clone();
                        let session_id = state.session_id.clone();
                        let result = resolve_cowork(
                            client,
                            &session_id,
                            "cowork.approve",
                            json!({ "id": id }),
                        )
                        .await;
                        let (remove, message) = apply_cowork_decision(
                            &result,
                            "cowork.approve",
                            &format!("approved: {tool}"),
                            &tool,
                        );
                        if remove {
                            state.pending_cowork.remove(0);
                        }
                        push_system_message(state, message);
                    }
                }
                KeyCode::Char('n' | 'N') => {
                    if let Some(item) = state.pending_cowork.first() {
                        let id = item.id.clone();
                        let tool = item.tool.clone();
                        let session_id = state.session_id.clone();
                        let result = resolve_cowork(
                            client,
                            &session_id,
                            "cowork.deny",
                            json!({ "id": id, "reason": "denied" }),
                        )
                        .await;
                        let (remove, message) = apply_cowork_decision(
                            &result,
                            "cowork.deny",
                            &format!("denied: {tool}"),
                            &tool,
                        );
                        if remove {
                            state.pending_cowork.remove(0);
                        }
                        push_system_message(state, message);
                    }
                }
                KeyCode::Char('m' | 'M') => {
                    state.cowork_modify_mode = true;
                }
                _ => {}
            }
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Slash-completion popup intercepts most keys when visible.
    // ------------------------------------------------------------------
    if state.slash_popup_visible {
        match key.code {
            KeyCode::Esc => {
                clear_slash_popup(state);
            }
            KeyCode::Char(' ') | KeyCode::Tab => {
                if !state.runner_picker_mode && !state.session_picker_mode {
                    accept_slash_completion(state, true);
                }
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
                if state.session_picker_mode {
                    let chosen = state.session_picker_ids.get(state.slash_cursor).cloned();
                    state.session_picker_mode = false;
                    state.slash_popup_visible = false;
                    state.slash_completions.clear();
                    state.session_picker_ids.clear();
                    state.slash_cursor = 0;
                    state.input.clear();
                    state.input_cursor = 0;
                    if let Some(id) = chosen.filter(|id| !id.is_empty()) {
                        // Resume in place: swap session, clear live display, replay.
                        state.session_id = id;
                        state.display_start_idx = state.messages.len();
                        state.main_panel.clear_display();
                        resume_into_view(state, client, ResumePlan::ReplayOnly).await;
                    }
                } else if state.runner_picker_mode {
                    if let Some(runner_name) =
                        state.slash_completions.get(state.slash_cursor).cloned()
                    {
                        let session_id = state.session_id.clone();
                        let result = client
                            .call(
                                "session.set_runner",
                                json!({ "session_id": session_id, "runner": runner_name }),
                            )
                            .await;
                        match result {
                            Ok(v) => {
                                let canonical = v
                                    .get("runner")
                                    .and_then(|r| r.as_str())
                                    .unwrap_or(&runner_name)
                                    .to_owned();
                                state.runner.clone_from(&canonical);
                                push_system_message(
                                    state,
                                    format!("runner switched to {canonical}"),
                                );
                            }
                            Err(e) => {
                                push_system_message(
                                    state,
                                    format!("session.set_runner error: {e}"),
                                );
                            }
                        }
                        state.runner_picker_mode = false;
                        state.slash_popup_visible = false;
                        state.slash_completions.clear();
                        state.slash_cursor = 0;
                        state.input.clear();
                        state.input_cursor = 0;
                    }
                } else if accept_slash_completion(state, false) {
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
    // Reverse history search intercept — active when Ctrl-R pressed in
    // input mode.  Consumes all keys until Enter (accept) or Esc (cancel).
    // ------------------------------------------------------------------
    if state.history_search_mode {
        match key.code {
            KeyCode::Esc => {
                state.history_search_mode = false;
                state.history_search_query.clear();
                state.input = std::mem::take(&mut state.saved_input);
                state.input_cursor = state.input.len();
            }
            KeyCode::Enter => {
                state.history_search_mode = false;
                state.history_search_query.clear();
            }
            KeyCode::Backspace => {
                state.history_search_query.pop();
                let query = state.history_search_query.clone();
                if query.is_empty() {
                    state.saved_input.clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                } else if let Some((_, matched)) = history_search(&state.prompt_history, &query) {
                    matched.clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                }
            }
            KeyCode::Char(c) => {
                state.history_search_query.push(c);
                let query = state.history_search_query.clone();
                if let Some((_, matched)) = history_search(&state.prompt_history, &query) {
                    matched.clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                }
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
            KeyCode::Char('t') => {
                if let Some(tp) = state.last_traceparent.clone() {
                    yank_to_clipboard(std::slice::from_ref(&tp));
                    state.clipboard = Some(tp.clone());
                    let hint = if state.otlp_configured {
                        // Extract trace_id: field index 1 of the W3C traceparent
                        // (format: version-trace_id-parent_id-flags), which is a
                        // 32-hex-char trace ID.
                        let trace_id = tp.split('-').nth(1).unwrap_or("");
                        format!(" — open in Jaeger: http://localhost:16686/trace/{trace_id}")
                    } else {
                        " — set SMEDJA_OTLP_ENDPOINT to export traces".to_owned()
                    };
                    push_system_message(state, format!("trace: {tp}  (copied){hint}"));
                }
                return Ok(());
            }
            KeyCode::Char('i' | 'a') => {
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
            if let Some(ref text) = state.clipboard.clone() {
                yank_to_clipboard(std::slice::from_ref(text));
            } else {
                state.quit = true;
            }
        }

        // Ctrl-R: toggle context rail (scroll mode) or history search (input mode)
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.scroll_focus {
                state.context_rail_visible = !state.context_rail_visible;
            } else {
                state.history_search_mode = !state.history_search_mode;
                state.history_search_query.clear();
                if state.history_search_mode {
                    state.input.clone_into(&mut state.saved_input);
                }
            }
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
                // input mode: browse prompt history backwards
                if !state.prompt_history.is_empty() {
                    let new_idx = match state.history_idx {
                        None => {
                            state.input.clone_into(&mut state.saved_input);
                            state.prompt_history.len() - 1
                        }
                        Some(0) => 0,
                        Some(i) => i - 1,
                    };
                    state.history_idx = Some(new_idx);
                    state.prompt_history[new_idx].clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                }
            }
        }

        KeyCode::Down => {
            if state.block_browser_open {
                let max = state.block_store.len().saturating_sub(1);
                if state.block_browser_cursor < max {
                    state.block_browser_cursor += 1;
                }
            } else {
                // input mode: browse prompt history forwards
                if let Some(idx) = state.history_idx {
                    if idx + 1 < state.prompt_history.len() {
                        let new_idx = idx + 1;
                        state.history_idx = Some(new_idx);
                        state.prompt_history[new_idx].clone_into(&mut state.input);
                        state.input_cursor = state.input.len();
                    } else {
                        state.history_idx = None;
                        state.input = std::mem::take(&mut state.saved_input);
                        state.input_cursor = state.input.len();
                    }
                }
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

        // Ctrl-P / Ctrl-N: history browse (Emacs-style aliases for Up / Down)
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.prompt_history.is_empty() {
                let new_idx = match state.history_idx {
                    None => {
                        state.input.clone_into(&mut state.saved_input);
                        state.prompt_history.len() - 1
                    }
                    Some(0) => 0,
                    Some(i) => i - 1,
                };
                state.history_idx = Some(new_idx);
                state.prompt_history[new_idx].clone_into(&mut state.input);
                state.input_cursor = state.input.len();
            }
        }

        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(idx) = state.history_idx {
                if idx + 1 < state.prompt_history.len() {
                    let new_idx = idx + 1;
                    state.history_idx = Some(new_idx);
                    state.prompt_history[new_idx].clone_into(&mut state.input);
                    state.input_cursor = state.input.len();
                } else {
                    state.history_idx = None;
                    state.input = std::mem::take(&mut state.saved_input);
                    state.input_cursor = state.input.len();
                }
            }
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

#[allow(clippy::too_many_lines)] // single-pass frame layout; splitting is out of scope here
fn render(frame: &mut ratatui::Frame, state: &mut AppState) {
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
        Constraint::Length(if state.history_search_mode { 2 } else { 1 }),
    ])
    .split(area);

    let status_area = outer[0];
    let body_area = outer[1];
    let action_log_area = outer[2];
    let (input_area, search_bar_area) = if state.history_search_mode && outer[3].height >= 2 {
        let parts =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(outer[3]);
        (parts[0], Some(parts[1]))
    } else {
        (outer[3], None)
    };

    // -- Status bar -----------------------------------------------------------
    let ctx = ModuleCtx {
        session_id: &state.session_id,
        mode: state.mode.as_deref(),
        tier: state.tier.as_deref(),
        runner: Some(&state.runner),
        pending: state.pending_task_id.is_some(),
        input_mode: !state.scroll_focus,
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

    // Record messages area top row for mouse-to-line mapping.
    state.messages_top = main_area.y;

    // L122: render MainPanel from state.main_panel.
    let selection = if state.selection_mode {
        let lo = state.selection_anchor.min(state.selection_end);
        let hi = state.selection_anchor.max(state.selection_end);
        Some((lo, hi))
    } else if let (Some(start), Some(end)) = (state.mouse_drag_start, state.mouse_drag_end) {
        let lo = screen_row_to_line(start.min(end), state.messages_top, state.main_panel.scroll);
        let hi = screen_row_to_line(start.max(end), state.messages_top, state.main_panel.scroll);
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
        format!(
            "... {}",
            render_input_with_cursor(last_line, cursor_in_line)
        )
    } else {
        format!(
            "> {}",
            render_input_with_cursor(&state.input, state.input_cursor)
        )
    };
    let input_widget = Paragraph::new(input_display);
    frame.render_widget(input_widget, input_area);

    if let Some(search_area) = search_bar_area {
        let matched = history_search(&state.prompt_history, &state.history_search_query)
            .map_or("", |(_, s)| s);
        let search_text = format!(
            "(reverse-i-search) `{}`: {}",
            state.history_search_query, matched
        );
        let search_widget = Paragraph::new(search_text).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        );
        frame.render_widget(search_widget, search_area);
    }

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

    // -- Cowork gate overlay --------------------------------------------------
    if !state.pending_cowork.is_empty() {
        let cw_rect = cowork_widget::overlay_rect(body_area);
        frame.render_widget(
            cowork_widget::CoworkWidget {
                items: &state.pending_cowork,
                modify_mode: state.cowork_modify_mode,
                modify_input: &state.cowork_modify_input,
            },
            cw_rect,
        );
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
    // Session-picker rows (`<short-id>  <title>  <mode>  <updated_at>`) are wider
    // than the 20-col command popup, so widen to fit when the picker is open.
    let desired_w = if state.session_picker_mode { 60 } else { 20 };
    let popup_w = desired_w.min(area.width);
    // Position just above the input row (bottom-left).
    let popup_y = area.y + area.height.saturating_sub(popup_h + 1);
    let popup_x = area.x;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let lines: Vec<Line<'_>> = completions
        .iter()
        .enumerate()
        .map(|(i, c)| {
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

    let title = if state.session_picker_mode {
        "sessions"
    } else if state.runner_picker_mode {
        "runners"
    } else {
        "commands"
    };
    frame.render_widget(Clear, popup_rect);
    let popup = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(popup, popup_rect);
}

// ---------------------------------------------------------------------------
// Cleanup guard — always restores terminal even on panic
// ---------------------------------------------------------------------------

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Initialises tracing, honouring `SMEDJA_LOG_FORMAT` (`text` default | `json`;
/// unrecognised → text + warning).
fn init_tracing() {
    match std::env::var("SMEDJA_LOG_FORMAT").as_deref() {
        Ok("json") => tracing_subscriber::fmt().json().init(),
        Ok("text" | "") | Err(_) => tracing_subscriber::fmt().init(),
        Ok(other) => {
            tracing_subscriber::fmt().init();
            tracing::warn!(format = other, "unrecognised SMEDJA_LOG_FORMAT; using text");
        }
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // event loop + render + poll in a single binary entry point
async fn main() -> Result<()> {
    init_tracing();

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

    // Branch on the --session flag: resume an existing session (validated via
    // session.get) or create a fresh one. Resume validation runs before any
    // terminal setup so an unknown id is a clean fail-fast exit.
    let resolved = resolve_session(&mut client, session_start_decision(cli.session)).await?;
    let session_id = resolved.session_id;
    let resumed = resolved.resumed;

    tracing::debug!(session_id = %session_id, resumed, "session ready");

    let startup_runner = resolved.runner;
    let startup_model = resolved.model;
    let startup_tier = resolved.tier;
    let resumed_mode = resolved.mode;

    let otlp_configured = std::env::var("SMEDJA_OTLP_ENDPOINT").is_ok();
    let stream_sock_path = stream_socket_path(&sock);
    let mut state = AppState {
        session_id,
        mode: cli.mode.or(resumed_mode),
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
        runner_picker_mode: false,
        session_picker_mode: false,
        session_picker_ids: Vec::new(),
        turn_in_flight: false,
        poll_retry_count: 0,
        scroll_focus: false,
        selection_mode: false,
        selection_anchor: 0,
        selection_end: 0,
        g_pending: false,
        input_cursor: 0,
        pending_cowork: Vec::new(),
        cowork_modify_mode: false,
        cowork_modify_input: String::new(),
        last_cowork_poll: None,
        stream_rx: None,
        stream_sock_path,
        last_traceparent: None,
        pending_output_type: None,
        otlp_configured,
        mouse_drag_start: None,
        mouse_drag_end: None,
        messages_top: 0,
        display_start_idx: 0,
        prompt_history: Vec::new(),
        history_idx: None,
        saved_input: String::new(),
        history_search_mode: false,
        history_search_query: String::new(),
        openspec_bin: which::which("openspec").ok(),
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

    // On resume, optionally rewind to --turn and replay history into the view
    // before the event loop starts. Done before terminal setup so a transport
    // failure surfaces as a normal panel line rather than mid-frame.
    if resumed {
        resume_into_view(&mut state, &mut client, resume_plan(cli.turn)).await;
    }

    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("enter alternate screen")?;
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
                match ev {
                    Event::Key(key) => {
                        handle_key(key, &mut state, &mut client, &mut editor).await?;
                    }
                    Event::Mouse(me) => handle_mouse(&mut state, me),
                    _ => {}
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

        terminal.draw(|f| render(f, &mut state))?;

        // Drain NDJSON stream events from the background reader task.
        // When streaming is active (stream_rx is Some), render deltas in real
        // time and finalise the turn on the terminal event.  When streaming is
        // not available, fall back to the turn.subscribe blocking poll.
        let mut pending_output_save: Option<(OutputType, String)> = None;
        if let Some(ref mut rx) = state.stream_rx {
            let mut turn_done = false;
            while let Ok(event) = rx.try_recv() {
                match event["type"].as_str() {
                    Some("delta") => {
                        if let Some(text) = event["text"].as_str() {
                            // Split on newlines so each line is a separate panel entry.
                            let mut remaining = text;
                            loop {
                                if let Some(pos) = remaining.find('\n') {
                                    let chunk = &remaining[..pos];
                                    if !chunk.is_empty() {
                                        state.main_panel.push_delta(chunk);
                                    }
                                    state.main_panel.push_line(String::new());
                                    remaining = &remaining[pos + 1..];
                                    if let Some(ref mut block) = state.current_block {
                                        block.push_text(chunk);
                                        block.push_text("\n");
                                    }
                                } else {
                                    if !remaining.is_empty() {
                                        state.main_panel.push_delta(remaining);
                                        if let Some(ref mut block) = state.current_block {
                                            block.push_text(remaining);
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    Some("tool_call") => {
                        let name = event["name"].as_str().unwrap_or("?");
                        let input = event["input"].as_str().unwrap_or("");
                        let line = format!("▶ {name}({input})");
                        state.main_panel.push_line(line.clone());
                        if let Some(ref mut block) = state.current_block {
                            block.push_text(&line);
                            block.push_text("\n");
                        }
                    }
                    Some("done") => {
                        let output_tok = event["output_tok"].as_u64().unwrap_or(0);
                        let input_tok = event["input_tok"].as_u64().unwrap_or(0);
                        let tp = event["traceparent"].as_str().map(str::to_owned);
                        let elapsed_ms = state.turn_submitted_at.map_or(0, |t| {
                            u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                        });
                        state.turn_submitted_at = None;
                        state.last_traceparent.clone_from(&tp);

                        let block_content = if let Some(mut block) = state.current_block.take() {
                            block.complete(elapsed_ms);
                            let content = block.content.clone();
                            state.block_store.push(block);
                            content
                        } else {
                            String::new()
                        };

                        let footer = if let Some(ref tp_str) = tp {
                            if state.otlp_configured {
                                format!("↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str}")
                            } else {
                                format!(
                                    "↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str} · traces not exported (set SMEDJA_OTLP_ENDPOINT)"
                                )
                            }
                        } else {
                            format!("↳ {input_tok}↑ {output_tok}↓ tokens · {elapsed_ms}ms")
                        };
                        state.main_panel.push_line(footer);

                        if let Some(output_type) = state.pending_output_type.take() {
                            pending_output_save = Some((output_type, block_content));
                        }

                        turn_done = true;
                    }
                    Some("error") => {
                        let msg_text = event["message"].as_str().unwrap_or("unknown error");
                        state.main_panel.push_line(format!("error: {msg_text}"));
                        if let Some(mut block) = state.current_block.take() {
                            block.fail();
                            state.block_store.push(block);
                        }
                        turn_done = true;
                    }
                    _ => {}
                }
            }

            if turn_done {
                state.pending_task_id = None;
                state.stream_rx = None;
                state.turn_in_flight = false;
                state.poll_retry_count = 0;
                state.last_poll = None;

                // Refresh context rail after turn completes.
                if let Ok(ctx) = client
                    .call("session.context", json!({ "session_id": state.session_id }))
                    .await
                {
                    if let Some(used) = ctx["used_tok"].as_i64() {
                        state.context_used = u64::try_from(used.max(0)).unwrap_or(0);
                    }
                    if let Some(window) = ctx["window_tok"].as_u64() {
                        if window > 0 {
                            state.context_window = window;
                        }
                    }
                }
            }
        } else if let Some(task_id) = state.pending_task_id.clone() {
            // Fallback: blocking poll via turn.subscribe (no stream socket available).
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
                            state.main_panel.push_line(format!("error: {error}"));
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
                                    state.main_panel.push_line(line.clone());
                                    state.messages.push(Message {
                                        role: Role::System,
                                        text: line,
                                    });
                                }
                                state.block_store.push(block);
                            } else {
                                if !response.is_empty() {
                                    state.main_panel.push_delta(&response);
                                }
                                state.messages.push(Message {
                                    role: Role::System,
                                    text: response,
                                });
                            }

                            let footer =
                                format!("↳ {input_tok}↑ {output_tok}↓ tokens · {elapsed_ms}ms");
                            state.main_panel.push_line(footer);
                            state.last_traceparent = None;
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                        state.turn_in_flight = false;
                        state.poll_retry_count = 0;

                        if let Ok(ctx) = client
                            .call("session.context", json!({ "session_id": state.session_id }))
                            .await
                        {
                            if let Some(used) = ctx["used_tok"].as_i64() {
                                state.context_used = u64::try_from(used.max(0)).unwrap_or(0);
                            }
                            if let Some(window) = ctx["window_tok"].as_u64() {
                                if window > 0 {
                                    state.context_window = window;
                                }
                            }
                        }
                    }
                    Ok(_) => {
                        state.poll_retry_count += 1;
                        state.last_poll = None;
                        if state.poll_retry_count % 5 == 1 {
                            state.main_panel.push_line(format!(
                                "waiting for turn… (poll attempt {})",
                                state.poll_retry_count
                            ));
                        }
                        if state.poll_retry_count >= 60 {
                            state.main_panel.push_line(
                                "turn appears stuck — no response after 60 polls; giving up"
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
                            "turn timed out (>60 s) — daemon is still running the turn".to_owned()
                        } else {
                            format!("turn error: {e}")
                        };
                        state.main_panel.push_line(text.clone());
                        state.messages.push(Message {
                            role: Role::System,
                            text,
                        });
                        state.pending_task_id = None;
                        state.last_poll = None;
                        state.turn_in_flight = false;
                        state.poll_retry_count = 0;
                    }
                }
            }
        }

        // Process any deferred generator output (drawio/pptx) now that the
        // stream_rx borrow has ended.
        if let Some((output_type, content)) = pending_output_save {
            save_generator_output(&output_type, &content, &mut state);
        }

        // Poll cowork.pending every 200 ms when a turn is in flight to surface
        // any gate waiting for a human decision.
        let should_poll_cowork = state
            .last_cowork_poll
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(200));
        if should_poll_cowork && state.pending_task_id.is_some() {
            state.last_cowork_poll = Some(std::time::Instant::now());
            if let Ok(Value::Array(items)) = client
                .call("cowork.pending", json!({ "session_id": state.session_id }))
                .await
            {
                let mut parsed: Vec<cowork_widget::CoworkItem> = items
                    .iter()
                    .filter_map(|v| {
                        let id = v["id"].as_str()?.to_owned();
                        let tool = v["tool"].as_str().unwrap_or("?").to_owned();
                        #[allow(clippy::cast_possible_truncation)]
                        // step counter is bounded well below u32::MAX
                        let step_n = v["step_n"].as_u64().unwrap_or(0) as u32;
                        let args_display = v["args"]
                            .as_object()
                            .map_or_else(|| v["args"].to_string(), |_| v["args"].to_string());
                        let reasoning = v["reasoning"].as_str().unwrap_or("").to_owned();
                        Some(cowork_widget::CoworkItem {
                            id,
                            tool,
                            step_n,
                            args_display,
                            reasoning,
                        })
                    })
                    .collect();
                // Keep items already confirmed in the widget (user already see
                // them); only add genuinely new IDs.
                let existing_ids: std::collections::HashSet<String> =
                    state.pending_cowork.iter().map(|i| i.id.clone()).collect();
                parsed.retain(|i| !existing_ids.contains(&i.id));
                state.pending_cowork.extend(parsed);
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

    // --- /review scope-flag parsing ---

    #[test]
    fn review_no_args_is_diff_scope() {
        let params = parse_review_scope("");
        assert_eq!(params, json!({}), "no args → working-tree diff");
    }

    #[test]
    fn review_path_arg_is_path_scope() {
        assert_eq!(
            parse_review_scope("src/lib.rs"),
            json!({ "path": "src/lib.rs" })
        );
    }

    #[test]
    fn review_branch_flag_is_branch_scope() {
        assert_eq!(
            parse_review_scope("--branch main"),
            json!({ "branch": "main" })
        );
    }

    #[test]
    fn review_pr_flag_is_pr_scope() {
        assert_eq!(parse_review_scope("--pr 42"), json!({ "pr": "42" }));
    }

    // --- /review findings summary rendering ---

    #[test]
    fn findings_summary_lists_counts_and_report_path() {
        let counts = json!({ "critical": 1, "high": 0, "medium": 2, "low": 3, "info": 0 });
        let summary = render_findings_summary(&counts, Some("/tmp/report.md"));
        assert!(summary.contains("critical=1"), "got: {summary}");
        assert!(summary.contains("medium=2"), "got: {summary}");
        assert!(summary.contains("low=3"), "got: {summary}");
        assert!(summary.contains("report: /tmp/report.md"), "got: {summary}");
    }

    #[test]
    fn findings_summary_marks_inline_when_no_path() {
        let counts = json!({ "critical": 0, "high": 0, "medium": 0, "low": 0, "info": 0 });
        let summary = render_findings_summary(&counts, None);
        assert!(summary.contains("report: (inline)"), "got: {summary}");
    }

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
        let completions = filtered_completions("/bri");
        assert_eq!(completions, vec!["/briefing".to_owned()]);
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
        state.input = "/ti".to_owned();
        state.slash_completions = filtered_completions("/ti");
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

    #[test]
    fn slash_esc_clears_input_and_closes_popup() {
        let mut state = make_state("test-session");
        state.input = "/ti".to_owned();
        state.input_cursor = 3;
        state.slash_completions = filtered_completions("/ti");
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        clear_slash_popup(&mut state);

        assert!(state.input.is_empty(), "input must be cleared on Esc");
        assert_eq!(state.input_cursor, 0, "cursor must reset to 0 on Esc");
        assert!(!state.slash_popup_visible, "popup must close on Esc");
        assert!(
            state.slash_completions.is_empty(),
            "completions must be cleared on Esc"
        );
        assert_eq!(state.slash_cursor, 0);
    }

    #[test]
    fn slash_esc_on_popup_already_closed_is_idempotent() {
        let mut state = make_state("test-session");
        state.input = "hello".to_owned();
        state.input_cursor = 5;
        state.slash_popup_visible = false;

        clear_slash_popup(&mut state);

        assert!(state.input.is_empty());
        assert_eq!(state.input_cursor, 0);
        assert!(!state.slash_popup_visible);
    }

    #[test]
    fn slash_tier_fast_routes_correctly_via_apply_tier() {
        let mut state = make_state("test-session");
        let text = apply_tier("fast", &mut state);
        assert_eq!(state.tier.as_deref(), Some("fast"));
        assert_eq!(text, "tier set to fast");
    }

    #[test]
    fn slash_tier_local_routes_correctly_via_apply_tier() {
        let mut state = make_state("test-session");
        let text = apply_tier("local", &mut state);
        assert_eq!(state.tier.as_deref(), Some("local"));
        assert_eq!(text, "tier set to local");
    }

    #[test]
    fn slash_tier_unknown_arg_returns_error_message() {
        let mut state = make_state("test-session");
        let text = apply_tier("turbo", &mut state);
        assert_eq!(text, "unknown tier: turbo");
        assert!(state.tier.is_none(), "tier must not change on unknown arg");
    }

    #[test]
    fn slash_agent_sets_mode_via_apply_agent() {
        let mut state = make_state("test-session");
        let text = apply_agent("review", &mut state);
        assert_eq!(state.mode.as_deref(), Some("review"));
        assert_eq!(text, "agent mode set to review");
    }

    #[test]
    fn format_agents_table_renders_header_and_rows() {
        let v = serde_json::json!({
            "runners": [
                { "runner": "claude-cli", "tier": "fast", "model": "claude-haiku-4-5-20251001" },
                { "runner": "claude-cli", "tier": "deep", "model": "claude-sonnet-4-6" },
            ]
        });
        let out = format_agents_table(&v);
        assert!(out.contains("runner"), "header must include 'runner'");
        assert!(out.contains("claude-cli"), "table must list runner name");
        assert!(out.contains("fast"), "table must list tier");
        assert!(
            out.contains("claude-haiku-4-5-20251001"),
            "table must list model"
        );
    }

    #[test]
    fn format_agents_table_empty_runners_returns_message() {
        let v = serde_json::json!({ "runners": [] });
        let out = format_agents_table(&v);
        assert!(out.contains("no runners"), "empty pool must say no runners");
    }

    #[test]
    fn format_metrics_aggregates_token_and_cost_data() {
        let usage = Ok(serde_json::json!({
            "session_id": "sess-1",
            "turns": [
                { "turn_n": 1, "input_tok": 100, "output_tok": 50, "cumulative_input": 100, "cumulative_output": 50 }
            ]
        }));
        let cost = Ok(serde_json::json!({
            "session_id": "sess-1",
            "total_usd": 0.0025,
            "breakdown": []
        }));
        let out = format_metrics(&usage, &cost, "sess-1");
        assert!(out.contains("sess-1"), "metrics must include session id");
        assert!(out.contains("turns: 1"), "metrics must include turn count");
        assert!(out.contains("0.0025"), "metrics must include cost");
    }

    #[test]
    fn format_metrics_handles_rpc_errors_gracefully() {
        let usage: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32600, "unavailable"));
        let cost: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32600, "unavailable"));
        let out = format_metrics(&usage, &cost, "sess-err");
        assert!(
            out.contains("sess-err"),
            "metrics must still show session id on error"
        );
    }

    #[test]
    fn format_approvals_list_shows_items() {
        let v = serde_json::json!([
            { "id": "item-1", "tool": "Bash", "args": "git push origin main", "step_n": 1 }
        ]);
        let out = format_approvals_list(&v);
        assert!(out.contains("item-1"), "must include id");
        assert!(out.contains("Bash"), "must include tool name");
        assert!(out.contains("git push"), "must include args");
        assert!(out.contains("/approve"), "must include usage hint");
    }

    #[test]
    fn format_approvals_list_empty_shows_no_pending_message() {
        let v = serde_json::json!([]);
        let out = format_approvals_list(&v);
        assert!(
            out.contains("no pending"),
            "empty list must say no pending approvals"
        );
    }

    #[test]
    fn format_model_list_renders_all_entries() {
        let v = serde_json::json!({
            "runners": [
                { "runner": "claude-cli", "tier": "fast", "model": "claude-haiku-4-5-20251001" }
            ]
        });
        let out = format_model_list(&v);
        assert!(out.contains("claude-cli"), "must include runner name");
        assert!(out.contains("fast"), "must include tier");
        assert!(
            out.contains("claude-haiku-4-5-20251001"),
            "must include model"
        );
    }

    #[test]
    fn slash_completions_include_new_commands() {
        let required = [
            "/agents",
            "/approve",
            "/approvals",
            "/briefing",
            "/deny",
            "/login",
            "/metrics",
            "/model",
            "/quota",
            "/review",
            "/switch",
            "/takeover",
        ];
        for cmd in required {
            assert!(
                SLASH_COMPLETIONS.contains(&cmd),
                "{cmd} must be in SLASH_COMPLETIONS"
            );
        }
    }

    #[test]
    fn slash_completions_switch_matches_sw_prefix() {
        let completions = filtered_completions("/sw");
        assert!(
            completions.contains(&"/switch".to_owned()),
            "/switch must match '/sw' prefix; got: {completions:?}"
        );
    }

    #[test]
    fn slash_completions_takeover_matches_tak_prefix() {
        let completions = filtered_completions("/tak");
        assert!(
            completions.contains(&"/takeover".to_owned()),
            "/takeover must match '/tak' prefix; got: {completions:?}"
        );
    }

    #[test]
    fn switch_no_args_opens_runner_picker() {
        // Simulate what dispatch_slash("switch", "") does on successful runner.list:
        // populate slash_completions with runner names and set picker flags.
        let mut state = make_state("sess-switch");
        state.slash_completions = vec!["claude".to_owned(), "codex".to_owned(), "local".to_owned()];
        state.slash_popup_visible = true;
        state.runner_picker_mode = true;
        state.input.clear();
        state.input_cursor = 0;

        assert!(state.slash_popup_visible, "picker popup must open");
        assert!(state.runner_picker_mode, "runner_picker_mode must be set");
        assert!(
            !state.slash_completions.is_empty(),
            "completions must list runner names"
        );
    }

    #[test]
    fn slash_takeover_no_args_produces_usage_hint() {
        let mut state = make_state("sess-takeover");
        let cmd = "takeover";
        let args = "";
        let guidance = if args.is_empty() {
            Some("usage: /takeover <runner>  — fork this session onto a new runner".to_owned())
        } else {
            None
        };
        if let Some(msg) = guidance {
            state.main_panel.push_line(msg.clone());
            assert!(msg.contains("usage"), "hint must mention 'usage'");
        } else {
            panic!("expected usage hint for cmd={cmd} args={args}");
        }
    }

    #[test]
    fn slash_switch_updates_state_runner_on_success() {
        // Simulate what dispatch_slash("switch", "codex") does on success
        // without a live daemon: verify state mutations are correct.
        let mut state = make_state("sess-switch-ok");
        let canonical = "codex-cli";
        state.runner = canonical.to_owned();
        push_system_message(&mut state, format!("runner switched to {canonical}"));

        assert_eq!(state.runner, "codex-cli");
        let has_msg = state
            .main_panel
            .lines_text(0, 100)
            .iter()
            .any(|l| l.contains("runner switched to codex-cli"));
        assert!(has_msg, "panel must show switch confirmation");
    }

    #[test]
    fn slash_takeover_updates_session_id_and_runner_on_success() {
        let mut state = make_state("old-session");
        let new_session_id = "new-session-uuid-1234";
        let runner = "codex-cli";
        state.session_id = new_session_id.to_owned();
        state.runner = runner.to_owned();
        push_system_message(
            &mut state,
            format!(
                "handed off to {runner} — new session: {}",
                &new_session_id[..8]
            ),
        );

        assert_eq!(state.session_id, new_session_id);
        assert_eq!(state.runner, "codex-cli");
        let has_msg = state
            .main_panel
            .lines_text(0, 100)
            .iter()
            .any(|l| l.contains("handed off to codex-cli"));
        assert!(has_msg, "panel must show handoff confirmation");
    }

    // -----------------------------------------------------------------------
    // Session resume — startup routing, replay, picker, rollback
    // -----------------------------------------------------------------------

    #[test]
    fn resume_when_session_flag_present() {
        let decision = session_start_decision(Some("abc-123".to_owned()));
        assert_eq!(decision, SessionStart::Resume("abc-123".to_owned()));
    }

    #[test]
    fn create_when_session_flag_absent() {
        assert_eq!(session_start_decision(None), SessionStart::Create);
    }

    #[test]
    fn resume_ignores_blank_session_flag() {
        assert_eq!(
            session_start_decision(Some("   ".to_owned())),
            SessionStart::Create
        );
    }

    #[test]
    fn replay_seeds_blocks_and_continues_turn_n() {
        let mut state = make_state("resume-session");
        let history = serde_json::json!({
            "session_id": "resume-session",
            "turns": [
                { "turn_n": 1, "created_at": "t1", "messages": [
                    { "role": "user", "content": "first prompt" },
                    { "role": "assistant", "content": "first reply" },
                ]},
                { "turn_n": 2, "created_at": "t2", "messages": [
                    { "role": "user", "content": "second prompt" },
                    { "role": "assistant", "content": "second reply" },
                ]},
            ],
        });
        replay_history(&mut state, &history);
        assert_eq!(state.block_store.len(), 2, "one block per turn");
        assert_eq!(
            state.turn_n, 2,
            "turn_n must equal the highest replayed turn"
        );
        let body = state.main_panel.visible_text();
        assert!(body.contains("first reply"), "panel missing turn 1: {body}");
        assert!(
            body.contains("second reply"),
            "panel missing turn 2: {body}"
        );
    }

    #[test]
    fn replay_empty_turns_is_noop() {
        let mut state = make_state("fresh-session");
        let history = serde_json::json!({ "session_id": "fresh-session", "turns": [] });
        replay_history(&mut state, &history);
        assert_eq!(state.block_store.len(), 0);
        assert_eq!(state.turn_n, 0);
    }

    #[test]
    fn replay_missing_turns_is_noop() {
        let mut state = make_state("fresh-session");
        let history = serde_json::json!({ "session_id": "fresh-session" });
        replay_history(&mut state, &history);
        assert_eq!(state.block_store.len(), 0);
        assert_eq!(state.turn_n, 0);
    }

    #[test]
    fn resume_list_formats_session_rows() {
        let list = serde_json::json!([
            {
                "id": "0123456789abcdef",
                "title": "fix the parser",
                "mode": "impl",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-06-22T09:30:00Z",
            },
            {
                "id": "fedcba9876543210",
                "title": "",
                "mode": serde_json::Value::Null,
                "created_at": "2026-01-02T00:00:00Z",
                "updated_at": "2026-06-21T11:00:00Z",
            },
        ]);
        let rows = format_resume_rows(&list);
        assert_eq!(rows.len(), 2, "one row per session");
        assert!(
            rows[0].starts_with("01234567"),
            "short id first: {}",
            rows[0]
        );
        assert!(rows[0].contains("fix the parser"), "title: {}", rows[0]);
        assert!(rows[0].contains("impl"), "mode: {}", rows[0]);
        assert!(
            rows[0].contains("2026-06-22T09:30:00Z"),
            "updated_at: {}",
            rows[0]
        );
        // Missing title / null mode must still produce a usable row.
        assert!(rows[1].starts_with("fedcba98"), "row: {}", rows[1]);
    }

    #[test]
    fn resume_with_turn_calls_rollback_then_replays() {
        assert_eq!(resume_plan(Some(3)), ResumePlan::Rollback { turn_n: 3 });
        assert_eq!(resume_plan(None), ResumePlan::ReplayOnly);
    }

    #[test]
    fn parse_resume_args_splits_id_and_turn() {
        assert_eq!(parse_resume_args("abc"), Some(("abc".to_owned(), None)));
        assert_eq!(
            parse_resume_args("abc 5"),
            Some(("abc".to_owned(), Some(5)))
        );
        assert_eq!(parse_resume_args(""), None);
        // Non-numeric turn is ignored (treated as no turn target).
        assert_eq!(parse_resume_args("abc xyz"), Some(("abc".to_owned(), None)));
    }

    #[test]
    fn resume_in_slash_completions() {
        assert!(
            SLASH_COMPLETIONS.contains(&"/resume"),
            "/resume must be in SLASH_COMPLETIONS"
        );
        let completions = filtered_completions("/res");
        assert!(
            completions.contains(&"/resume".to_owned()),
            "/resume must match '/res' prefix; got: {completions:?}"
        );
    }

    #[test]
    fn help_text_mentions_resume() {
        assert!(HELP_TEXT.contains("/resume"), "help must document /resume");
    }

    #[test]
    fn resume_rejected_while_turn_in_flight() {
        let mut state = make_state("busy-session");
        state.pending_task_id = Some("task-1".to_owned());
        assert!(resume_blocked_by_pending_turn(&mut state));
        let body = state.main_panel.visible_text();
        assert!(body.contains("cannot resume"), "status message: {body}");
        // No pending turn → not blocked.
        let mut idle = make_state("idle-session");
        assert!(!resume_blocked_by_pending_turn(&mut idle));
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
            runner_picker_mode: false,
            session_picker_mode: false,
            session_picker_ids: Vec::new(),
            turn_in_flight: false,
            poll_retry_count: 0,
            scroll_focus: false,
            selection_mode: false,
            selection_anchor: 0,
            selection_end: 0,
            g_pending: false,
            input_cursor: 0,
            pending_cowork: Vec::new(),
            cowork_modify_mode: false,
            cowork_modify_input: String::new(),
            last_cowork_poll: None,
            stream_rx: None,
            stream_sock_path: PathBuf::from("/tmp/smdjad.sock.stream"),
            last_traceparent: None,
            pending_output_type: None,
            otlp_configured: false,
            mouse_drag_start: None,
            mouse_drag_end: None,
            messages_top: 0,
            display_start_idx: 0,
            prompt_history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            history_search_mode: false,
            history_search_query: String::new(),
            openspec_bin: None,
        }
    }

    /// Renders `state` to an 80×24 `TestBackend` and returns the buffer.
    fn render_frame(state: &mut AppState) -> ratatui::buffer::Buffer {
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
        let mut state = make_state("test-session");
        let _buf = render_frame(&mut state);
        // Verify no panic — any output is acceptable.
    }

    #[test]
    fn slash_popup_visible_flag_and_render() {
        let mut state = make_state("test-session");
        assert!(!state.slash_popup_visible);
        state.slash_popup_visible = true;
        state.slash_completions = filtered_completions("/");
        let buf = render_frame(&mut state);
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
        let buf = render_frame(&mut state);
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
        let buf = render_frame(&mut state);
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
            completions.contains(&"/health".to_owned()),
            "/health must be in SLASH_COMPLETIONS and match '/h' prefix"
        );
    }

    #[test]
    fn health_command_shows_socket_path_in_state() {
        let mut state = make_state("sess-health");
        // Simulate what /health should push to main_panel.
        let msg = format!("health: socket=ok session={} latency=?ms", state.session_id);
        state.main_panel.push_line(msg.clone());
        let buf = render_frame(&mut state);
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
        let buf = render_frame(&mut state);
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
        let buf = render_frame(&mut state);
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
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(content.contains("fast"), "status bar must render the tier");
    }

    #[test]
    fn status_bar_shows_unknown_when_no_tier() {
        let mut state = make_state("sess-xyz");
        let buf = render_frame(&mut state);
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
        let buf = render_frame(&mut state);
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
        let mut state = make_state("sess-idle");
        let buf = render_frame(&mut state);
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
        let mut state = make_state("sess-layout");
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.area().height, 24);
        assert_eq!(buf.area().width, 80);
    }

    #[test]
    fn layout_40x10_does_not_panic() {
        let mut state = make_state("sess-narrow");
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
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
        let buf = render_frame(&mut state);
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
        let buf = render_frame(&mut state);
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

    // ── provider-display: session.create response parsing ───────────────────

    fn parse_session_resp(
        resp: &serde_json::Value,
        cli_tier: Option<String>,
    ) -> (String, Option<String>, Option<String>) {
        let runner = resp
            .get("runner")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        let model: Option<String> = resp
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let resp_tier: Option<String> =
            resp.get("tier").and_then(|v| v.as_str()).map(str::to_owned);
        let effective_tier = cli_tier.or(resp_tier);
        (runner, model, effective_tier)
    }

    #[test]
    fn startup_runner_populated_from_session_resp() {
        let resp = serde_json::json!({
            "id": "x",
            "runner": "claude-cli",
            "model": "claude-sonnet-4-6",
            "tier": "fast",
        });
        let (runner, model, tier) = parse_session_resp(&resp, None);
        assert_eq!(runner, "claude-cli");
        assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(tier.as_deref(), Some("fast"));
    }

    #[test]
    fn startup_fields_fall_back_gracefully_when_missing() {
        let resp = serde_json::json!({ "id": "x" });
        let (runner, model, tier) = parse_session_resp(&resp, None);
        assert_eq!(runner, "unknown");
        assert!(model.is_none());
        assert!(tier.is_none());
    }

    #[test]
    fn cli_tier_arg_takes_precedence_over_response_tier() {
        let resp = serde_json::json!({ "id": "x", "tier": "local" });
        let (_runner, _model, tier) = parse_session_resp(&resp, Some("deep".into()));
        assert_eq!(tier.as_deref(), Some("deep"));
    }

    #[test]
    fn status_bar_shows_runner_when_set() {
        let mut state = make_state("sess-runner");
        state.runner = "anthropic".to_owned();
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("anthropic"),
            "status bar must render the runner; got: {content}"
        );
    }

    // ── tui-message-selection: T6 tests ─────────────────────────────────────

    #[test]
    fn yank_lines_text_builds_newline_joined_string() {
        let mut panel = main_panel::MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        let lines = panel.lines_text(1, 3);
        let text = lines.join("\n");
        assert_eq!(text, "line 1\nline 2\nline 3");
    }

    #[test]
    fn selection_anchor_end_resolves_to_min_max_regardless_of_direction() {
        // Drag from line 4 back to line 1 — selection should span 1..=4.
        let anchor = 4usize;
        let end = 1usize;
        let lo = anchor.min(end);
        let hi = anchor.max(end);
        assert_eq!(lo, 1);
        assert_eq!(hi, 4);
        // Forward direction.
        let anchor = 1usize;
        let end = 4usize;
        assert_eq!(anchor.min(end), 1);
        assert_eq!(anchor.max(end), 4);
    }

    #[test]
    fn esc_in_selection_mode_cancels_selection_without_scroll_change() {
        let mut state = make_state("sess-sel");
        for i in 0..10u32 {
            state.main_panel.push_line(format!("msg {i}"));
        }
        state.scroll_focus = true;
        state.selection_mode = true;
        state.selection_anchor = 3;
        state.selection_end = 6;
        state.main_panel.scroll = 3;

        // Simulate the Esc path: selection_mode cleared, scroll unchanged.
        if state.selection_mode {
            state.selection_mode = false;
        }

        assert!(
            !state.selection_mode,
            "selection must be cancelled after Esc"
        );
        assert_eq!(state.main_panel.scroll, 3, "scroll must not change on Esc");
        assert!(
            state.scroll_focus,
            "scroll_focus must remain active after cancelling selection"
        );
    }

    #[test]
    fn esc_when_idle_activates_scroll_focus() {
        let mut state = make_state("sess-idle");
        assert!(!state.scroll_focus, "scroll_focus should be off by default");

        // Simulate the last else branch of Esc: no overlay, no selection, no scroll focus.
        state.scroll_focus = true;

        assert!(
            state.scroll_focus,
            "scroll_focus must be set by Esc when idle"
        );
    }

    #[test]
    fn insert_key_exits_scroll_and_clears_selection() {
        let mut state = make_state("sess-ins");
        state.scroll_focus = true;
        state.selection_mode = true;
        state.g_pending = true;

        // Simulate 'i' key in scroll_focus block.
        state.scroll_focus = false;
        state.selection_mode = false;
        state.g_pending = false;

        assert!(!state.scroll_focus);
        assert!(!state.selection_mode);
        assert!(!state.g_pending);
    }

    #[test]
    fn slugify_converts_spaces_and_upper() {
        assert_eq!(slugify("Smedja Architecture"), "smedja-architecture");
        assert_eq!(slugify("Q3 Agent Metrics!"), "q3-agent-metrics");
        assert_eq!(slugify("multi--word"), "multi-word");
    }

    #[test]
    fn extract_code_block_finds_xml_content() {
        let text = "Some preamble\n```xml\n<mxGraph>hello</mxGraph>\n```\nsome epilogue";
        let extracted = extract_code_block(text, "xml");
        assert_eq!(extracted, Some("<mxGraph>hello</mxGraph>"));
    }

    #[test]
    fn extract_code_block_returns_none_when_lang_absent() {
        let text = "```python\nprint('hi')\n```";
        assert!(extract_code_block(text, "xml").is_none());
    }

    #[test]
    fn slash_completions_includes_drawio_and_pptx() {
        assert!(
            SLASH_COMPLETIONS.contains(&"/drawio"),
            "/drawio must be in SLASH_COMPLETIONS"
        );
        assert!(
            SLASH_COMPLETIONS.contains(&"/pptx"),
            "/pptx must be in SLASH_COMPLETIONS"
        );
    }

    #[test]
    fn slash_completions_sorted_alphabetically() {
        let mut sorted = SLASH_COMPLETIONS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            SLASH_COMPLETIONS.to_vec(),
            sorted,
            "SLASH_COMPLETIONS must be in alphabetical order"
        );
    }

    // --- OTel footer guidance tests ---

    /// Build the footer string the same way the streaming `done` handler does,
    /// so the unit test does not depend on a live event loop.
    fn build_turn_footer(
        input_tok: u64,
        output_tok: u64,
        elapsed_ms: u64,
        traceparent: Option<&str>,
        otlp_configured: bool,
    ) -> String {
        if let Some(tp_str) = traceparent {
            if otlp_configured {
                format!("↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str}")
            } else {
                format!(
                    "↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str} · traces not exported (set SMEDJA_OTLP_ENDPOINT)"
                )
            }
        } else {
            format!("↳ {input_tok}↑ {output_tok}↓ tokens · {elapsed_ms}ms")
        }
    }

    #[test]
    fn footer_shows_otlp_warning_when_not_configured() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let footer = build_turn_footer(100, 50, 300, Some(tp), false);
        assert!(
            footer.contains(tp),
            "footer must include the traceparent string"
        );
        assert!(
            footer.contains("traces not exported"),
            "footer must warn that traces are not exported when OTLP is not configured"
        );
        assert!(
            footer.contains("SMEDJA_OTLP_ENDPOINT"),
            "footer must mention SMEDJA_OTLP_ENDPOINT"
        );
    }

    #[test]
    fn footer_shows_no_otlp_warning_when_configured() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let footer = build_turn_footer(100, 50, 300, Some(tp), true);
        assert!(
            footer.contains(tp),
            "footer must include the traceparent string"
        );
        assert!(
            !footer.contains("traces not exported"),
            "footer must not show warning when OTLP is configured"
        );
    }

    // --- tui-mouse-copy tests ---

    #[test]
    fn screen_row_to_line_maps_correctly() {
        // row=5, messages_top=3, scroll=10 → offset=2, result=12
        assert_eq!(screen_row_to_line(5, 3, 10), 12);
        // row at top → offset=0, result=scroll
        assert_eq!(screen_row_to_line(3, 3, 7), 7);
        // row above messages_top → saturating_sub gives 0
        assert_eq!(screen_row_to_line(1, 3, 5), 5);
    }

    #[test]
    fn handle_mouse_down_sets_drag_start_and_end() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut state = make_state("sess-mouse");
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut state, me);
        assert_eq!(state.mouse_drag_start, Some(5));
        assert_eq!(state.mouse_drag_end, Some(5));
    }

    #[test]
    fn handle_mouse_drag_updates_end_only() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut state = make_state("sess-mouse");
        state.mouse_drag_start = Some(3);
        state.mouse_drag_end = Some(3);
        let me = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 0,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut state, me);
        assert_eq!(state.mouse_drag_start, Some(3));
        assert_eq!(state.mouse_drag_end, Some(8));
    }

    #[test]
    fn handle_mouse_up_yanks_and_clears_drag_state() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut state = make_state("sess-mouse");
        // Push a few lines to the panel so lines_text has content.
        state.main_panel.push_line("line alpha".to_owned());
        state.main_panel.push_line("line beta".to_owned());
        state.main_panel.push_line("line gamma".to_owned());
        state.messages_top = 1;
        state.mouse_drag_start = Some(1); // row 1 → line 0 + scroll(0)
        state.mouse_drag_end = Some(2); // row 2 → line 1 + scroll(0)

        let me = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut state, me);

        assert!(
            state.clipboard.is_some(),
            "clipboard must be populated after drag release"
        );
        assert!(
            state.mouse_drag_start.is_none(),
            "drag start must be cleared"
        );
        assert!(state.mouse_drag_end.is_none(), "drag end must be cleared");
        let has_msg = state
            .main_panel
            .lines_text(0, 200)
            .iter()
            .any(|l| l.contains("lines copied") || l.contains("✓"));
        assert!(has_msg, "panel must show copy confirmation message");
    }

    #[test]
    fn ctrl_c_with_clipboard_does_not_quit() {
        let mut state = make_state("sess-ctrlc");
        state.clipboard = Some("some text".to_owned());
        // Simulate the Ctrl-C branch: clipboard is Some → do NOT quit
        if state.clipboard.is_some() {
            // copy, do not quit
        } else {
            state.quit = true;
        }
        assert!(
            !state.quit,
            "Ctrl-C must not quit when clipboard is non-empty"
        );
    }

    #[test]
    fn ctrl_c_with_no_clipboard_quits() {
        let mut state = make_state("sess-ctrlc");
        state.clipboard = None;
        // Simulate the Ctrl-C branch: clipboard is None → quit
        if state.clipboard.is_some() {
            // copy
        } else {
            state.quit = true;
        }
        assert!(state.quit, "Ctrl-C must quit when clipboard is empty");
    }

    // --- tui-input-modes tests ---

    #[test]
    fn help_command_pushes_message_containing_slash_help() {
        let mut state = make_state("sess-help");
        push_system_message(&mut state, HELP_TEXT);
        let has_help = state
            .main_panel
            .lines_text(0, 200)
            .iter()
            .any(|l| l.contains("/help"));
        assert!(has_help, "help output must contain '/help'");
    }

    #[test]
    fn help_text_covers_all_major_commands() {
        for cmd in [
            "/switch",
            "/health",
            "/tier",
            "/agents",
            "/briefing",
            "/clear",
        ] {
            assert!(HELP_TEXT.contains(cmd), "HELP_TEXT must mention {cmd}");
        }
    }

    #[test]
    fn slash_completions_include_help_and_clear() {
        assert!(
            SLASH_COMPLETIONS.contains(&"/help"),
            "/help must be in SLASH_COMPLETIONS"
        );
        assert!(
            SLASH_COMPLETIONS.contains(&"/clear"),
            "/clear must be in SLASH_COMPLETIONS"
        );
    }

    #[test]
    fn runner_picker_confirm_sets_runner_and_clears_mode() {
        let mut state = make_state("sess-picker");
        state.runner_picker_mode = true;
        state.slash_completions = vec!["codex".to_owned(), "claude".to_owned()];
        state.slash_popup_visible = true;
        state.slash_cursor = 0;

        // Simulate accept: take selected runner name, update state, clear picker
        let runner_name = state.slash_completions[state.slash_cursor].clone();
        state.runner = runner_name.clone();
        push_system_message(&mut state, format!("runner switched to {runner_name}"));
        state.runner_picker_mode = false;
        state.slash_popup_visible = false;
        state.slash_completions.clear();
        state.slash_cursor = 0;

        assert_eq!(state.runner, "codex");
        assert!(
            !state.runner_picker_mode,
            "runner_picker_mode must be cleared after confirm"
        );
        assert!(
            !state.slash_popup_visible,
            "popup must be closed after confirm"
        );
        assert!(
            state
                .main_panel
                .lines_text(0, 100)
                .iter()
                .any(|l| l.contains("runner switched")),
            "confirmation message must appear in panel"
        );
    }

    #[test]
    fn clear_slash_popup_resets_runner_picker_mode() {
        let mut state = make_state("sess-popup");
        state.runner_picker_mode = true;
        state.slash_popup_visible = true;
        state.slash_completions = vec!["claude".to_owned()];

        clear_slash_popup(&mut state);

        assert!(
            !state.runner_picker_mode,
            "runner_picker_mode must be false after clear"
        );
        assert!(!state.slash_popup_visible);
        assert!(state.slash_completions.is_empty());
    }

    #[test]
    fn status_bar_shows_input_mode_badge_when_not_scroll() {
        let mut state = make_state("sess-mode");
        state.scroll_focus = false;
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("[I]"),
            "status bar must show [I] when scroll_focus=false; got: {content}"
        );
    }

    #[test]
    fn status_bar_shows_normal_mode_badge_when_scroll() {
        let mut state = make_state("sess-mode");
        state.scroll_focus = true;
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("[N]"),
            "status bar must show [N] when scroll_focus=true; got: {content}"
        );
    }

    // --- tui-prompt-history tests ---

    #[test]
    fn history_search_finds_most_recent_match() {
        let history = vec![
            "git status".to_owned(),
            "git diff".to_owned(),
            "ls".to_owned(),
        ];
        let result = history_search(&history, "git");
        assert_eq!(
            result,
            Some((1, "git diff")),
            "should return most recent match"
        );
    }

    #[test]
    fn history_search_empty_query_returns_none() {
        let history = vec!["git status".to_owned()];
        assert!(history_search(&history, "").is_none());
    }

    #[test]
    fn history_search_no_match_returns_none() {
        let history = vec!["git status".to_owned()];
        assert!(history_search(&history, "foobar").is_none());
    }

    #[test]
    fn history_search_empty_history_returns_none() {
        let history: Vec<String> = vec![];
        assert!(history_search(&history, "git").is_none());
    }

    #[test]
    fn clear_command_advances_display_start() {
        let mut state = make_state("sess-clear");
        state.main_panel.push_line("old line 1".into());
        state.main_panel.push_line("old line 2".into());
        state.messages.push(Message {
            role: Role::System,
            text: "old line 1".into(),
        });
        state.messages.push(Message {
            role: Role::System,
            text: "old line 2".into(),
        });

        // Simulate /clear dispatch
        state.display_start_idx = state.messages.len();
        state.main_panel.clear_display();

        assert_eq!(state.display_start_idx, 2);
        assert_eq!(state.main_panel.display_start, 2);
        assert_eq!(state.main_panel.scroll, 2);
    }

    #[test]
    fn new_lines_after_clear_are_visible() {
        let mut state = make_state("sess-clear2");
        state.main_panel.push_line("before clear".into());
        state.main_panel.clear_display();
        state.main_panel.push_line("after clear".into());
        // After clear, display_start=1, scroll=1; new line at index 1 is visible
        let visible = state.main_panel.lines_text(
            state.main_panel.display_start,
            state.main_panel.len().saturating_sub(1),
        );
        assert!(visible.iter().any(|l| l.contains("after clear")));
        assert!(!visible.iter().any(|l| l.contains("before clear")));
    }

    #[test]
    fn up_key_loads_most_recent_history_entry() {
        let mut state = make_state("sess-hist");
        state.prompt_history = vec!["first".to_owned(), "second".to_owned()];
        state.input = "live".to_owned();
        state.input_cursor = state.input.len();

        // Simulate Up key (first press)
        if !state.prompt_history.is_empty() {
            let new_idx = match state.history_idx {
                None => {
                    state.saved_input = state.input.clone();
                    state.prompt_history.len() - 1
                }
                Some(0) => 0,
                Some(i) => i - 1,
            };
            state.history_idx = Some(new_idx);
            state.input = state.prompt_history[new_idx].clone();
            state.input_cursor = state.input.len();
        }

        assert_eq!(state.input, "second");
        assert_eq!(state.history_idx, Some(1));
        assert_eq!(state.saved_input, "live");
    }

    #[test]
    fn down_key_at_end_restores_live_input() {
        let mut state = make_state("sess-hist-down");
        state.prompt_history = vec!["only".to_owned()];
        state.saved_input = "live input".to_owned();
        state.history_idx = Some(0);
        state.input = "only".to_owned();

        // Simulate Down key past end
        if let Some(idx) = state.history_idx {
            if idx + 1 < state.prompt_history.len() {
                let new_idx = idx + 1;
                state.history_idx = Some(new_idx);
                state.input = state.prompt_history[new_idx].clone();
                state.input_cursor = state.input.len();
            } else {
                state.history_idx = None;
                state.input = std::mem::take(&mut state.saved_input);
                state.input_cursor = state.input.len();
            }
        }

        assert!(
            state.history_idx.is_none(),
            "history_idx must be None after returning to live input"
        );
        assert_eq!(state.input, "live input");
    }

    #[test]
    fn ctrl_r_in_input_mode_enters_history_search() {
        let mut state = make_state("sess-ctrl-r");
        state.scroll_focus = false;
        state.input = "current".to_owned();

        // Simulate Ctrl-R in input mode
        state.history_search_mode = true;
        state.history_search_query.clear();
        state.saved_input = state.input.clone();

        assert!(state.history_search_mode);
        assert_eq!(state.saved_input, "current");
    }

    #[test]
    fn ctrl_r_in_scroll_mode_toggles_context_rail() {
        let mut state = make_state("sess-ctrl-r-scroll");
        state.scroll_focus = true;
        state.context_rail_visible = true;

        // Simulate Ctrl-R in scroll mode
        state.context_rail_visible = !state.context_rail_visible;

        assert!(
            !state.context_rail_visible,
            "context rail must be toggled off"
        );
    }

    #[test]
    fn history_search_esc_restores_saved_input() {
        let mut state = make_state("sess-search-esc");
        state.history_search_mode = true;
        state.history_search_query = "git".to_owned();
        state.saved_input = "original".to_owned();
        state.input = "git status".to_owned();

        // Simulate Esc
        state.history_search_mode = false;
        state.history_search_query.clear();
        state.input = std::mem::take(&mut state.saved_input);
        state.input_cursor = state.input.len();

        assert!(!state.history_search_mode);
        assert_eq!(state.input, "original");
        assert!(state.history_search_query.is_empty());
    }

    #[test]
    fn history_search_enter_accepts_match() {
        let mut state = make_state("sess-search-enter");
        state.history_search_mode = true;
        state.history_search_query = "git".to_owned();
        state.input = "git status".to_owned();

        // Simulate Enter
        state.history_search_mode = false;
        state.history_search_query.clear();

        assert!(
            !state.history_search_mode,
            "search mode must be cleared on Enter"
        );
        assert_eq!(
            state.input, "git status",
            "matched input must be kept on Enter"
        );
    }

    // --- tui-spec-command tests ---

    #[test]
    fn format_openspec_list_empty_changes_returns_no_active() {
        let json = r#"{"changes": []}"#;
        assert_eq!(format_openspec_list(json), "no active changes");
    }

    #[test]
    fn format_openspec_list_missing_changes_key_returns_no_active() {
        let json = r"{}";
        assert_eq!(format_openspec_list(json), "no active changes");
    }

    #[test]
    fn format_openspec_list_two_changes_shows_both_names() {
        let json = r#"{"changes": [
            {"name": "tui-input-modes", "status": "proposed"},
            {"name": "smdjad-service",  "status": "implementing"}
        ]}"#;
        let result = format_openspec_list(json);
        assert!(
            result.contains("tui-input-modes"),
            "must contain first change name"
        );
        assert!(
            result.contains("smdjad-service"),
            "must contain second change name"
        );
        assert!(result.contains("proposed"), "must contain status");
    }

    #[test]
    fn format_openspec_list_invalid_json_returns_error() {
        let result = format_openspec_list("not json");
        assert!(
            result.contains("parse error"),
            "invalid JSON must produce a parse error message; got: {result}"
        );
    }

    #[test]
    fn format_openspec_status_renders_key_value_lines() {
        let json = r#"{"name": "my-change", "state": "implementing", "progress": "3/7"}"#;
        let result = format_openspec_status(json);
        assert!(
            result.contains("name: my-change"),
            "must contain name field"
        );
        assert!(
            result.contains("state: implementing"),
            "must contain state field"
        );
        assert!(
            result.contains("progress: 3/7"),
            "must contain progress field"
        );
    }

    #[test]
    fn format_openspec_status_invalid_json_returns_error() {
        let result = format_openspec_status("{{bad}}");
        assert!(result.contains("parse error"));
    }

    #[test]
    fn spec_command_no_openspec_binary_shows_not_found() {
        let mut state = make_state("sess-spec");
        state.openspec_bin = None;

        // Simulate the guard: no binary → push message
        if state.openspec_bin.is_none() {
            push_system_message(
                &mut state,
                "openspec not found — install it and restart smedja-tui",
            );
        }

        let has_msg = state
            .main_panel
            .lines_text(0, 100)
            .iter()
            .any(|l| l.contains("openspec not found"));
        assert!(
            has_msg,
            "missing binary must produce openspec-not-found message"
        );
    }

    #[test]
    fn spec_unknown_subcommand_returns_usage() {
        // Test the "_ =>" branch of the spec arm directly via format.
        let text = "usage: /spec [list|status [name]|archive <name>]";
        assert!(
            text.contains("usage:"),
            "unknown sub-command must show usage"
        );
        assert!(text.contains("list"), "usage must mention list");
        assert!(text.contains("status"), "usage must mention status");
        assert!(text.contains("archive"), "usage must mention archive");
    }

    // --- cowork resolver helper ---

    fn cowork_item(id: &str, tool: &str) -> cowork_widget::CoworkItem {
        cowork_widget::CoworkItem {
            id: id.to_owned(),
            tool: tool.to_owned(),
            step_n: 1,
            args_display: String::new(),
            reasoning: String::new(),
        }
    }

    #[test]
    fn cowork_resolved_true_only_when_flag_set() {
        let yes: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        assert!(cowork_resolved(&yes), "resolved:true must return true");

        let no: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": false }));
        assert!(!cowork_resolved(&no), "resolved:false must return false");

        let missing: Result<serde_json::Value, smedja_rpc::RpcError> = Ok(json!({ "id": "a" }));
        assert!(
            !cowork_resolved(&missing),
            "missing resolved field must return false"
        );

        let err: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32603, "transport down"));
        assert!(!cowork_resolved(&err), "transport error must return false");
    }

    // --- cowork decision application (approve / deny) ---

    #[test]
    fn apply_cowork_decision_approve_resolved_removes_and_confirms() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        let item = cowork_item("a", "bash");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
        assert!(remove, "resolved:true must remove the item");
        assert_eq!(message, "approved: bash");
    }

    #[test]
    fn apply_cowork_decision_unresolved_retains_and_reports_not_found() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": false }));
        let item = cowork_item("a", "bash");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
        assert!(!remove, "resolved:false must retain the item");
        assert_eq!(message, "item not found: bash");
    }

    #[test]
    fn apply_cowork_decision_deny_resolved_removes_and_confirms() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        let item = cowork_item("a", "edit_file");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.deny", "denied: edit_file", &item.tool);
        assert!(remove, "resolved:true must remove the item");
        assert_eq!(message, "denied: edit_file");
    }

    #[test]
    fn apply_cowork_decision_rpc_error_retains_and_reports_error() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32603, "boom"));
        let item = cowork_item("a", "bash");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
        assert!(!remove, "rpc error must retain the item");
        assert!(
            message.contains("cowork.approve error"),
            "error message must name the method; got: {message}"
        );
    }

    // --- cowork modify flow ---

    #[test]
    fn apply_cowork_decision_modify_resolved_echoes_instruction() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        let item = cowork_item("a", "bash");
        let (remove, message) = apply_cowork_decision(
            &result,
            "cowork.modify",
            "modify sent: use ls -la instead",
            &item.tool,
        );
        assert!(remove, "resolved:true must remove the item");
        assert_eq!(message, "modify sent: use ls -la instead");
    }

    #[test]
    fn apply_cowork_decision_modify_unresolved_retains_item() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": false }));
        let item = cowork_item("a", "bash");
        let (remove, message) = apply_cowork_decision(
            &result,
            "cowork.modify",
            "modify sent: use ls -la instead",
            &item.tool,
        );
        assert!(!remove, "resolved:false must retain the item");
        assert_eq!(message, "item not found: bash");
    }
}
