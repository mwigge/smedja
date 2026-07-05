pub mod action_log;
mod alerts;
mod blocks;
mod clipboard;
mod context_rail;
mod cowork_widget;
mod diff_viewer;
mod editor;
mod fleet_panel;
mod formatting;
mod governance;
mod live_line;
mod lsp_panel;
pub mod main_panel;
mod metrics_view;
mod obs_panel;
mod plan_panel;
mod quality_panel;
mod secrets;
pub(crate) mod slash;
mod staging;
mod statusbar;
mod terminal_guard;
pub mod theme;
mod thoughts_panel;
mod tool_call;
mod trace_waterfall;
mod upgrade;
mod value_panel;
mod viz;

mod bootstrap;
mod events;
mod input;
mod render;
mod run_loop;
mod state;

// Re-export slash module items so that `use super::*` in the test module
// continues to find them without change.  The `#[allow(unused_imports)]` is
// needed because the compiler does not see the indirect usage via `use super::*`
// in the test module.
#[allow(unused_imports)]
pub(crate) use slash::{
    apply_agent, apply_tier, dispatch_slash, format_agents_table, format_approvals_list,
    format_local_model_list, format_metrics, format_model_list,
};

// Re-export extracted module items so callers (slash.rs, tests) see them
// at the crate root unchanged.
#[allow(unused_imports)]
pub(crate) use clipboard::{
    emit_osc9, osc9_turn_complete_bytes, paste_from_clipboard, push_kill, yank_to_clipboard,
};
#[allow(unused_imports)]
pub(crate) use editor::{open_in_editor, resolve_editor};
#[allow(unused_imports)]
pub(crate) use events::{apply_stream_event, start_stream_reader};
#[allow(unused_imports)]
pub(crate) use governance::{
    format_gov_list, gov_create, gov_transition, scan_gov_artifacts, GovArtifact,
};
#[allow(unused_imports)]
pub(crate) use input::{
    accept_slash_completion, apply_cowork_decision, clear_slash_popup, cowork_resolved, handle_key,
};
#[allow(unused_imports)]
pub(crate) use render::render;
#[allow(unused_imports)]
pub(crate) use state::{AppState, Message, PanelVisibility, Role, SessionDetail};
#[allow(unused_imports)]
pub(crate) use terminal_guard::TerminalGuard;
#[allow(unused_imports)]
pub(crate) use tool_call::tool_call_card;
#[allow(unused_imports)]
pub(crate) use upgrade::{
    fetch_latest_version, format_openspec_list, format_openspec_status, is_newer, run_openspec,
    run_upgrade, VERSION,
};

use std::collections::VecDeque;
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use statusbar::ModuleCtx;

use crate::theme::{palette, runner_color, runner_label};
use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseEventKind, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use crossterm::{event, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Terminal;
use serde_json::{json, Value};
use smedja_bellows::StreamEvent;
use smedja_rpc::client::Client;

/// Returns the largest byte index `<= max` that lies on a UTF-8 char boundary of
/// `s`. Use this before `&s[..n]` byte slicing so multibyte names (e.g.
/// `"café_αβγ_日本語"`) never panic mid-codepoint. Ported from the st-statusbar
/// helper to avoid a dependency on the `term/` crates.
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "smedja-tui", version, about = "smedja agent dashboard (TUI)")]
struct Cli {
    /// smdjad socket path (default: `$XDG_RUNTIME_DIR/smdjad.sock`)
    #[arg(long, env = "SMEDJA_SOCK")]
    sock: Option<PathBuf>,

    /// Agent role (impl|plan|research|debug|ask|review|test|sre|data|iac|orchestrator)
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

/// Structured output type requested by a generator slash command.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutputType {
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
pub(crate) fn resume_plan(turn: Option<u32>) -> ResumePlan {
    match turn {
        Some(turn_n) => ResumePlan::Rollback { turn_n },
        None => ResumePlan::ReplayOnly,
    }
}

/// Available slash-command completions shown in the popup.
/// Short descriptions shown in the command palette (Ctrl+K). Order matches `SLASH_COMPLETIONS`.
const SLASH_COMMAND_DESCRIPTIONS: &[(&str, &str)] = &[
    ("/agent", "run named agent"),
    ("/approve", "approve a cowork item"),
    ("/briefing", "show session briefing"),
    (
        "/capabilities",
        "list provider capabilities (thinking, subprocess, model)",
    ),
    ("/clear", "clear message display"),
    ("/cowork", "toggle cowork approval mode"),
    ("/drawio", "generate draw.io diagram"),
    ("/gov", "govctl artifacts"),
    ("/health", "check daemon connectivity"),
    ("/help", "show help"),
    ("/index", "build the code graph"),
    ("/login", "authenticate with runner"),
    ("/loop", "manage loop runs"),
    ("/lsp", "LSP status and diagnostics"),
    ("/memory", "list stored memory"),
    ("/metrics", "show token usage and cost"),
    ("/model", "show or set model"),
    ("/pptx", "generate PowerPoint"),
    ("/quit", "exit smedja-tui"),
    ("/quota", "show usage quota"),
    ("/resume", "resume a session"),
    ("/review", "send git diff for review"),
    ("/session", "manage sessions"),
    ("/skills", "list loaded skills"),
    ("/spec", "browse OpenSpec changes"),
    ("/switch", "switch active session"),
    ("/takeover", "take over agent output"),
    ("/test", "run test suite"),
    ("/tier", "show or set tier"),
    ("/tools", "list available tools"),
    ("/upgrade", "upgrade smedja"),
    ("/version", "show version"),
];

const SLASH_COMPLETIONS: &[&str] = &[
    "/agent",
    "/approve",
    "/briefing",
    "/capabilities",
    "/clear",
    "/cowork",
    "/drawio",
    "/gov",
    "/health",
    "/help",
    "/index",
    "/login",
    "/loop",
    "/lsp",
    "/memory",
    "/metrics",
    "/model",
    "/pptx",
    "/quit",
    "/quota",
    "/resume",
    "/review",
    "/session",
    "/skills",
    "/spec",
    "/switch",
    "/takeover",
    "/test",
    "/tier",
    "/tools",
    "/upgrade",
    "/version",
];

pub(crate) const HELP_TEXT: &str = "\
slash commands:
  /agent [role]      — set the session role (impl|plan|research|debug|ask|review|test|sre|data|iac|orchestrator); omit to list runners
  /approve [id]      — approve a cowork item (omit id to list pending approvals)
  /briefing          — show session briefing
  /clear             — clear message display (keeps session data)
  /cowork on|off|status — toggle or query cowork approval mode
  /drawio <slug>     — generate draw.io diagram
  /gov [list|show <id>|create work-item|rfc|adr <title>|transition <id> <status>] — govctl artifacts
  /health            — check daemon connectivity
  /help              — show this message
  /login             — authenticate with runner
  /loop [status|list|create <goal>|cancel] — manage loop runs
  /index [path]      — build the code graph for the workspace (auto-injected into context)
  /lsp               — show LSP server status and diagnostic summary
  /memory [session]  — list stored memory (turn history); pass a session id to view another's
  /metrics           — show token usage and cost
  /model [name]      — show or set model (local runner: lists GPU fit / hot-swaps)
  /pptx <slug>       — generate PowerPoint
  /quit              — exit smedja-tui
  /quality           — trigger Tier-2 LLM quality review (Ctrl-Q hold for 500ms also fires this)
  /value             — print ROI report for the active openspec change
  /quota             — show usage quota
  /resume [id [turn]] — resume a session (omit id for interactive picker; turn rewinds)
  /review            — send git diff for review
  /spec              — browse OpenSpec changes
  /skills [add <dir>] — list skills (~/.claude/skills + .smedja/skills) or add a directory
  /switch [runner]   — switch AI runner (omit for interactive picker)
  /takeover <runner> — fork session to new runner
  /test              — run cargo test and show a pass/fail summary
  /tools             — list recent tool calls (right-click a tool card for full args)
  /tier <t>          — set tier (local|fast|deep)
  /version           — print current version and check for a newer release
  /upgrade           — download and install the latest release in-place

inline context fragments (expanded into your message before the turn runs):
  @file <path>       — inject a workspace file's contents (path stays inside the workspace)
  @git               — inject `git status --short` and `git diff HEAD`
  @branch            — inject the current branch and upstream
  @shell <cmd>       — inject a shell command's output (gated by cowork when enabled)

keybindings (input mode):
  Esc                — enter scroll/normal mode
  Enter              — submit the message
  Shift/Alt-Enter    — insert a newline (compose multi-line in place)
  Up / Ctrl-P        — browse history backwards
  Down / Ctrl-N      — browse history forwards
  Ctrl-R             — toggle reverse history search
  Ctrl-G             — open $EDITOR / $VISUAL to compose a multi-line message
  Ctrl-B             — move cursor left one character
  Ctrl-K             — kill from cursor to end of line (push to kill ring)
  Ctrl-U             — kill from start of line to cursor (push to kill ring)
  Ctrl-Y             — yank most recent kill at cursor

keybindings (scroll/normal mode):
  i / a              — return to input mode
  j / k              — scroll down / up
  G                  — scroll to bottom
  gg                 — scroll to top
  Ctrl-A             — toggle role cockpit panel (active role/tier/turn status)
  Ctrl-F             — toggle context rail
  Ctrl-G             — toggle multi-agent fleet roster panel
  Ctrl-L             — toggle LSP diagnostic panel
  Ctrl-O             — toggle observability panel (with the turn trace waterfall)
  Ctrl-Q             — toggle quality gate panel
  Ctrl-V             — toggle value / ROI panel (Ctrl-V in input mode pastes)
  Ctrl-W             — toggle session browser (left rail)
  x                  — inspect trace-waterfall spans (scroll mode)
  Alt+↑ / Alt+↓     — move cursor up / down in session rail (input mode)
  [ / ]              — move cursor up / down in session rail (scroll mode)
  mouse drag         — mark lines in the messages panel; release copies them
  v                  — start line selection (visual mode)
  y                  — yank selection to clipboard
  t                  — copy traceparent
  T                  — expand / collapse thinking block (when model emits thinking tokens)
  /                  — search panel text (type to filter, Esc to clear)
  Esc                — exit selection / return to input

note: scroll wheel scrolls the main panel; drag the mouse over messages to mark
      and copy, or use v/y in scroll mode. Long messages wrap to the panel width.";

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
            // Announce our working directory as the workspace so the daemon
            // roots the LSP + code-graph at the project (not its own $HOME).
            let workspace = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let resp = client
                .call(
                    "session.create",
                    json!({ "title": "smedja", "workspace": workspace }),
                )
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
                runner: resp
                    .get("runner")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
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
pub(crate) async fn resume_into_view(state: &mut AppState, client: &mut Client, plan: ResumePlan) {
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

pub(crate) async fn submit(input: &str, state: &mut AppState, client: &mut Client) -> Result<()> {
    let text = input.trim().to_owned();
    if text.is_empty() {
        return Ok(());
    }
    if state.turn_in_flight {
        push_system_message(state, "a turn is already in flight — press Esc to cancel");
        return Ok(());
    }
    state.prompt_history.push(text.clone());
    if state.prompt_history.len() > PROMPT_HISTORY_CAP {
        state.prompt_history.remove(0);
    }
    state.history_idx = None;
    state.saved_input.clear();
    let user_msg = Message {
        role: Role::User,
        text: text.clone(),
    };
    // Author chip + message body. The user identity leads with the signature
    // molten lava-orange + a heavy `▌` bar so it is unmistakable against the
    // amber chrome and the assistant's cooler brand colour. Reset the assistant
    // chip latch so the next response emits its own boundary on a fresh line.
    let you_color = palette().molten;
    push_author_chip(
        &mut state.main_panel,
        "\u{258c}",
        "you",
        you_color,
        state.no_color,
    );
    state.main_panel.push_line(user_msg.text.clone());
    state.assistant_open = false;
    state.push_message(user_msg);
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
            state.current_thinking.clear();
            state.thinking_steps.clear();
            state.thinking_expanded = false;
            state.active_agent_name = None;
            state.plan_steps.clear();
            // Reset the live line / trace / plan tracking for the new turn.
            state.current_trace.start_turn();
            state.trace_selected = 0;
            state.trace_expanded = false;
            state.live_tokens = 0;
            // Fresh per-turn token high-water marks for the obs throughput bar.
            state.turn_tokens_in = 0;
            state.turn_tokens_out = 0;
            state.last_stream_activity = Some(std::time::Instant::now());
            state.tool_started_at = None;
            state.plan_current = 0;

            // Start streaming reader; events arrive via unbounded channel.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            state.stream_rx = Some(rx);
            let sock = state.stream_sock_path.clone();
            let tid = task_id.clone();
            tokio::spawn(start_stream_reader(sock, tid, tx));

            // Dim provider hint — visible immediately while the turn is in flight,
            // before the first token arrives.
            {
                let p = palette();
                let label = theme::runner_label(&state.runner).to_lowercase();
                let model_part = state
                    .model
                    .as_deref()
                    .map_or_else(String::new, |m| format!(" · {m}"));
                state.main_panel.push_styled_line(Line::from(Span::styled(
                    format!("↪ {label}{model_part}"),
                    Style::default().fg(p.text_dim),
                )));
            }

            // "queued" is operational noise — route it to the actions log, not
            // the message box (keeps the conversation clean). Lead with the
            // session id (same 12-char form as the session rail) so the queued
            // task can be tied back to its session; the task id is a separate
            // per-turn handle and is shown short, after.
            let sid = &state.session_id[..state.session_id.len().min(12)];
            let short_task = &task_id[..task_id.len().min(8)];
            push_action_log(state, format!("queued · session {sid} · task {short_task}"));
            None
        }
        Err(ref e) => Some(format!("error: {e}")),
    };
    // Only genuine errors surface in the message panel now.
    if let Some(text) = reply {
        let sys_msg = Message {
            role: Role::System,
            text,
        };
        state.main_panel.push_line(sys_msg.text.clone());
        state.push_message(sys_msg);
    }
    Ok(())
}

/// Appends a single operational entry to the actions log (the "emit" rail),
/// timestamped, without touching the message panel.
fn push_action_log(state: &mut AppState, action: impl Into<String>) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let ts = format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    );
    state.action_log.push(action_log::AuditEntry {
        timestamp: ts,
        action: action.into(),
        tool_name: String::new(),
        outcome: "sys".to_owned(),
    });
}

// ---------------------------------------------------------------------------
// Slash completion helpers
// ---------------------------------------------------------------------------

/// Lists directory entries for the file picker: `../` first, then sorted dirs, then files.
fn list_dir_entries(dir: &std::path::Path) -> Vec<(String, bool)> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    if dir.parent().is_some() {
        entries.push(("../".to_owned(), true));
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // skip hidden
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            dirs.push((format!("{name}/"), true));
        } else {
            files.push((name, false));
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    entries.extend(dirs);
    entries.extend(files);
    entries
}

/// Opens the file picker overlay rooted at `dir`.
fn open_file_picker(state: &mut AppState, dir: std::path::PathBuf) {
    state.file_picker_entries = list_dir_entries(&dir);
    state.file_picker_dir = dir;
    state.file_picker_cursor = 0;
    state.file_picker_open = true;
}

/// Returns completions from `SLASH_COMPLETIONS` whose prefix matches `input`.
fn filtered_completions(input: &str) -> Vec<String> {
    SLASH_COMPLETIONS
        .iter()
        .copied()
        .filter(|c| c.starts_with(input))
        .map(str::to_owned)
        .collect()
}

/// Returns all slash commands whose name contains `query` as a substring (case-insensitive).
/// An empty query returns every command.
fn command_palette_filtered(query: &str) -> Vec<String> {
    let q = query.to_ascii_lowercase();
    SLASH_COMPLETIONS
        .iter()
        .copied()
        .filter(|c| q.is_empty() || c.to_ascii_lowercase().contains(&q))
        .map(str::to_owned)
        .collect()
}

/// Classifies an LLM turn error message into a short label and optional hint.
///
/// Returns `(label, hint)` where `hint` is empty when there is nothing useful
/// to suggest.  The label is used to prefix the displayed error line.
use formatting::{classify_turn_error, format_turn_error};

pub(crate) fn push_system_message(state: &mut AppState, text: impl Into<String>) {
    let msg = Message {
        role: Role::System,
        text: text.into(),
    };
    // Short single-line operational messages are also routed to the action log
    // (the "emit" rail in the SuperConsole pattern) so they appear in both
    // the main panel and the scrolling event strip.
    let first_line = msg.text.lines().next().unwrap_or("").to_owned();
    if !msg.text.contains('\n') {
        let ts = {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let h = (secs / 3600) % 24;
            let m = (secs / 60) % 60;
            let s = secs % 60;
            format!("{h:02}:{m:02}:{s:02}")
        };
        state.action_log.push(action_log::AuditEntry {
            timestamp: ts,
            action: first_line,
            tool_name: String::new(),
            outcome: "sys".to_owned(),
        });
    }
    state.main_panel.push_line(msg.text.clone());
    state.push_message(msg);
}

/// Formats a tool call's full arguments into overlay lines, pretty-printing the
/// JSON input when possible. Used by right-click expansion and `/tools`.
pub(crate) fn format_tool_detail(name: &str, full: &str) -> Vec<String> {
    let mut lines = vec![format!("tool: {name}"), String::new()];
    let pretty = serde_json::from_str::<serde_json::Value>(full)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| full.to_owned());
    lines.extend(pretty.lines().map(str::to_owned));
    lines.push(String::new());
    lines.push("(Esc to close)".to_owned());
    lines
}

/// Builds an author chip line (`▌ you` / `▏ codex`) marking a turn boundary so
/// messages have clear authorship. The `glyph` is a role-specific left-gutter
/// bar — a heavy `▌` for the user (loud, owns the turn) and a thin `▏` for the
/// assistant (quiet, since it owns the content width). Pushed on its own line;
/// the message body follows beneath it.
///
/// Pushes an author chip, preceded by a blank spacer line (a turn separator)
/// when the panel already has content — so successive turns read as distinct
/// blocks instead of one running mass of text.
fn push_author_chip(
    panel: &mut main_panel::MainPanel,
    glyph: &str,
    label: &str,
    color: Color,
    no_color: bool,
) {
    if !panel.is_empty() {
        panel.push_styled_line(Line::from(""));
    }
    panel.push_styled_line(author_chip(glyph, label, color, no_color));
}

fn author_chip(glyph: &str, label: &str, color: Color, no_color: bool) -> Line<'static> {
    let style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    Line::from(Span::styled(format!("{glyph} {label}"), style))
}

/// Pushes a run of dim chrome lines (startup banner, operational notices) so
/// they never out-shout the conversation. Every line is muted per the
/// dim-the-chrome rule.
pub(crate) fn push_chrome_line(panel: &mut main_panel::MainPanel, text: impl Into<String>) {
    let p = palette();
    panel.push_styled_line(Line::from(Span::styled(
        text.into(),
        Style::default().fg(p.text_dim).add_modifier(Modifier::DIM),
    )));
}

/// Builds the starship-style segmented status line: a mode pip, a runner chip
/// (brand-coloured), tier, mode, and a dim session id, separated by thin dots.
/// Colour-segmented rather than powerline-glyph based, so it needs no Nerd Font.
fn status_bar_line(ctx: &ModuleCtx<'_>, no_color: bool) -> Line<'static> {
    let p = palette();
    let plain = no_color;
    let dim = if plain {
        Style::default()
    } else {
        Style::default().fg(p.text_dim).bg(p.panel)
    };
    let sep = || Span::styled(" · ", dim);
    let chip = |text: String, color: Color, bold: bool| {
        let mut s = if plain {
            Style::default()
        } else {
            Style::default().fg(color).bg(p.panel)
        };
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        Span::styled(text, s)
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    // Mode pip — input vs scroll.
    let (pip, pip_label) = if ctx.input_mode {
        ("●", "INSERT")
    } else {
        ("◆", "SCROLL")
    };
    spans.push(chip(format!("{pip} {pip_label}"), p.accent, true));

    if let Some(runner) = ctx.runner {
        spans.push(sep());
        spans.push(chip(
            format!("◆ {}", runner_label(runner)),
            runner_color(runner),
            true,
        ));
    }
    if let Some(tier) = ctx.tier {
        spans.push(sep());
        let c = match tier {
            "local" => p.local,
            "deep" => p.deep,
            _ => p.fast,
        };
        spans.push(chip(tier.to_owned(), c, false));
    }
    if let Some(mode) = ctx.mode {
        spans.push(sep());
        let mc = crate::theme::agent_color(mode);
        spans.push(chip(mode.to_owned(), mc, false));
    }
    spans.push(sep());
    spans.push(chip(
        ctx.session_id.chars().take(8).collect::<String>(),
        p.text_dim,
        false,
    ));
    if let Some(pct) = ctx.ctx_pct {
        spans.push(sep());
        let color = if pct >= 80 {
            p.error
        } else if pct >= 60 {
            p.warn
        } else {
            p.text_dim
        };
        spans.push(chip(format!("▓ {pct}%"), color, false));
    }
    if ctx.pending {
        spans.push(chip("  ⟳".to_owned(), p.accent, true));
    }
    Line::from(spans)
}

/// Returns `true` when `runner` supports extended thinking tokens.
fn runner_supports_thinking(runner: &str) -> bool {
    matches!(runner, "anthropic")
}

/// Returns `true` when `runner` is a subprocess CLI wrapper rather than a
/// native HTTP provider.
fn runner_is_subprocess(runner: &str) -> bool {
    matches!(runner, "claude-cli" | "codex-cli")
}

/// Formats a capability table from a `runner.list` response array.
///
/// Each row shows runner name, tier, model, and derived capability flags
/// (thinking support, subprocess mode).
fn format_capabilities_table(runners: &[serde_json::Value]) -> String {
    if runners.is_empty() {
        return "no runners available".to_owned();
    }
    let mut lines = vec![format!(
        "{:<16} {:<8} {:<8} {:<36}",
        "runner", "tier", "flags", "model"
    )];
    lines.push("-".repeat(72));
    for r in runners {
        let name = r.get("runner").and_then(|v| v.as_str()).unwrap_or("?");
        let tier = r.get("tier").and_then(|v| v.as_str()).unwrap_or("-");
        let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("-");
        let mut flags: Vec<&str> = Vec::new();
        if runner_supports_thinking(name) {
            flags.push("thinking");
        }
        if runner_is_subprocess(name) {
            flags.push("subprocess");
        }
        let flag_str = if flags.is_empty() {
            "-".to_owned()
        } else {
            flags.join(",")
        };
        lines.push(format!("{name:<16} {tier:<8} {flag_str:<8} {model}"));
    }
    lines.join("\n")
}

/// A dim, right-aligned discoverability hint for the status row — surfaces the
/// few entry points (slash commands + the rail toggles) so they are not
/// keybind-only knowledge.
fn status_hint_line(no_color: bool) -> Line<'static> {
    let p = palette();
    let style = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.text_dim).bg(p.panel)
    };
    Line::from(Span::styled(
        "/help · ^W/^⇧W sessions · ^O obs · ^L lsp ".to_owned(),
        style,
    ))
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

    // Seed latency samples from audit events so the p95/p99 sparkline has
    // historical data rather than starting blank on every session load.
    if let Some(audit) = history.get("audit").and_then(serde_json::Value::as_array) {
        for ev in audit {
            if let Some(ms) = ev.get("latency_ms").and_then(serde_json::Value::as_u64) {
                if ms > 0 {
                    if state.latency_samples.len() >= LATENCY_SAMPLE_CAP {
                        state.latency_samples.pop_front();
                    }
                    state.latency_samples.push_back(ms);
                }
            }
        }
        state.obs_snapshot.latency_samples = state.latency_samples.clone();
    }
}

/// Formats `session.list` rows for the `/resume` picker.
///
/// Each row renders as `<short-id>  <title>  <mode>  <updated_at>`, where the
/// short id is the first 8 characters. Missing titles/modes degrade to empty
/// or `?` placeholders rather than being dropped.
#[must_use]
pub(crate) fn format_resume_rows(list: &serde_json::Value) -> Vec<String> {
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
            let updated =
                if let Some(s_val) = s.get("updated_at").and_then(serde_json::Value::as_str) {
                    s_val.to_owned()
                } else if let Some(n) = s.get("updated_at").and_then(serde_json::Value::as_f64) {
                    // epoch microseconds → relative display
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let secs = (n / 1_000_000.0) as i64;
                    #[allow(clippy::cast_possible_wrap)]
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs() as i64);
                    let ago = now - secs;
                    if ago < 60 {
                        format!("{ago}s ago")
                    } else if ago < 3600 {
                        format!("{}m ago", ago / 60)
                    } else if ago < 86400 {
                        format!("{}h ago", ago / 3600)
                    } else {
                        format!("{}d ago", ago / 86400)
                    }
                } else {
                    "-".to_owned()
                };
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
pub(crate) fn parse_resume_args(args: &str) -> Option<(String, Option<u32>)> {
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
pub(crate) fn resume_blocked_by_pending_turn(state: &mut AppState) -> bool {
    if state.pending_task_id.is_some() {
        push_system_message(state, "cannot resume while a turn is in flight");
        true
    } else {
        false
    }
}

/// Slugify `topic` for use in output filenames.
pub(crate) fn slugify(topic: &str) -> String {
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
pub(crate) fn parse_review_scope(args: &str) -> serde_json::Value {
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
pub(crate) fn render_findings_summary(
    counts: &serde_json::Value,
    report_path: Option<&str>,
) -> String {
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
use formatting::extract_code_block;

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
            match std::fs::write(&script_path, script) {
                Ok(()) => push_system_message(
                    state,
                    format!(
                        "presentation script saved: ./{script_path}\nrun explicitly: python3 {script_path}"
                    ),
                ),
                Err(e) => push_system_message(state, format!("failed to write script {script_path}: {e}")),
            }
        }
    }
}

/// Maximum visual rows the input field grows to before it scrolls internally.
const INPUT_MAX_ROWS: u16 = 6;

use formatting::{history_search, next_char_boundary, prev_char_boundary, wrap_input_rows};

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Detects model / tier / context-compaction transitions since the last turn
/// and pushes a highlighted transcript divider for each, dolphie-style. Silent
/// on the first observation (it only seeds the baseline).
fn check_transitions(state: &mut AppState) {
    const W: usize = 60;
    // Model change.
    let model = state.model.clone();
    if state.prev_model.is_some() && state.prev_model != model {
        let from = state.prev_model.as_deref().unwrap_or("?");
        let to = model.as_deref().unwrap_or("?");
        let line = alerts::model_change_line(from, to, W, state.no_color);
        state.main_panel.push_styled_line(line);
    }
    state.prev_model = model;

    // Tier change.
    let tier = state.tier.clone();
    if state.prev_tier.is_some() && state.prev_tier != tier {
        let from = state.prev_tier.as_deref().unwrap_or("?");
        let to = tier.as_deref().unwrap_or("?");
        let line = alerts::tier_change_line(from, to, W, state.no_color);
        state.main_panel.push_styled_line(line);
    }
    state.prev_tier = tier;

    // Context compaction — a drop of more than 30 % from the prior turn.
    let now = state.context_used;
    #[allow(clippy::cast_precision_loss)]
    if state.prev_context_used > 0 && (now as f64) < (state.prev_context_used as f64) * 0.7 {
        let line = alerts::compaction_line(state.prev_context_used, now, W, state.no_color);
        state.main_panel.push_styled_line(line);
    }
    state.prev_context_used = now;
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Resolves the TUI log file path: `$XDG_STATE_HOME/smedja/smedja-tui.log`
/// (falling back to `~/.local/state/smedja/`). Creates the directory.
fn tui_log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let dir = base.join("smedja");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("smedja-tui.log"))
}

/// Initialises tracing to a **file**, honouring `SMEDJA_LOG_FORMAT` (`text`
/// default | `json`).
///
/// Crucially this never writes to stdout/stderr: this process is a full-screen
/// ratatui app that owns the terminal, and log lines on that stream would be
/// painted straight into the UI (interleaved garbage). If the log file cannot
/// be opened we install no subscriber at all rather than corrupt the display.
fn init_tracing() {
    let Some(path) = tui_log_path() else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    // The fmt layer clones the file handle per write via this closure; on the
    // (essentially OOM-only) clone failure we discard the line rather than panic.
    let make_writer = move || -> Box<dyn std::io::Write> {
        file.try_clone().map_or_else(
            |_| Box::new(std::io::sink()) as Box<dyn std::io::Write>,
            |f| Box::new(f),
        )
    };
    match std::env::var("SMEDJA_LOG_FORMAT").as_deref() {
        Ok("json") => tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_writer(make_writer)
            .init(),
        _ => tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(make_writer)
            .init(),
    }
}

/// Loads optional `[tui.colors]` overrides from `~/.config/smedja/config.toml`.
///
/// Returns `None` if the file is absent, unreadable, or has no `[tui.colors]`
/// section; in all cases the forge defaults apply.
fn load_tui_colors() -> Option<crate::theme::TuiColorConfig> {
    #[derive(serde::Deserialize, Default)]
    struct TuiSection {
        colors: Option<crate::theme::TuiColorConfig>,
    }
    #[derive(serde::Deserialize, Default)]
    struct ConfigFile {
        tui: Option<TuiSection>,
    }

    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join(".config")
        .join("smedja")
        .join("config.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    let cfg: ConfigFile = toml::from_str(&text).ok()?;
    cfg.tui?.colors
}

#[tokio::main]
async fn main() -> Result<()> {
    let session = bootstrap::bootstrap().await?;
    run_loop::run(session).await
}

// ---------------------------------------------------------------------------
// Reconnect helper
// ---------------------------------------------------------------------------

/// Attempts to re-establish a connection to the smdjad socket after a
/// transport-level failure (e.g. daemon restart).
///
/// Tries up to 3 times with exponential backoff (500 ms → 1 s → 2 s).
/// Returns `Some(client)` on success, `None` if all attempts fail.
async fn try_reconnect(sock: &std::path::Path) -> Option<Client> {
    for attempt in 0..3u32 {
        tokio::time::sleep(std::time::Duration::from_millis(
            500 * u64::from(2u32.pow(attempt)),
        ))
        .await;
        if let Ok(client) = Client::connect(sock).await {
            return Some(client);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// metrics-live-fetch: pure helpers (off the render hot path, unit-testable)
// ---------------------------------------------------------------------------

/// Refresh interval for the metrics panel poll. Metrics are aggregates, not live
/// deltas, so a slow cadence is correct and cheap.
/// Maximum number of turn latency samples retained for p95/p99 computation.
const LATENCY_SAMPLE_CAP: usize = 50;

/// Maximum number of entries kept in the prompt history ring.
pub(crate) const PROMPT_HISTORY_CAP: usize = 500;

/// Upper bound on retained [`AppState::messages`] scrollback entries. Older
/// entries are dropped on push so a long-lived session cannot grow RSS without
/// bound. The visible transcript renders from `main_panel`, and `messages` is a
/// parallel log read only via its length, so trimming it is invisible to users.
const MESSAGE_HISTORY_CAP: usize = 10_000;

/// Upper bound on retained [`AppState::tool_details`] entries. Each entry holds
/// a tool call's full argument JSON, so an uncapped log grows RSS forever in a
/// long-lived session. Older entries are dropped on push. Entries store absolute
/// `main_panel` line indices (not positions within the Vec) and are looked up by
/// value, so dropping the oldest keeps every retained lookup correct — only tool
/// cards scrolled far past the cap lose their right-click expansion.
const TOOL_DETAILS_CAP: usize = 10_000;

/// Pushes `item` onto `buf`, then drops the oldest entries so its length never
/// exceeds `cap`. Returns how many front entries were dropped, so a caller
/// holding absolute indices into `buf` can shift them accordingly.
fn push_capped<T>(buf: &mut Vec<T>, item: T, cap: usize) -> usize {
    buf.push(item);
    let overflow = buf.len().saturating_sub(cap);
    if overflow > 0 {
        buf.drain(..overflow);
    }
    overflow
}

const METRICS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Window covered by the metrics fetch: the last 24h, in microseconds.
const METRICS_SINCE_WINDOW_MICROS: i64 = 24 * 3_600 * 1_000_000;

/// Builds an `LspSnapshot` from `lsp.status` and `lsp.diagnostics` RPC responses.
///
/// State field: `"starting"` | `"ready"` | `"degraded: <reason>"` (daemon format).
/// Severity field: `"error"` | `"warning"` | `"info"` | `"hint"` (daemon format).
#[allow(clippy::cast_precision_loss)]
pub(crate) fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn lsp_snapshot_from_rpc(
    status_resp: &serde_json::Value,
    diag_resp: &serde_json::Value,
) -> smedja_lsp::LspSnapshot {
    let servers = status_resp["servers"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let name = s["name"].as_str()?.to_owned();
                    let state_str = s["state"].as_str().unwrap_or("starting");
                    let state = if state_str == "ready" {
                        smedja_lsp::ServerState::Ready
                    } else if let Some(reason) = state_str.strip_prefix("degraded: ") {
                        smedja_lsp::ServerState::Degraded(reason.to_owned())
                    } else {
                        smedja_lsp::ServerState::Starting
                    };
                    Some(smedja_lsp::ServerStatus { name, state })
                })
                .collect()
        })
        .unwrap_or_default();

    let diagnostics = diag_resp["diagnostics"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    let file = std::path::PathBuf::from(d["file"].as_str()?);
                    let line = u32::try_from(d["line"].as_u64().unwrap_or(1)).unwrap_or(u32::MAX);
                    let col = u32::try_from(d["col"].as_u64().unwrap_or(1)).unwrap_or(u32::MAX);
                    let severity = match d["severity"].as_str().unwrap_or("error") {
                        "warning" => smedja_lsp::Severity::Warning,
                        "info" => smedja_lsp::Severity::Info,
                        "hint" => smedja_lsp::Severity::Hint,
                        _ => smedja_lsp::Severity::Error,
                    };
                    let code = d["code"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    let message = d["message"].as_str()?.to_owned();
                    Some(smedja_lsp::Diagnostic {
                        file,
                        line,
                        col,
                        severity,
                        code,
                        message,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    smedja_lsp::LspSnapshot {
        servers,
        diagnostics,
    }
}

/// Folds a `metrics.summary` response into one [`metrics_view::MetricsRow`] per
/// runner, in first-seen runner order.
///
/// Reads `resp["buckets"]`, summing `input_tok + output_tok` into `tokens` and
/// accumulating `cost_usd` and `error_count`. An hourly 24h window can return
/// several buckets per runner, which collapse to a single row. Missing or
/// non-array `buckets`, and missing per-bucket fields, are treated as
/// empty / zero — never a panic — so a malformed response yields an empty `Vec`.
#[must_use]
fn metrics_rows_from_summary(resp: &serde_json::Value) -> Vec<metrics_view::MetricsRow> {
    let Some(buckets) = resp["buckets"].as_array() else {
        return Vec::new();
    };
    let mut rows: Vec<metrics_view::MetricsRow> = Vec::new();
    for bucket in buckets {
        let runner = bucket["runner"].as_str().unwrap_or("-");
        let tokens =
            bucket["input_tok"].as_i64().unwrap_or(0) + bucket["output_tok"].as_i64().unwrap_or(0);
        let cost_usd = bucket["cost_usd"].as_f64().unwrap_or(0.0);
        let errors = bucket["error_count"].as_i64().unwrap_or(0);
        if let Some(row) = rows.iter_mut().find(|r| r.runner == runner) {
            row.tokens += tokens;
            row.cost_usd += cost_usd;
            row.errors += errors;
        } else {
            rows.push(metrics_view::MetricsRow {
                runner: runner.to_owned(),
                tokens,
                cost_usd,
                errors,
            });
        }
    }
    rows
}

/// Folds a `metrics.summary` response into one [`metrics_view::TierRow`] per
/// model, in first-seen order.
///
/// Reads `resp["buckets"]`, summing `input_tok + output_tok` into `tokens` and
/// accumulating `cost_usd` per model. Error-only rows (empty `model`, from the
/// audit log) are skipped — they have no tier to attribute to. Missing or
/// non-array `buckets`, and missing per-bucket fields, are treated as empty /
/// zero, so a malformed response yields an empty `Vec` rather than a panic.
#[must_use]
fn tier_rows_from_summary(resp: &serde_json::Value) -> Vec<metrics_view::TierRow> {
    let Some(buckets) = resp["buckets"].as_array() else {
        return Vec::new();
    };
    let mut rows: Vec<metrics_view::TierRow> = Vec::new();
    for bucket in buckets {
        let model = bucket["model"].as_str().unwrap_or("");
        if model.is_empty() {
            continue; // error-only rows carry no model / tier
        }
        let tokens =
            bucket["input_tok"].as_i64().unwrap_or(0) + bucket["output_tok"].as_i64().unwrap_or(0);
        let cost_usd = bucket["cost_usd"].as_f64().unwrap_or(0.0);
        if let Some(row) = rows.iter_mut().find(|r| r.model == model) {
            row.tokens += tokens;
            row.cost_usd += cost_usd;
        } else {
            rows.push(metrics_view::TierRow {
                model: model.to_owned(),
                tokens,
                cost_usd,
            });
        }
    }
    rows
}

/// Returns whether the metrics panel poll is due: true only when the panel is
/// `visible` and `last` is unset or [`METRICS_POLL_INTERVAL`] has elapsed by
/// `now`. The panel is never polled while hidden.
#[must_use]
fn metrics_poll_due(
    visible: bool,
    last: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    visible && last.is_none_or(|t| now.saturating_duration_since(t) >= METRICS_POLL_INTERVAL)
}

/// Toggles the metrics panel visibility. When the toggle makes the panel
/// visible, clears `last_metrics_poll` so the next event-loop tick fetches
/// immediately rather than waiting a full interval for the first paint.
fn toggle_metrics_view(state: &mut AppState) {
    state.panels.metrics = !state.panels.metrics;
    if state.panels.metrics {
        state.last_metrics_poll = None;
    }
}

// ---------------------------------------------------------------------------
// Tests (L128, L129)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
