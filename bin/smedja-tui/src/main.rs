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

mod events;
mod input;
mod render;
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
    detect_project_types, format_gov_list, gov_create, gov_transition, scan_gov_artifacts,
    GovArtifact,
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
    // Author chip + message body. Reset the assistant chip latch so the next
    // response emits its own "▌ <runner>" boundary on a fresh line.
    let you_accent = palette().accent;
    push_author_chip(&mut state.main_panel, "you", you_accent, state.no_color);
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

/// Builds an author chip line (`▌ you` / `▌ claude`) marking a turn boundary so
/// messages have clear authorship. Pushed on its own line; the message body
/// follows beneath it.
/// Pushes an author chip, preceded by a blank spacer line (a turn separator)
/// when the panel already has content — so successive turns read as distinct
/// blocks instead of one running mass of text.
fn push_author_chip(panel: &mut main_panel::MainPanel, label: &str, color: Color, no_color: bool) {
    if !panel.is_empty() {
        panel.push_styled_line(Line::from(""));
    }
    panel.push_styled_line(author_chip(label, color, no_color));
}

fn author_chip(label: &str, color: Color, no_color: bool) -> Line<'static> {
    let style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    Line::from(Span::styled(format!("▌ {label}"), style))
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
#[allow(clippy::too_many_lines)] // event loop + render + poll in a single binary entry point
async fn main() -> Result<()> {
    init_tracing();
    crate::theme::init_palette(load_tui_colors().as_ref());

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

    let mut client = Client::connect(&sock).await.unwrap_or_else(|_| {
        eprintln!("smedja: cannot connect to smdjad at {}", sock.display());
        eprintln!();
        eprintln!("If smdjad is not running, start it:");
        eprintln!("  systemctl --user start smdjad");
        eprintln!("  # or run directly: smdjad &");
        eprintln!();
        eprintln!("If you haven't set up a provider yet:");
        eprintln!("  export ANTHROPIC_API_KEY=<your-key>");
        eprintln!("  smdjad &");
        std::process::exit(1);
    });

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
        quit_armed: false,
        permission_mode: "ask".to_owned(),
        graph_workspace: None,
        graph_symbols: None,
        tool_details: Vec::new(),
        pending_tool: None,
        secret_var: None,
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
        diff_split_view: false,
        staging_queue: staging::StagingQueue::new(),
        panels: PanelVisibility {
            context_rail: true,
            metrics: true,
            session_rail: true,
            lsp: true,
            obs: true,
            role_cockpit: true,
            quality: true,
            value: true,
            fleet: false,
        },
        metrics_snapshot: Vec::new(),
        savings_snapshot: metrics_view::SavingsSnapshot::default(),
        last_metrics_poll: None,
        last_obs_poll: None,
        context_used: 0,
        context_window: 200_000,
        main_panel: main_panel::MainPanel::new(),
        action_log: action_log::ActionLog::new(50),
        slash_completions: Vec::new(),
        slash_popup_visible: false,
        slash_cursor: 0,
        runner_picker_mode: false,
        session_picker_mode: false,
        command_palette_mode: false,
        file_picker_open: false,
        file_picker_dir: std::path::PathBuf::new(),
        file_picker_entries: Vec::new(),
        file_picker_cursor: 0,
        session_picker_ids: Vec::new(),
        session_rail_items: Vec::new(),
        session_rail_cursor: 0,
        last_session_rail_poll: None,
        session_detail_overlay: None,
        turn_in_flight: false,
        assistant_open: false,
        poll_retry_count: 0,
        scroll_focus: false,
        selection_mode: false,
        selection_anchor: (0, 0),
        selection_end: (0, 0),
        g_pending: false,
        input_cursor: 0,
        pending_cowork: Vec::new(),
        cowork_modify_mode: false,
        cowork_modify_input: String::new(),
        last_graph_poll: None,
        stream_rx: None,
        upgrade_rx: None,
        current_thinking: String::new(),
        thinking_steps: Vec::new(),
        thinking_expanded: false,
        kill_ring: VecDeque::new(),
        active_agent_name: None,
        stream_sock_path,
        last_traceparent: None,
        pending_output_type: None,
        otlp_configured,
        no_color: std::env::var("NO_COLOR").is_ok()
            || std::env::var("TERM").ok().as_deref() == Some("dumb"),
        spinner_tick: 0,
        panel_search_mode: false,
        panel_search_query: String::new(),
        display_start_idx: 0,
        prompt_history: Vec::new(),
        history_idx: None,
        saved_input: String::new(),
        history_search_mode: false,
        history_search_query: String::new(),
        openspec_bin: which::which("openspec").ok(),
        lsp_last_poll: None,
        lsp_snapshot: smedja_lsp::LspSnapshot::default(),
        obs_snapshot: obs_panel::ObsSnapshot::default(),
        quality_snapshot: quality_panel::QualitySnapshot::default(),
        plan_steps: Vec::new(),
        consecutive_low_quality: 0,
        quality_review_in_progress: false,
        ctrl_q_pressed_at: None,
        value_snapshot: value_panel::ValueSnapshot::default(),
        latency_samples: VecDeque::new(),
        session_tokens_in: 0,
        session_tokens_out: 0,
        quality_score_sum: 0,
        quality_score_count: 0,
        show_session_peek: false,
        render_mode: viz::detect_render_mode(),
        current_trace: trace_waterfall::TurnTrace::default(),
        trace_selected: 0,
        trace_expanded: false,
        fleet: fleet_panel::FleetState::default(),
        live_tokens: 0,
        last_stream_activity: None,
        tool_started_at: None,
        plan_current: 0,
        prev_model: None,
        prev_tier: None,
        prev_context_used: 0,
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

    let _guard = TerminalGuard; // instantiate immediately so Drop restores terminal on any panic
    enable_raw_mode().context("enable raw mode")?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .context("enter alternate screen")?;
    // Negotiate the kitty keyboard protocol so the host terminal emits CSI-u
    // sequences: this is what lets us distinguish Shift+Enter from Enter and see
    // Ctrl-modified keys reliably. Best-effort — terminals that don't support it
    // ignore the push (and the TerminalGuard pops it on exit).
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    // Watch for SIGTERM so we can clean up the terminal state before exiting.
    let (sigterm_tx, sigterm_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
        let _ = sigterm_tx.send(true);
    });

    // Periodically force a full repaint so any cell that desynced between
    // ratatui's diff and the host terminal grid (observed as stale content
    // lingering in the top rows) is rewritten. The repaint is atomic thanks to
    // the synchronized-output bracket below, so it does not flicker.
    let mut last_full_repaint = std::time::Instant::now();
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
                    Event::Mouse(mouse_ev) => match mouse_ev.kind {
                        MouseEventKind::ScrollDown => {
                            state.main_panel.scroll_down();
                        }
                        MouseEventKind::ScrollUp => {
                            state.main_panel.scroll_up();
                        }
                        // Left press inside the messages panel starts a
                        // character-precise drag selection at the clicked column.
                        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                            if let Some(pos) =
                                state.main_panel.pos_at(mouse_ev.column, mouse_ev.row)
                            {
                                // Selection renders from `selection_mode` alone —
                                // do NOT switch into scroll/keyboard mode, so the
                                // user can keep typing while/after marking.
                                state.selection_mode = true;
                                state.selection_anchor = pos;
                                state.selection_end = pos;
                            }
                        }
                        // Right-click on a tool card → expand its full args in an
                        // overlay (left-drag still selects; no conflict).
                        MouseEventKind::Down(crossterm::event::MouseButton::Right) => {
                            if let Some((line, _)) =
                                state.main_panel.pos_at(mouse_ev.column, mouse_ev.row)
                            {
                                if let Some((_, name, full)) = state
                                    .tool_details
                                    .iter()
                                    .find(|(l, _, _)| *l == line)
                                    .cloned()
                                {
                                    state.diff_overlay =
                                        Some((0, format_tool_detail(&name, &full)));
                                    state.diff_scroll = 0;
                                }
                            }
                        }
                        // Dragging extends the selection to the cell under cursor.
                        // Dragging past the top/bottom edge auto-scrolls so a
                        // selection can run beyond the visible region.
                        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                            if state.selection_mode {
                                if state.main_panel.row_above(mouse_ev.row) {
                                    state.main_panel.scroll_up();
                                } else if state.main_panel.row_below(mouse_ev.row) {
                                    state.main_panel.scroll_down();
                                }
                                if let Some(pos) = state
                                    .main_panel
                                    .pos_at_clamped(mouse_ev.column, mouse_ev.row)
                                {
                                    state.selection_end = pos;
                                }
                            }
                        }
                        // Release copies the marked text (only if an actual range
                        // was dragged — a bare click just places the anchor and is
                        // dismissed without clobbering the clipboard).
                        MouseEventKind::Up(crossterm::event::MouseButton::Left)
                            if state.selection_mode =>
                        {
                            if state.selection_anchor == state.selection_end {
                                state.selection_mode = false;
                            } else {
                                let text = state
                                    .main_panel
                                    .selection_text(state.selection_anchor, state.selection_end);
                                let count = text.lines().count().max(1);
                                let msg = match yank_to_clipboard(std::slice::from_ref(&text)) {
                                    Ok(_) => {
                                        format!("\u{2713} {count} lines copied to clipboard")
                                    }
                                    Err(e) => e,
                                };
                                state.clipboard = Some(text);
                                state.selection_mode = false;
                                push_system_message(&mut state, msg);
                            }
                        }
                        _ => {}
                    },
                    Event::Paste(text) => {
                        // Insert the whole paste as a single edit at the cursor.
                        // Because we don't process it key-by-key, embedded
                        // newlines stay literal (no accidental submit) — pasting
                        // a multi-line URL/snippet just lands in the input.
                        let cur = state.input_cursor.min(state.input.len());
                        state.input.insert_str(cur, &text);
                        state.input_cursor = cur + text.len();
                        let completions = filtered_completions(&state.input);
                        state.slash_completions = completions;
                    }
                    Event::Resize(_, _) => {
                        // Clamp scroll after resize so we don't end up past the
                        // last available line.
                        state.main_panel.clamp_scroll();
                    }
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

        // Bracket the frame in a synchronized update (`?2026h`/`?2026l`) so the
        // host terminal only presents complete frames. Without this, rapid
        // streaming redraws are parsed and rendered half-applied, tearing the
        // message area into overlapping garbage.
        let _ = execute!(stdout(), crossterm::terminal::BeginSynchronizedUpdate);
        // Force a full redraw a few times a second to self-heal any diff/grid
        // desync (clear() erases the screen + discards ratatui's prev-buffer so
        // the next draw emits every cell). Kept inside the synchronized-update
        // bracket so the erase+redraw is presented atomically — no flicker.
        if last_full_repaint.elapsed() >= Duration::from_millis(500) {
            let _ = terminal.clear();
            last_full_repaint = std::time::Instant::now();
        }
        terminal.draw(|f| render(f, &mut state))?;
        let _ = execute!(stdout(), crossterm::terminal::EndSynchronizedUpdate);

        // Drain NDJSON stream events from the background reader task.
        // When streaming is active (stream_rx is Some), render deltas in real
        // time and finalise the turn on the terminal event.  When streaming is
        // not available, fall back to the turn.subscribe blocking poll.
        let mut pending_output_save: Option<(OutputType, String)> = None;
        if state.stream_rx.is_some() {
            let mut turn_done = false;
            let mut stream_disconnected = false;
            loop {
                let event = match state
                    .stream_rx
                    .as_mut()
                    .expect("stream_rx present")
                    .try_recv()
                {
                    Ok(ev) => ev,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        if !turn_done {
                            stream_disconnected = true;
                        }
                        break;
                    }
                };
                if apply_stream_event(&mut state, event, &mut pending_output_save) {
                    turn_done = true;
                }
            }

            if stream_disconnected {
                // Sender dropped without a terminal event — daemon socket closed
                // unexpectedly. Surface a recoverable error rather than spinning forever.
                state.main_panel.push_line(
                    "[STREAM] daemon closed unexpectedly — ↑ to recall and retry".to_owned(),
                );
                if let Some(mut block) = state.current_block.take() {
                    block.fail();
                    state.block_store.push(block);
                }
                turn_done = true;
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
                    state.obs_snapshot.context_used = state.context_used;
                    state.obs_snapshot.context_window = state.context_window;
                }
                // Surface model/tier/compaction transitions as transcript dividers.
                check_transitions(&mut state);
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
                            let (label, hint) = classify_turn_error(error);
                            let header = format_turn_error(&state.runner, label, error);
                            let display = if hint.is_empty() {
                                header
                            } else {
                                format!("{header}\n  \u{2192} {hint}")
                            };
                            state.main_panel.push_line(display.clone());
                            push_system_message(&mut state, display);
                            if let Some(mut block) = state.current_block.take() {
                                block.fail();
                                state.block_store.push(block);
                            }
                        } else {
                            let response = v["response"].as_str().unwrap_or("").to_owned();
                            let input_tok = v["input_tok"].as_i64().unwrap_or(0);
                            let output_tok = v["output_tok"].as_i64().unwrap_or(0);
                            let turn_ms = state.turn_submitted_at.map_or(0, |t| {
                                u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                            });
                            state.turn_submitted_at = None;

                            if let Some(mut block) = state.current_block.take() {
                                block.push_text(&response);
                                block.complete(turn_ms);
                                for line in block.render_lines(80) {
                                    state.main_panel.push_line(line.clone());
                                    state.push_message(Message {
                                        role: Role::System,
                                        text: line,
                                    });
                                }
                                state.block_store.push(block);
                            } else {
                                if !response.is_empty() {
                                    state.main_panel.push_delta(&response);
                                }
                                state.push_message(Message {
                                    role: Role::System,
                                    text: response,
                                });
                            }

                            let footer =
                                format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms");
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
                            state.obs_snapshot.context_used = state.context_used;
                            state.obs_snapshot.context_window = state.context_window;
                        }
                    }
                    Ok(_) => {
                        state.poll_retry_count += 1;
                        // Exponential backoff on non-terminal returns: 100 ms → 200 ms → … → 1 s.
                        let shift = state.poll_retry_count.saturating_sub(1).min(10);
                        let backoff_ms = (100u64 << shift).min(1_000);
                        #[allow(clippy::unchecked_time_subtraction)]
                        let poll_base =
                            std::time::Instant::now() - std::time::Duration::from_millis(50);
                        state.last_poll =
                            Some(poll_base + std::time::Duration::from_millis(backoff_ms));
                        if state.poll_retry_count % 5 == 1 {
                            let attempt = state.poll_retry_count;
                            push_action_log(
                                &mut state,
                                format!("waiting for turn… (poll attempt {attempt})"),
                            );
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
                        // On transport-level disconnects attempt a reconnect before
                        // giving up, so a daemon restart does not require a TUI restart.
                        if e.code == smedja_rpc::codes::SERVER_DISCONNECTED {
                            state.main_panel.push_line(
                                "daemon disconnected — attempting reconnect…".to_owned(),
                            );
                            match try_reconnect(&sock).await {
                                Some(new_client) => {
                                    client = new_client;
                                    state
                                        .main_panel
                                        .push_line("reconnected to daemon".to_owned());
                                }
                                None => {
                                    state.main_panel.push_line(
                                        "reconnect failed — restart smedja-tui when the daemon is back".to_owned(),
                                    );
                                }
                            }
                        } else {
                            state.main_panel.push_line(text.clone());
                            state.push_message(Message {
                                role: Role::System,
                                text,
                            });
                        }
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

        // Poll background upgrade result.
        let upgrade_done: Option<String> = if let Some(ref mut rx) = state.upgrade_rx {
            rx.try_recv().ok()
        } else {
            None
        };
        if let Some(msg) = upgrade_done {
            push_system_message(&mut state, msg);
            state.upgrade_rx = None;
        }

        // Graph status poll: reflect the real indexed symbol count for the
        // workspace every 5 s, so the right-bar shows "N symbols" after an index
        // built outside this session (e.g. `smj workspace index`) instead of
        // always "graph: /index to build".
        let should_poll_graph = state
            .last_graph_poll
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(5));
        if should_poll_graph {
            state.last_graph_poll = Some(std::time::Instant::now());
            let ws = state.graph_workspace.clone().or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
            });
            if let Some(ws) = ws {
                if let Ok(v) = client
                    .call("graph.status", json!({ "workspace": ws }))
                    .await
                {
                    if v.get("exists").and_then(Value::as_bool).unwrap_or(false) {
                        if let Some(n) = v.get("indexed").and_then(Value::as_u64) {
                            state.graph_symbols = usize::try_from(n).ok();
                        }
                    }
                }
            }
        }

        // Session rail poll: refresh the session list every 5 s when visible.
        let should_poll_sessions = state.panels.session_rail
            && state
                .last_session_rail_poll
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(5));
        if should_poll_sessions {
            state.last_session_rail_poll = Some(std::time::Instant::now());
            if let Ok(Value::Array(sessions)) = client.call("session.list", json!({})).await {
                state.session_rail_items = sessions
                    .iter()
                    .filter_map(|v| {
                        let id = v["id"].as_str()?.to_owned();
                        let runner = v["runner"].as_str().unwrap_or("?");
                        let label = format!("{runner}  {}", &id[..id.len().min(12)]);
                        Some((id, label))
                    })
                    .collect();
                // On first load (cursor still at 0) point at the current session.
                // On subsequent polls clamp to the new list length.
                let current_idx = state
                    .session_rail_items
                    .iter()
                    .position(|(id, _)| id == &state.session_id);
                if let Some(idx) = current_idx {
                    state.session_rail_cursor = idx;
                } else if !state.session_rail_items.is_empty() {
                    state.session_rail_cursor = state
                        .session_rail_cursor
                        .min(state.session_rail_items.len().saturating_sub(1));
                }
            }
        }

        // Metrics panel poll: a single slow (~3 s) cadence drives BOTH the
        // per-runner `metrics.summary` fetch (cost/usage rows) and the
        // token-economy `savings.summary` fetch (savings/efficiency section).
        // Gated on the panel being visible; never fetched while hidden. The
        // fetch only mutates the cached snapshots — it never blocks the render,
        // mirroring the cowork poll's tolerant `if let Ok(...)` handling.
        if metrics_poll_due(
            state.panels.metrics,
            state.last_metrics_poll,
            std::time::Instant::now(),
        ) {
            state.last_metrics_poll = Some(std::time::Instant::now());
            let now_micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0i64, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));

            // Per-runner rollups: hourly tier over the last 24h.
            let metrics_since = now_micros.saturating_sub(METRICS_SINCE_WINDOW_MICROS);
            if let Ok(resp) = client
                .call(
                    "metrics.summary",
                    json!({ "tier": "hourly", "since": metrics_since }),
                )
                .await
            {
                state.metrics_snapshot = metrics_rows_from_summary(&resp);
                // Extract 24h token total for the daily quota bar (buckets array).
                if let Some(buckets) = resp["buckets"].as_array() {
                    let total_24h: u64 = buckets
                        .iter()
                        .map(|b| {
                            let i = b["input_tok"].as_u64().unwrap_or(0);
                            let o = b["output_tok"].as_u64().unwrap_or(0);
                            i.saturating_add(o)
                        })
                        .sum();
                    if total_24h > 0 {
                        state.obs_snapshot.daily_tokens_used = Some(total_24h);
                    }
                }
            }

            // Token-economy savings: daily tier over the last 7 days, matching
            // `smj savings` defaults.
            let savings_since = now_micros.saturating_sub(7 * 86_400 * 1_000_000);
            if let Ok(resp) = client
                .call(
                    "savings.summary",
                    json!({ "tier": "daily", "since": savings_since }),
                )
                .await
            {
                state.savings_snapshot = metrics_view::savings_snapshot_from_json(&resp);
                // Mirror savings data into obs_snapshot.
                state.obs_snapshot.efficiency_ratio = state.savings_snapshot.efficiency_ratio;
                state.obs_snapshot.cache_saved = state.savings_snapshot.cache_saved;
            }
        }

        // Independent obs-panel poll (session.cost) — runs even when the metrics
        // overlay is closed so the obs rail always shows current cost.
        #[allow(clippy::items_after_statements)]
        const OBS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
        let obs_due = state
            .last_obs_poll
            .is_none_or(|t| t.elapsed() >= OBS_POLL_INTERVAL);
        if obs_due {
            state.last_obs_poll = Some(std::time::Instant::now());
            if let Ok(cost_resp) = client
                .call("session.cost", json!({ "session_id": &state.session_id }))
                .await
            {
                if let Some(usd) = cost_resp["cost_usd"].as_f64() {
                    state.obs_snapshot.session_cost_usd = usd;
                }
            }
            // Fetch daily token limit from daemon (reads SMEDJA_DAILY_TOKEN_LIMIT).
            if let Ok(quota_resp) = client.call("quota.limit", serde_json::Value::Null).await {
                state.obs_snapshot.daily_tokens_limit = quota_resp["daily_tokens"].as_u64();
            }
            // Refresh value panel with active-change token cost.
            if let Ok(vc) = client
                .call("cost.active_change", serde_json::Value::Null)
                .await
            {
                state.value_snapshot.change_name = vc["change_name"].as_str().map(str::to_owned);
                let token_cost = vc["token_cost"].as_u64().unwrap_or(0);
                state.value_snapshot.token_cost = token_cost;
                // Session blended $/token rate applied to this change's tokens.
                let total_tok = state
                    .session_tokens_in
                    .saturating_add(state.session_tokens_out);
                state.value_snapshot.cost_usd_micros = value_panel::blended_cost_micros(
                    state.obs_snapshot.session_cost_usd,
                    total_tok,
                    token_cost,
                );
                // Real running average of observed Tier-1 quality scores.
                let quality_avg = state
                    .quality_score_sum
                    .checked_div(state.quality_score_count)
                    .map_or(0u8, |avg| {
                        #[allow(clippy::cast_possible_truncation)] // avg of 0–100 fits u8
                        let a = avg as u8;
                        a
                    });
                state.value_snapshot.quality_avg = quality_avg;
                state.value_snapshot.estimated_value = value_panel::estimate_value(quality_avg);
            }
        }

        // Poll smdjad for LSP state every 5 s (single canonical source).
        #[allow(clippy::items_after_statements)]
        const LSP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
        let lsp_due = state
            .lsp_last_poll
            .is_none_or(|t| t.elapsed() >= LSP_POLL_INTERVAL);
        if lsp_due {
            state.lsp_last_poll = Some(std::time::Instant::now());
            if let (Ok(status_resp), Ok(diag_resp)) = (
                client.call("lsp.status", serde_json::Value::Null).await,
                client
                    .call("lsp.diagnostics", serde_json::Value::Null)
                    .await,
            ) {
                state.lsp_snapshot = lsp_snapshot_from_rpc(&status_resp, &diag_resp);
            }
        }

        if state.quit || *sigterm_rx.borrow() {
            break;
        }
    }

    // L127: persist history on clean shutdown.
    let _ = editor.save_history(&history_path);

    Ok(())
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
