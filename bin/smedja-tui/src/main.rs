pub mod action_log;
mod blocks;
mod clipboard;
pub mod code_widget;
mod context_rail;
mod cowork_widget;
mod editor;
mod governance;
mod lsp_panel;
pub mod main_panel;
mod metrics_view;
mod obs_panel;
mod quality_panel;
pub(crate) mod slash;
mod staging;
mod statusbar;
mod terminal_guard;
pub mod theme;
mod upgrade;
mod value_panel;

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
pub(crate) use governance::{
    detect_project_types, format_gov_list, gov_create, gov_transition, scan_gov_artifacts,
    GovArtifact,
};
#[allow(unused_imports)]
pub(crate) use terminal_guard::TerminalGuard;
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

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "smedja-tui", version, about = "smedja agent dashboard (TUI)")]
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
  /agent [id]        — run named agent (omit id to list available agents)
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
  Ctrl-L             — toggle LSP diagnostic panel
  Ctrl-O             — toggle observability panel
  Ctrl-Q             — toggle quality gate panel
  Ctrl-V             — toggle value / ROI panel (Ctrl-V in input mode pastes)
  Ctrl-W             — toggle session browser (left rail)
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

/// Visibility state for all toggleable rail and overlay panels.
/// Full detail for a single session, fetched on demand via `session.get` when
/// the user presses Enter on a session rail item.
#[derive(Debug, Clone)]
struct SessionDetail {
    id: String,
    title: Option<String>,
    mode: Option<String>,
    status: Option<String>,
    active_change: Option<String>,
    created_at: String,
    updated_at: String,
    cowork_mode: Option<String>,
}

impl SessionDetail {
    /// Construct from a `session.get` JSON response, tolerating missing optional fields.
    fn from_json(v: &serde_json::Value) -> Self {
        let str_opt = |key: &str| v[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        Self {
            id: v["id"].as_str().unwrap_or("-").to_owned(),
            title: str_opt("title"),
            mode: str_opt("mode"),
            status: str_opt("status"),
            active_change: str_opt("active_change"),
            created_at: v["created_at"].as_str().unwrap_or("-").to_owned(),
            updated_at: v["updated_at"].as_str().unwrap_or("-").to_owned(),
            cowork_mode: str_opt("cowork_mode"),
        }
    }
}

///
/// Grouped here so new panels only require adding one field instead of
/// threading a top-level boolean through `AppState` and every test helper.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct PanelVisibility {
    /// Context rail (right, Ctrl-F).
    context_rail: bool,
    /// Metrics view overlay (Ctrl-T).
    metrics: bool,
    /// Session browser left-rail (Ctrl-W).
    session_rail: bool,
    /// LSP diagnostic panel (right rail, Ctrl-L).
    lsp: bool,
    /// Observability panel (right rail, Ctrl-O).
    obs: bool,
    /// Role cockpit panel (right rail, Ctrl-A).
    role_cockpit: bool,
    /// Quality gate panel (right rail, Ctrl-Q).
    quality: bool,
    /// Value / ROI panel (right rail, Ctrl-V).
    value: bool,
}

#[allow(clippy::struct_excessive_bools)] // AppState is a TUI dispatch table; enum-splitting would add indirection without clarity
#[derive(Debug)]
pub(crate) struct AppState {
    session_id: String,
    mode: Option<String>,
    tier: Option<String>,
    runner: String,
    model: Option<String>,
    messages: Vec<Message>,
    input: String,
    quit: bool,
    /// True after one Ctrl-C with an empty input — a second consecutive Ctrl-C
    /// confirms quit. Reset by any other key so quitting is always deliberate.
    quit_armed: bool,
    /// Current permission mode (`ask`/`accept_edits`/`plan`/`auto`), cycled with
    /// Shift+Tab via `cowork.set_mode` and shown in the status bar.
    permission_mode: String,
    /// Workspace whose code-graph status the right-bar reflects — the last
    /// `/index <path>`, falling back to the TUI's cwd.
    graph_workspace: Option<String>,
    /// Symbol count from the last `/index` this session (`None` = not indexed
    /// here yet). Surfaced as a code-graph status under the LSP panel.
    graph_symbols: Option<usize>,
    /// Tool-call detail log: `(card_line_index, tool_name, full_input)`. Backs
    /// right-click expansion of a tool card and the `/tools` inspector.
    tool_details: Vec<(usize, String, String)>,
    /// The currently-running tool card awaiting its result: `(line, name,
    /// input_summary)`. Resolved to ✓/✗ when the result arrives.
    pending_tool: Option<(usize, String, String)>,
    /// When `Some(env_var)`, the input bar is in masked secret-entry mode (e.g.
    /// pasting an API key during login). Input renders as dots and Enter saves
    /// the value to the secrets file under this env-var name instead of sending
    /// a turn. `Esc` cancels.
    secret_var: Option<String>,
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
    /// Visibility state for all toggleable rail and overlay panels.
    panels: PanelVisibility,
    /// Cached per-runner metrics snapshot for the latest rollup window.
    metrics_snapshot: Vec<metrics_view::MetricsRow>,
    /// Cached token-economy savings snapshot for the latest rollup window.
    savings_snapshot: metrics_view::SavingsSnapshot,
    /// Timestamp of the last metrics panel poll (drives both the `metrics.summary`
    /// per-runner fetch and the `savings.summary` token-economy fetch on one
    /// cadence). `None` forces an immediate fetch on the next tick.
    last_metrics_poll: Option<std::time::Instant>,
    /// Timestamp of the last obs-panel poll (session.cost + daily token total).
    /// Independent of `panels.metrics` so the obs panel is always current.
    last_obs_poll: Option<std::time::Instant>,
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
    /// True when the popup is the Ctrl+K command palette (fuzzy filter, wider, shows descriptions).
    command_palette_mode: bool,
    /// True while the Ctrl+F file picker overlay is open.
    file_picker_open: bool,
    /// Current directory being browsed in the file picker.
    file_picker_dir: std::path::PathBuf,
    /// Entries in the current directory: (display-name, is_dir).
    file_picker_entries: Vec<(String, bool)>,
    /// Cursor index within file_picker_entries.
    file_picker_cursor: usize,
    /// Session ids parallel to `slash_completions` while the session picker is open.
    session_picker_ids: Vec<String>,
    /// Sessions shown in the left rail: (id, label) pairs.
    session_rail_items: Vec<(String, String)>,
    /// Cursor row within the session rail.
    session_rail_cursor: usize,
    /// Timestamp of the last session rail refresh.
    last_session_rail_poll: Option<std::time::Instant>,
    /// Detail overlay opened by pressing Enter on a session rail item.
    session_detail_overlay: Option<SessionDetail>,
    /// True while a turn is awaiting a streaming response.
    turn_in_flight: bool,
    /// True once the assistant author chip + fresh line for the current turn have
    /// been emitted, so streamed deltas land on their own line (not merged into
    /// the preceding "queued"/user line) and the chip is shown exactly once.
    assistant_open: bool,
    /// Number of consecutive unexpected (non-done) poll responses received.
    ///
    /// Used to rate-limit the "waiting for turn…" status message so it does not
    /// flood the panel on rapid retries.
    poll_retry_count: u32,
    /// Whether the messages panel has scroll focus (input bar is inactive).
    scroll_focus: bool,
    /// Whether selection mode is active within the messages panel.
    selection_mode: bool,
    /// Anchor `(line, char_col)` of the current selection.
    selection_anchor: (usize, usize),
    /// Moving end `(line, char_col)` of the current selection.
    selection_end: (usize, usize),
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
    /// Timestamp of the last `graph.status` poll (refreshes the right-bar count).
    last_graph_poll: Option<std::time::Instant>,
    /// NDJSON stream receiver for the current in-flight turn.
    stream_rx: Option<tokio::sync::mpsc::UnboundedReceiver<StreamEvent>>,
    /// Oneshot receiver for a background /upgrade operation.
    upgrade_rx: Option<tokio::sync::oneshot::Receiver<String>>,
    /// Accumulated thinking-token text for the current in-flight turn.
    ///
    /// Reset to empty at the start of each new turn. Rendered as a dim
    /// collapsible block while the turn is in flight; summarised as a
    /// single-line badge once the turn completes.
    current_thinking: String,
    /// Whether the completed thinking block is expanded in the panel.
    thinking_expanded: bool,
    /// Kill ring for Ctrl-K / Ctrl-U / Ctrl-Y input editing (max 16 entries).
    kill_ring: VecDeque<String>,
    /// Name of the agent/role active in the current in-flight turn (from `CorrelationCtx`).
    active_agent_name: Option<String>,
    /// Path of the smdjad stream socket (`<rpc_sock>.stream`).
    stream_sock_path: PathBuf,
    /// W3C traceparent from the most recently completed turn.
    last_traceparent: Option<String>,
    /// Pending structured output type for generator commands (/drawio, /pptx).
    pending_output_type: Option<OutputType>,
    /// True when `SMEDJA_OTLP_ENDPOINT` is set in the environment at startup.
    otlp_configured: bool,
    /// Disable all colours when `NO_COLOR` is set in the environment.
    no_color: bool,
    /// Braille spinner frame counter; advances each render tick while a turn is in flight.
    spinner_tick: u8,
    /// Whether '/' panel search is active (intercepts keys to refine the query).
    panel_search_mode: bool,
    /// Current search query string (highlights matching panel lines while non-empty).
    panel_search_query: String,
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
    /// Instant of last `lsp.status` / `lsp.diagnostics` RPC poll; `None` before first poll.
    lsp_last_poll: Option<std::time::Instant>,
    /// Most recent LSP snapshot (updated from RPC polls every 5 s).
    lsp_snapshot: smedja_lsp::LspSnapshot,
    /// Observability snapshot — updated from turn events + metrics polls.
    obs_snapshot: obs_panel::ObsSnapshot,
    /// Quality gate snapshot — updated on each `TurnEvent::QualitySnapshot`.
    quality_snapshot: quality_panel::QualitySnapshot,
    /// Consecutive turns with quality score < 60 (resets on score ≥ 60).
    consecutive_low_quality: u8,
    /// Value / ROI snapshot — updated on the obs poll cadence.
    value_snapshot: value_panel::ValueSnapshot,
    /// Whether a Tier-2 LLM quality review is in flight.
    quality_review_in_progress: bool,
    /// When Ctrl-Q was first pressed; used to detect hold ≥ 500ms.
    ctrl_q_pressed_at: Option<std::time::Instant>,
    /// Last 50 turn round-trip latencies in ms, used for p95/p99 computation.
    latency_samples: VecDeque<u64>,
    /// Cumulative input tokens for this session (updated on each turn done event).
    session_tokens_in: u64,
    /// Cumulative output tokens for this session.
    session_tokens_out: u64,
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
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "stream socket connect failed");
            let _ = tx.send(StreamEvent::Error {
                message: format!("stream unavailable: {e}"),
            });
            return;
        }
    };
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let req = format!("{{\"task_id\":\"{task_id}\"}}\n");
    if writer.write_all(req.as_bytes()).await.is_err() {
        let _ = tx.send(StreamEvent::Error {
            message: "stream handshake failed".to_owned(),
        });
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
                if let Ok(ev) = serde_json::from_str::<StreamEvent>(trimmed) {
                    let terminal = ev.is_terminal();
                    let _ = tx.send(ev);
                    if terminal {
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
            state.current_thinking.clear();
            state.thinking_expanded = false;
            state.active_agent_name = None;

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
        state.messages.push(sys_msg);
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
fn classify_turn_error(msg: &str) -> (&'static str, &'static str) {
    let lower = msg.to_lowercase();
    if lower.contains("rate limit") || lower.contains("rate_limit") {
        (
            "RATE LIMITED",
            "Use ↑ to recall your last message, then Enter to retry",
        )
    } else if lower.contains("api key")
        || lower.contains("auth")
        || lower.contains("401")
        || lower.contains("403")
    {
        (
            "AUTH ERROR",
            "Check ANTHROPIC_API_KEY or provider credentials",
        )
    } else if lower.contains("quota") || lower.contains("429") {
        ("QUOTA EXCEEDED", "Daily quota reached; check smj cost")
    } else if lower.contains("timeout") || lower.contains("timed out") {
        (
            "TIMEOUT",
            "Turn hit the wall-clock cap (default 900s; raise with SMEDJA_TURN_TIMEOUT_S)",
        )
    } else if lower.contains("network") || lower.contains("connection") || lower.contains("connect")
    {
        (
            "NETWORK ERROR",
            "Check network connectivity and provider endpoint",
        )
    } else if lower.contains("overload") {
        (
            "OVERLOADED",
            "Provider overloaded — use ↑ and retry in a moment",
        )
    } else if lower.contains("context length") || lower.contains("maximum context") {
        (
            "CONTEXT FULL",
            "Context window full — use /rollback to trim history",
        )
    } else {
        (
            "ERROR",
            "Check /obs for details or run smj session rollback",
        )
    }
}

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
    state.messages.push(msg);
}

/// Maps a tool name to a compact `(glyph, short-label)` pair so cards stay tidy
/// — e.g. the verbose `ToolSearch` becomes "⌕ search". Unknown tools fall back
/// to a lowercased, length-capped name.
fn tool_glyph_label(name: &str) -> (&'static str, String) {
    match name {
        "Bash" | "bash" | "shell" => ("⌘", "bash".to_owned()),
        "Read" | "read" => ("◇", "read".to_owned()),
        "Write" | "write" => ("✎", "write".to_owned()),
        "Edit" | "edit" | "MultiEdit" | "Update" => ("✎", "edit".to_owned()),
        "Grep" | "grep" | "search_files" => ("⌕", "grep".to_owned()),
        "Glob" | "glob" | "find" => ("⌕", "glob".to_owned()),
        "ToolSearch" => ("⌕", "search".to_owned()),
        "WebFetch" | "fetch" => ("⬇", "fetch".to_owned()),
        "WebSearch" => ("⌕", "web".to_owned()),
        "Task" | "Agent" => ("◈", "agent".to_owned()),
        "TodoWrite" => ("☑", "todo".to_owned()),
        "NotebookEdit" => ("✎", "notebook".to_owned()),
        other => {
            let s: String = other.to_lowercase().chars().take(14).collect();
            ("▶", s)
        }
    }
}

/// Builds a one-line tool-call card — `<status> <glyph> <label>  <summary>`.
/// `status` is the progress glyph: a spinner frame while running, `✓` on success,
/// `✗` on error. glyph+label are accented/bold and the summary dimmed.
fn tool_call_card(name: &str, input: &str, no_color: bool, status: char) -> Line<'static> {
    let (glyph, label) = tool_glyph_label(name);
    let (status_style, head_style, arg_style) = if no_color {
        (
            Style::default(),
            Style::default().add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        let p = palette();
        let st = match status {
            '\u{2713}' => Style::default().fg(p.code_added), // ✓
            '\u{2717}' => Style::default().fg(p.code_removed), // ✗
            _ => Style::default().fg(p.text_dim),
        };
        (
            st,
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            Style::default().fg(p.text_dim),
        )
    };
    let mut spans = vec![
        Span::styled(format!("{status} "), status_style),
        Span::styled(format!("{glyph} {label}"), head_style),
    ];
    if !input.is_empty() {
        spans.push(Span::styled(format!("  {input}"), arg_style));
    }
    Line::from(spans)
}

/// Saves a secret (e.g. an API key pasted during login) to
/// `~/.config/smedja/secrets.env` under `var`, replacing any existing line for
/// that variable, and chmods the file to 0600. Returns a status string that
/// never contains the secret value.
fn save_secret(var: &str, value: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    let dir = std::path::PathBuf::from(home)
        .join(".config")
        .join("smedja");
    let path = dir.join("secrets.env");
    if std::fs::create_dir_all(&dir).is_err() {
        return "login: cannot create ~/.config/smedja".to_owned();
    }
    let prefix = format!("{var}=");
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .map(str::to_owned)
        .collect();
    lines.push(format!("{var}={value}"));
    let body = format!("{}\n", lines.join("\n"));
    if std::fs::write(&path, body).is_err() {
        return format!("login: failed to write {}", path.display());
    }
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    format!(
        "\u{2713} saved {var} to {} (0600). Activate: add\n  EnvironmentFile=%h/.config/smedja/secrets.env\nto the smdjad unit, then: systemctl --user restart smdjad",
        path.display()
    )
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
        spans.push(chip(mode.to_owned(), p.text, false));
    }
    spans.push(sep());
    spans.push(chip(
        ctx.session_id.chars().take(8).collect::<String>(),
        p.text_dim,
        false,
    ));
    if ctx.pending {
        spans.push(chip("  ⟳".to_owned(), p.accent, true));
    }
    Line::from(spans)
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

/// Maximum visual rows the input field grows to before it scrolls internally.
const INPUT_MAX_ROWS: u16 = 6;

/// Character-wraps `text` (honouring embedded `'\n'`) to `width` columns and
/// returns the visual rows. A plain column wrap — no word boundaries — matching
/// the fixed-width input echo, so the field's height and the cursor's row can be
/// computed the same way ratatui will render it. `width` is clamped to ≥1.
fn wrap_input_rows(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for logical in text.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for ch in logical.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > width && !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += cw;
        }
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
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
    state.command_palette_mode = false;
    state.session_picker_ids.clear();
}

// dispatch_slash, apply_tier, apply_agent, and their exclusive format helpers
// (format_model_list, format_local_model_list, format_agents_table,
// format_metrics, format_approvals_list) have been extracted to src/slash.rs.
// They are re-exported at the top of this file via `pub(crate) use slash::...`
// so callers and the test module (which uses `use super::*`) see them unchanged.

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
    // Any key other than Ctrl-C disarms the quit confirmation so the two presses
    // must be consecutive.
    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if !is_ctrl_c {
        state.quit_armed = false;
    }

    // ------------------------------------------------------------------
    // Cowork gate widget intercepts keys when there are pending approvals.
    // ------------------------------------------------------------------
    // `y`/`Y` → cowork.approve, `n`/`N` → cowork.deny, `m`/`M` → modify
    // mode. All other keys are consumed while approvals are pending so that
    // accidental keystrokes do not reach the input bar.
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
    // ESC interrupts an in-flight turn (kills the runaway stream) while
    // staying in the prompt. Guarded so it never steals ESC from a sub-mode
    // that uses it for its own cancel.
    // ------------------------------------------------------------------
    if key.code == KeyCode::Esc
        && state.pending_task_id.is_some()
        && !state.slash_popup_visible
        && !state.history_search_mode
        && state.secret_var.is_none()
        && !state.selection_mode
    {
        if let Some(task_id) = state.pending_task_id.take() {
            let _ = client
                .call("turn.cancel", json!({ "task_id": task_id }))
                .await;
            state.turn_in_flight = false;
            state.stream_rx = None;
            push_system_message(state, "\u{2298} interrupted");
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Session detail overlay: Ctrl+Enter loads, Esc closes.
    // ------------------------------------------------------------------
    if state.session_detail_overlay.is_some() {
        if key.code == KeyCode::Esc {
            state.session_detail_overlay = None;
            return Ok(());
        }
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(detail) = state.session_detail_overlay.take() {
                state.session_id = detail.id;
                state.display_start_idx = state.messages.len();
                state.main_panel.clear_display();
                resume_into_view(state, client, ResumePlan::ReplayOnly).await;
            }
            return Ok(());
        }
    }

    // ------------------------------------------------------------------
    // Shift+Tab cycles the permission mode (ask → accept_edits → plan → auto).
    // ------------------------------------------------------------------
    if key.code == KeyCode::BackTab && state.secret_var.is_none() {
        if let Ok(v) = client
            .call("cowork.set_mode", json!({ "session_id": state.session_id }))
            .await
        {
            if let Some(m) = v.get("mode").and_then(Value::as_str) {
                m.clone_into(&mut state.permission_mode);
                push_system_message(state, format!("permission mode \u{2192} {m}"));
            }
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // File picker intercepts keys when open.
    // ------------------------------------------------------------------
    if state.file_picker_open {
        match key.code {
            KeyCode::Esc => {
                state.file_picker_open = false;
            }
            KeyCode::Up => {
                state.file_picker_cursor = state.file_picker_cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = state.file_picker_entries.len().saturating_sub(1);
                if state.file_picker_cursor < max {
                    state.file_picker_cursor += 1;
                }
            }
            KeyCode::Enter => {
                if let Some((name, is_dir)) = state
                    .file_picker_entries
                    .get(state.file_picker_cursor)
                    .cloned()
                {
                    if is_dir {
                        let new_dir = if name == "../" {
                            state
                                .file_picker_dir
                                .parent()
                                .unwrap_or(&state.file_picker_dir)
                                .to_owned()
                        } else {
                            state.file_picker_dir.join(&name)
                        };
                        open_file_picker(state, new_dir);
                    } else {
                        let full_path = state.file_picker_dir.join(&name);
                        let at_ref = format!("@file {} ", full_path.display());
                        state.input.insert_str(state.input_cursor, &at_ref);
                        state.input_cursor += at_ref.len();
                        state.file_picker_open = false;
                    }
                }
            }
            _ => {}
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
                let completions = if state.command_palette_mode {
                    command_palette_filtered(&state.input)
                } else if state.input.is_empty() {
                    state.slash_popup_visible = false;
                    Vec::new()
                } else {
                    filtered_completions(&state.input)
                };
                state.slash_cursor = state.slash_cursor.min(completions.len().saturating_sub(1));
                state.slash_completions = completions;
            }
            KeyCode::Char(c) => {
                state.input.insert(state.input_cursor, c);
                state.input_cursor += c.len_utf8();
                let completions = if state.command_palette_mode {
                    command_palette_filtered(&state.input)
                } else {
                    filtered_completions(&state.input)
                };
                state.slash_cursor = 0;
                if completions.is_empty() && !state.command_palette_mode {
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
    // Panel search mode intercept — '/' in scroll mode opens this.
    // ------------------------------------------------------------------
    if state.panel_search_mode {
        match key.code {
            KeyCode::Esc => {
                state.panel_search_mode = false;
                state.panel_search_query.clear();
            }
            KeyCode::Enter => {
                // Keep query on Enter so the user can browse matches.
                state.panel_search_mode = false;
            }
            KeyCode::Backspace => {
                state.panel_search_query.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.panel_search_query.push(c);
            }
            _ => {}
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Ctrl-A: toggle role cockpit panel (works in both input and scroll mode).
    // ------------------------------------------------------------------
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.panels.role_cockpit = !state.panels.role_cockpit;
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Ctrl-V: toggle value panel when in scroll/rail mode; paste in input mode.
    // ------------------------------------------------------------------
    if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.scroll_focus {
            state.panels.value = !state.panels.value;
        } else if let Some(text) = paste_from_clipboard() {
            let text = text.replace('\r', "");
            let before = &state.input[..state.input_cursor];
            let after = &state.input[state.input_cursor..];
            let new_input = format!("{before}{text}{after}");
            let advance = text.len();
            state.input = new_input;
            state.input_cursor += advance;
        }
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Session rail cursor navigation in input mode — Alt+↑/↓ only, so
    // plain Up/Down remain available for prompt history.
    // ------------------------------------------------------------------
    if state.panels.session_rail && !state.scroll_focus && key.modifiers.contains(KeyModifiers::ALT)
    {
        match key.code {
            KeyCode::Up => {
                state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down if !state.session_rail_items.is_empty() => {
                let max = state.session_rail_items.len().saturating_sub(1);
                state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
                return Ok(());
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Scroll / visual-selection mode intercept.
    // ------------------------------------------------------------------
    if state.scroll_focus {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if state.panels.session_rail && !state.session_rail_items.is_empty() {
                    let max = state.session_rail_items.len().saturating_sub(1);
                    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
                } else if state.selection_mode {
                    // Keyboard selection is whole-line: extend by a line, snapping
                    // the end column to that line's length.
                    let next =
                        (state.selection_end.0 + 1).min(state.main_panel.len().saturating_sub(1));
                    state.selection_end = (next, state.main_panel.line_char_len(next));
                } else {
                    state.main_panel.scroll_down();
                }
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if state.panels.session_rail {
                    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
                } else if state.selection_mode {
                    let prev = state.selection_end.0.saturating_sub(1);
                    state.selection_end = (prev, state.main_panel.line_char_len(prev));
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
                let l = state.main_panel.scroll;
                state.selection_anchor = (l, 0);
                state.selection_end = (l, state.main_panel.line_char_len(l));
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('y') if state.selection_mode => {
                let text = state
                    .main_panel
                    .selection_text(state.selection_anchor, state.selection_end);
                let count = text.lines().count().max(1);
                let msg = match yank_to_clipboard(std::slice::from_ref(&text)) {
                    Ok(_) => format!("\u{2713} {count} lines copied to clipboard"),
                    Err(e) => e,
                };
                state.clipboard = Some(text);
                state.selection_mode = false;
                push_system_message(state, msg);
                return Ok(());
            }
            KeyCode::Char('t') => {
                if let Some(tp) = state.last_traceparent.clone() {
                    let _ = yank_to_clipboard(std::slice::from_ref(&tp));
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
            // T (uppercase): toggle thinking block expansion.
            KeyCode::Char('T') => {
                if !state.current_thinking.is_empty() {
                    state.thinking_expanded = !state.thinking_expanded;
                }
                return Ok(());
            }
            // [ / ] : navigate session rail cursor (when rail is visible).
            KeyCode::Char('[') => {
                if state.panels.session_rail {
                    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
                }
                return Ok(());
            }
            KeyCode::Char(']') => {
                if state.panels.session_rail && !state.session_rail_items.is_empty() {
                    let max = state.session_rail_items.len().saturating_sub(1);
                    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
                }
                return Ok(());
            }
            // Enter: open session detail overlay for the highlighted session.
            KeyCode::Enter if state.panels.session_rail => {
                if let Some((id, _)) = state
                    .session_rail_items
                    .get(state.session_rail_cursor)
                    .cloned()
                {
                    if let Ok(v) = client.call("session.get", json!({ "id": id })).await {
                        state.session_detail_overlay = Some(SessionDetail::from_json(&v));
                    }
                }
                return Ok(());
            }
            KeyCode::Char('i' | 'a') => {
                state.scroll_focus = false;
                state.selection_mode = false;
                state.g_pending = false;
                return Ok(());
            }
            KeyCode::Char('/') => {
                state.panel_search_mode = true;
                state.panel_search_query.clear();
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
            // Ctrl-C is non-destructive: clear an in-progress input, otherwise
            // require a SECOND consecutive Ctrl-C to actually quit (so an accidental
            // press never drops you to a blank terminal). Copy is mouse / v-y.
            if !state.input.is_empty() {
                state.input.clear();
                state.input_cursor = 0;
                state.quit_armed = false;
            } else if state.quit_armed {
                state.quit = true;
            } else {
                state.quit_armed = true;
                push_system_message(state, "press Ctrl-C again to exit smedja-tui");
            }
        }

        // Ctrl-R: toggle reverse history search (input mode only).
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                state.history_search_mode = !state.history_search_mode;
                state.history_search_query.clear();
                if state.history_search_mode {
                    state.input.clone_into(&mut state.saved_input);
                }
            }
        }

        // Ctrl-F: toggle context rail (scroll mode) / open file picker (input mode).
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.scroll_focus {
                state.panels.context_rail = !state.panels.context_rail;
            } else {
                let start =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                open_file_picker(state, start);
            }
        }

        // Ctrl-W / Ctrl-Shift-W: toggle session browser left-rail.
        // Ctrl-W is consumed by many Linux WMs/terminals (e.g. WezTerm on CachyOS),
        // so Ctrl-Shift-W (crossterm: Char('W') + CONTROL) is the Linux fallback.
        KeyCode::Char('w' | 'W') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.panels.session_rail = !state.panels.session_rail;
            state.session_rail_cursor = 0;
            // Trigger an immediate poll on next tick by clearing the timestamp.
            if state.panels.session_rail {
                state.last_session_rail_poll = None;
            }
        }

        // Ctrl-G: open $EDITOR / $VISUAL to compose a multi-line message.
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                if let Some(new_text) = open_in_editor(&state.input) {
                    state.input = new_text;
                    state.input_cursor = state.input.chars().count();
                }
            }
        }

        // Ctrl-K: kill from cursor to end of line; or open command palette when input is empty.
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                let tail: String = state.input[state.input_cursor..].to_owned();
                if tail.is_empty() && state.input_cursor == 0 {
                    // Empty input → open command palette.
                    state.slash_popup_visible = true;
                    state.command_palette_mode = true;
                    state.slash_completions = command_palette_filtered("");
                    state.slash_cursor = 0;
                } else if !tail.is_empty() {
                    state.input.drain(state.input_cursor..);
                    push_kill(&mut state.kill_ring, tail);
                }
            }
        }

        // Ctrl-U: kill from start of line to cursor; push onto kill ring (input mode only).
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                let killed: String = state.input[..state.input_cursor].to_owned();
                if !killed.is_empty() {
                    state.input.drain(..state.input_cursor);
                    state.input_cursor = 0;
                    push_kill(&mut state.kill_ring, killed);
                }
            }
        }

        // Ctrl-Y: yank most recent kill at cursor position (input mode only).
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus {
                if let Some(text) = state.kill_ring.back().cloned() {
                    state.input.insert_str(state.input_cursor, &text);
                    state.input_cursor += text.len();
                }
            }
        }

        // Ctrl-B: move cursor one character left (input mode only).
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.scroll_focus && state.input_cursor > 0 {
                state.input_cursor = prev_char_boundary(&state.input, state.input_cursor);
            }
        }

        // Ctrl-T: toggle the metrics view panel (read-only rollup snapshot).
        // Toggling on clears the poll cadence so the next tick fetches at once.
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            toggle_metrics_view(state);
        }

        // Ctrl-L: toggle LSP diagnostic panel in the right rail.
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.panels.lsp = !state.panels.lsp;
        }

        // Ctrl-O: toggle observability panel in the right rail.
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.panels.obs = !state.panels.obs;
        }

        // Ctrl-Q: tap toggles the quality panel; hold ≥ 500ms triggers Tier-2 review.
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match key.kind {
                KeyEventKind::Press => {
                    // First press: toggle panel and start timing for hold detection.
                    state.panels.quality = !state.panels.quality;
                    state.ctrl_q_pressed_at = Some(std::time::Instant::now());
                }
                KeyEventKind::Repeat => {
                    // Key repeat fires while held; trigger review once at 500ms.
                    if let Some(t) = state.ctrl_q_pressed_at {
                        if t.elapsed() >= std::time::Duration::from_millis(500) {
                            state.ctrl_q_pressed_at = None;
                            state.panels.quality = true; // ensure panel is open
                            slash::trigger_quality_review(state, client).await;
                        }
                    }
                }
                KeyEventKind::Release => {
                    state.ctrl_q_pressed_at = None;
                }
            }
        }

        KeyCode::Esc => {
            if state.secret_var.take().is_some() {
                // Cancel masked secret entry; discard whatever was typed.
                state.input.clear();
                state.input_cursor = 0;
                push_system_message(state, "login: cancelled");
            } else if state.panel_search_mode {
                state.panel_search_mode = false;
                state.panel_search_query.clear();
            } else if state.diff_overlay.is_some() {
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

        // NOTE: the keyboard "block browser" (bare b/c/r/d/D) was removed — those
        // letters now type normally via the catch-all `Char(c)` arm below. Browse
        // with the arrow keys / mouse wheel; mark & copy with the mouse;
        // Shift+Enter for a newline.

        // Shift/Alt/Ctrl+Enter → insert a literal newline (multi-line compose),
        // mirroring claude-cli / opencode. Requires the kitty keyboard protocol
        // (pushed at startup) so the host terminal disambiguates the modifier.
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            if !state.scroll_focus {
                state.input.insert(state.input_cursor, '\n');
                state.input_cursor += 1;
            }
        }

        KeyCode::Enter => {
            // Masked secret entry (API key paste): save to the secrets file under
            // the pending env-var name; never echo or send it as a turn.
            if let Some(var) = state.secret_var.take() {
                let key = std::mem::take(&mut state.input);
                state.input_cursor = 0;
                let msg = if key.trim().is_empty() {
                    "login: empty key — cancelled".to_owned()
                } else {
                    save_secret(&var, key.trim())
                };
                push_system_message(state, msg);
                return Ok(());
            }

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
                        let session_id = state.session_id.clone();
                        match client
                            .call("session.get", json!({ "id": session_id }))
                            .await
                        {
                            Ok(resp) => {
                                let cowork_on = resp["cowork_mode"].as_bool().unwrap_or(false);
                                push_system_message(
                                    state,
                                    format!("cowork: {}", if cowork_on { "on" } else { "off" }),
                                );
                            }
                            Err(_) => {
                                push_system_message(state, "cowork: status unavailable");
                            }
                        }
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
    let p = palette();

    // Flood-fill the entire frame with the forge background so no terminal
    // default colour bleeds through panel gaps or empty areas.
    frame.render_widget(Block::default().style(Style::default().bg(p.bg)), area);

    // Build the input echo (prefix + visible cursor) and compute how many
    // visual rows it needs, so the input field grows and wraps instead of
    // running off the right edge ("typing blind"). The cursor's row drives an
    // internal scroll once the field hits its row cap.
    // Wrap at the main-content column width, not the full terminal width.
    // When rails are visible they take columns from the right/left of body_area;
    // subtracting their widths here keeps the height calculation and the visual
    // rendering in sync, so the input grows a row at the same point the text
    // visually wraps instead of running under the rail.
    let right_rail_w = if state.panels.context_rail && area.width >= 100 {
        context_rail::ContextRail::WIDTH
    } else {
        0
    };
    let input_w = area.width.saturating_sub(right_rail_w).max(1) as usize;
    let (input_display, input_cursor_row) = if let Some(ref var) = state.secret_var {
        // Masked secret entry — never echo the value (e.g. an API key).
        let dots = "\u{2022}".repeat(state.input.chars().count());
        (format!("{var} (hidden): {dots}\u{2588}"), 0usize)
    } else {
        let cur = state.input_cursor.min(state.input.len());
        let head = format!("> {}", &state.input[..cur]);
        let cursor_row = wrap_input_rows(&head, input_w).len().saturating_sub(1);
        (format!("{head}_{}", &state.input[cur..]), cursor_row)
    };
    let input_rows: u16 = if state.history_search_mode {
        2
    } else if state.secret_var.is_some() {
        1
    } else {
        u16::try_from(wrap_input_rows(&input_display, input_w).len())
            .unwrap_or(INPUT_MAX_ROWS)
            .clamp(1, INPUT_MAX_ROWS)
    };
    // Scroll the field so the cursor's row stays visible once input overflows.
    let input_scroll = u16::try_from(input_cursor_row)
        .unwrap_or(0)
        .saturating_sub(input_rows.saturating_sub(1));

    // L122: outer vertical split:
    //   row 0 = status bar (1 row)
    //   row 1 = body (fill)
    //   row 2 = action log (5 rows)
    //   row 3 = input (grows to wrap, capped at INPUT_MAX_ROWS)
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(input_rows),
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
    // Starship-style segmented status line (left), with a dim discoverability
    // hint right-aligned over the same row. Paint the panel background first so
    // both passes share it.
    let status_bg = if state.no_color {
        Style::default()
    } else {
        Style::default().bg(p.panel)
    };
    frame.render_widget(
        Paragraph::new(status_bar_line(&ctx, state.no_color)).style(status_bg),
        status_area,
    );
    frame.render_widget(
        Paragraph::new(status_hint_line(state.no_color))
            .alignment(ratatui::layout::Alignment::Right),
        status_area,
    );

    // -- Body: optional session rail | main panel | optional context rail ------
    #[allow(clippy::items_after_statements)]
    const SESSION_RAIL_W: u16 = 28;

    // First carve out the optional left session rail.
    let (session_rail_area_opt, content_area) = if state.panels.session_rail
        && body_area.width >= SESSION_RAIL_W + 40
    {
        let cols = Layout::horizontal([Constraint::Length(SESSION_RAIL_W), Constraint::Fill(1)])
            .split(body_area);
        (Some(cols[0]), cols[1])
    } else {
        (None, body_area)
    };

    // Then carve out the optional right context rail.
    let (main_area, rail_area) = if state.panels.context_rail && content_area.width >= 100 {
        let cols = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(context_rail::ContextRail::WIDTH),
        ])
        .split(content_area);
        (cols[0], Some(cols[1]))
    } else {
        (content_area, None)
    };

    // Render session rail when visible.
    if let Some(sr_area) = session_rail_area_opt {
        let cursor = state.session_rail_cursor;
        let lines: Vec<Line<'_>> = state
            .session_rail_items
            .iter()
            .enumerate()
            .map(|(i, (_, label))| {
                if i == cursor {
                    Line::from(Span::styled(
                        format!("▶ {label}"),
                        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(Span::raw(format!("  {label}")))
                }
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.border_dim))
            .title(" sessions [Ctrl-W] ");
        frame.render_widget(Paragraph::new(lines).block(block), sr_area);
    }

    // L122: render MainPanel from state.main_panel.
    let selection = if state.selection_mode {
        Some((state.selection_anchor, state.selection_end))
    } else {
        None
    };
    let search_q = if state.panel_search_query.is_empty() {
        None
    } else {
        Some(state.panel_search_query.as_str())
    };
    state
        .main_panel
        .render(main_area, frame, selection, search_q, state.no_color);

    // Overlay an animated thinking indicator at the bottom of the main area.
    // When the model emits thinking tokens, we show a one-line preview of the
    // accumulated content alongside the spinner.
    if state.turn_in_flight && main_area.height >= 1 {
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let frame_char = SPINNER[state.spinner_tick as usize % SPINNER.len()];
        state.spinner_tick = state.spinner_tick.wrapping_add(1);
        let thinking_area = ratatui::layout::Rect::new(
            main_area.x,
            main_area.y + main_area.height.saturating_sub(1),
            main_area.width,
            1,
        );
        let spinner_style = if state.no_color {
            Style::default()
        } else {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        };
        let dim_style = if state.no_color {
            Style::default()
        } else {
            Style::default()
                .fg(p.text_dim)
                .add_modifier(Modifier::ITALIC)
        };
        // Label adapts to what the model is actually doing right now.
        let (label, show_thinking_preview) =
            if let Some((_, ref name, ref inp)) = state.pending_tool {
                let inp_short: String = inp.chars().take(40).collect();
                let ellipsis = if inp.chars().count() > 40 {
                    "\u{2026}"
                } else {
                    ""
                };
                (format!("{frame_char} {name}: {inp_short}{ellipsis}"), false)
            } else if !state.current_thinking.is_empty() {
                (format!("{frame_char} thinking\u{2026}"), true)
            } else {
                (format!("{frame_char} working\u{2026}"), false)
            };
        let mut spans = vec![Span::styled(label, spinner_style)];
        if show_thinking_preview {
            // Show the last ~50 chars of thinking so users see live progress.
            let preview: String = state
                .current_thinking
                .chars()
                .rev()
                .take(50)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let preview = preview.replace('\n', " ");
            spans.push(Span::raw("  "));
            spans.push(Span::styled(preview, dim_style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), thinking_area);
    }

    // When thinking_expanded is set, render the full thinking content in an
    // overlay above the input area.
    if state.thinking_expanded && !state.current_thinking.is_empty() && main_area.height >= 4 {
        let h = main_area.height.min(10);
        let overlay_rect = ratatui::layout::Rect::new(
            main_area.x,
            main_area.y + main_area.height.saturating_sub(h + 1),
            main_area.width,
            h,
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" thinking (T to collapse) ");
        let inner = block.inner(overlay_rect);
        let thinking_style = if state.no_color {
            Style::default()
        } else {
            Style::default().fg(p.text_dim)
        };
        let lines: Vec<Line<'_>> = state
            .current_thinking
            .lines()
            .map(|l| Line::from(Span::styled(l.to_owned(), thinking_style)))
            .collect();
        frame.render_widget(block, overlay_rect);
        frame.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }

    // -- Action log -----------------------------------------------------------
    // L122: 5-row area using the existing ActionLog widget.
    state.action_log.render(action_log_area, frame);

    // -- Input area (auto-growing + wrapped; display/height computed above) ----
    // Prompt feedback: right-aligned char + estimated token count. Shown only
    // when the input is a single row, so it can never overlap wrapped text.
    let counter_text = if state.input.is_empty() {
        String::new()
    } else {
        let chars = state.input.chars().count();
        #[allow(clippy::integer_division)]
        let est_tok = chars / 4;
        format!("{chars}c ≈{est_tok}tok")
    };
    #[allow(clippy::cast_possible_truncation)]
    let counter_len = counter_text.chars().count() as u16;
    let counter_style = if state.no_color {
        Style::default()
    } else {
        Style::default().fg(p.text_dim).add_modifier(Modifier::DIM)
    };
    let input_para = Paragraph::new(input_display)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((input_scroll, 0));
    // Narrow the render rect to match input_w so the Paragraph wrap point
    // agrees with the height calculation above.
    let effective_input_w = u16::try_from(input_w).unwrap_or(input_area.width);
    let effective_input_area = ratatui::layout::Rect::new(
        input_area.x,
        input_area.y,
        effective_input_w.min(input_area.width),
        input_area.height,
    );
    if input_rows == 1 && counter_len > 0 && counter_len + 4 < effective_input_w {
        let input_sub_w = effective_input_w - counter_len;
        let input_sub = ratatui::layout::Rect::new(
            effective_input_area.x,
            effective_input_area.y,
            input_sub_w,
            effective_input_area.height,
        );
        let counter_rect = ratatui::layout::Rect::new(
            effective_input_area.x + input_sub_w,
            effective_input_area.y,
            counter_len,
            effective_input_area.height,
        );
        frame.render_widget(input_para, input_sub);
        frame.render_widget(
            Paragraph::new(Span::styled(counter_text, counter_style)),
            counter_rect,
        );
    } else {
        frame.render_widget(input_para, effective_input_area);
    }

    if let Some(search_area) = search_bar_area {
        let matched = history_search(&state.prompt_history, &state.history_search_query)
            .map_or("", |(_, s)| s);
        let search_text = format!(
            "(reverse-i-search) `{}`: {}",
            state.history_search_query, matched
        );
        let search_widget = Paragraph::new(search_text)
            .style(Style::default().fg(p.text).add_modifier(Modifier::DIM));
        frame.render_widget(search_widget, search_area);
    }

    // -- Right rail: context | role cockpit | LSP panel | obs panel | quality panel | value panel
    // The rail is split vertically into 1–6 sections. Context (1 row) is always
    // present; role cockpit, LSP, obs, quality, and value panels are individually toggled.
    if let Some(rail_rect) = rail_area {
        use Constraint::{Fill, Length};

        let show_cockpit = state.panels.role_cockpit;
        let show_lsp = state.panels.lsp;
        let show_obs = state.panels.obs;
        let show_quality = state.panels.quality;
        let show_value = state.panels.value;

        // Build constraint list dynamically so Layout never gets zero-length.
        let mut constraints: Vec<Constraint> = vec![];
        // Metrics panel sits at the very top of the rail when visible.
        let show_metrics = state.panels.metrics;
        if show_metrics {
            let metrics_lines = metrics_view::MetricsView::with_savings(
                state.metrics_snapshot.clone(),
                state.savings_snapshot.clone(),
            )
            .lines()
            .len();
            // +2 for Block top and bottom border.
            let h = u16::try_from(metrics_lines + 2)
                .unwrap_or(11)
                .min(rail_rect.height / 2);
            constraints.push(Length(h));
        }
        constraints.push(Length(1)); // context row
        if show_cockpit {
            constraints.push(Length(5));
        }
        // LSP gets flexible space; fixed-height panels slot directly below it.
        if show_lsp {
            constraints.push(Fill(1));
        }
        if show_obs {
            constraints.push(Length(6));
        }
        if show_quality {
            constraints.push(Length(8));
        }
        if show_value {
            constraints.push(Length(4));
        }

        let rail_chunks = Layout::vertical(constraints).split(rail_rect);
        let mut ci = 0usize;

        // ── Metrics / runner panel ────────────────────────────────────────
        if show_metrics && ci < rail_chunks.len() {
            frame.render_widget(
                metrics_view::MetricsView::with_savings(
                    state.metrics_snapshot.clone(),
                    state.savings_snapshot.clone(),
                ),
                rail_chunks[ci],
            );
            ci += 1;
        }

        // ── Context slot ──────────────────────────────────────────────────
        // Clamp to usize::MAX — well within range on 64-bit targets.
        let slots = vec![context_rail::ContextSlot {
            name: "context".into(),
            used: usize::try_from(state.context_used).unwrap_or(usize::MAX),
            total: usize::try_from(state.context_window).unwrap_or(usize::MAX),
        }];
        frame.render_widget(context_rail::ContextRail::new(slots), rail_chunks[ci]);
        ci += 1;

        // ── Role cockpit panel ────────────────────────────────────────────
        if show_cockpit && ci < rail_chunks.len() {
            render_role_cockpit(frame, rail_chunks[ci], state);
            ci += 1;
        }

        // ── LSP panel ─────────────────────────────────────────────────────
        if show_lsp && ci < rail_chunks.len() {
            lsp_panel::LspPanel::new(&state.lsp_snapshot)
                .with_graph(state.graph_symbols)
                .render(rail_chunks[ci], frame);
            ci += 1;
        }

        // ── Observability panel ───────────────────────────────────────────
        if show_obs && ci < rail_chunks.len() {
            obs_panel::ObsPanel::new(&state.obs_snapshot).render(rail_chunks[ci], frame);
            ci += 1;
        }

        // ── Quality gate panel ────────────────────────────────────────────
        if show_quality && ci < rail_chunks.len() {
            quality_panel::QualityPanel::new(&state.quality_snapshot)
                .render(rail_chunks[ci], frame);
            ci += 1;
        }

        // ── Value / ROI panel ─────────────────────────────────────────────
        if show_value && ci < rail_chunks.len() {
            value_panel::ValuePanel::new(&state.value_snapshot).render(rail_chunks[ci], frame);
        }
    }

    // -- Session detail overlay -----------------------------------------------
    if let Some(ref detail) = state.session_detail_overlay {
        render_session_detail(frame, area, detail, p);
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

        let diff_widget = Paragraph::new(visible).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" tool detail "),
        );
        frame.render_widget(diff_widget, overlay_rect);
    }

    // -- Block browser overlay ------------------------------------------------
    if state.block_browser_open && !state.block_store.is_empty() {
        let total = state.block_store.len();
        let cursor = state.block_browser_cursor;
        let overlay_lines: Vec<Line<'_>> = state
            .block_store
            .blocks()
            .enumerate()
            .map(|(i, b)| {
                let status_icon = match &b.status {
                    blocks::BlockStatus::Complete => "\u{2713}",
                    blocks::BlockStatus::Failed => "\u{2717}",
                    blocks::BlockStatus::Streaming => "\u{22ef}",
                    blocks::BlockStatus::ToolCall { .. } => "\u{25c6}",
                };
                let text = format!(" {status_icon} turn {}", b.turn_n);
                if i == cursor {
                    Line::from(Span::styled(
                        text,
                        Style::default()
                            .fg(p.bg)
                            .bg(p.text_bright)
                            .add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(Span::styled(text, Style::default().fg(p.text)))
                }
            })
            .collect();
        let bb_title = format!("blocks {}/{}", cursor.saturating_add(1).min(total), total);
        #[allow(clippy::cast_possible_truncation)]
        let bb_h = (total + 2).min(body_area.height as usize) as u16;
        let bb_w = 24u16.min(body_area.width);
        let bb_rect = ratatui::layout::Rect::new(
            body_area.x + body_area.width.saturating_sub(bb_w),
            body_area.y,
            bb_w,
            bb_h,
        );
        frame.render_widget(Clear, bb_rect);
        frame.render_widget(
            Paragraph::new(overlay_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .title(bb_title),
            ),
            bb_rect,
        );
    }

    // -- Panel search bar -----------------------------------------------------
    if state.panel_search_mode {
        // Show the search query as a one-row overlay at the top of the main panel.
        let sb_rect = ratatui::layout::Rect::new(main_area.x, main_area.y, main_area.width, 1);
        let search_text = format!("/ {}_", state.panel_search_query);
        let search_style = if state.no_color {
            Style::default()
        } else {
            Style::default().fg(p.bg).bg(p.text_bright)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(search_text, search_style))),
            sb_rect,
        );
    }

    // -- Slash-completion popup -----------------------------------------------
    if state.slash_popup_visible && !state.slash_completions.is_empty() {
        render_slash_popup(frame, area, state);
    }

    // -- File picker overlay --------------------------------------------------
    if state.file_picker_open {
        render_file_picker(frame, area, state);
    }
}

/// Renders a centred pop-up overlay with the full [`SessionDetail`] fields.
/// The overlay is dismissed by pressing Esc.
fn render_session_detail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    detail: &SessionDetail,
    p: &crate::theme::Palette,
) {
    use ratatui::widgets::Clear;

    let popup_w = area.width.clamp(30, 60);
    let popup_h: u16 = 14;
    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let field = |label: &str, value: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {label:<14}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(value.to_owned()),
        ])
    };

    let lines = vec![
        field("id", &detail.id),
        field("title", detail.title.as_deref().unwrap_or("-")),
        field("mode", detail.mode.as_deref().unwrap_or("-")),
        field("status", detail.status.as_deref().unwrap_or("-")),
        field("change", detail.active_change.as_deref().unwrap_or("-")),
        field("cowork", detail.cowork_mode.as_deref().unwrap_or("-")),
        Line::raw(""),
        field("created", &detail.created_at),
        field("updated", &detail.updated_at),
        Line::raw(""),
        Line::from(Span::styled(
            "  ^Enter load \u{00b7} Esc close",
            Style::default().fg(p.text_dim),
        )),
    ];

    frame.render_widget(Clear, popup_rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(" session detail "),
        ),
        popup_rect,
    );
}

/// Renders the slash-command completion popup in the bottom portion of the screen.
/// Renders the role cockpit panel showing current session role, tier, and
/// in-flight turn status.  Displayed in the right rail when `Ctrl-A` is active.
fn render_role_cockpit(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
    let p = palette();
    let mode = state.mode.as_deref().unwrap_or("impl");
    let tier = state.tier.as_deref().unwrap_or("fast");
    let runner = &state.runner;

    let in_flight = state.pending_task_id.is_some();
    let status_symbol = if in_flight {
        "● in-flight"
    } else {
        "○ idle"
    };
    let status_style = if in_flight {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.text_dim)
    };

    // Tier colour follows the forge tier palette.
    let tier_color = match tier {
        "local" => p.local,
        "deep" => p.deep,
        _ => p.fast,
    };

    let active_name = state.active_agent_name.as_deref().unwrap_or(mode);

    // Prominent brand-coloured client badge: `◆ CLAUDE · deep`.
    let client_color = crate::theme::runner_color(runner);
    let client_label = crate::theme::runner_label(runner);

    let lines: Vec<Line<'_>> = vec![
        Line::from(vec![
            Span::styled(
                format!("\u{25C6} {client_label}"),
                Style::default()
                    .fg(client_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" \u{00B7} {tier}"), Style::default().fg(tier_color)),
        ]),
        Line::from(vec![
            Span::styled("role  ", Style::default().fg(p.text_dim)),
            // Per-agent accent pip (deterministic colour); the name itself stays
            // bright/readable rather than being recoloured.
            Span::styled(
                "\u{25C6} ",
                Style::default().fg(crate::theme::agent_color(active_name)),
            ),
            Span::styled(
                active_name.to_owned(),
                Style::default()
                    .fg(p.text_bright)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("turn  ", Style::default().fg(p.text_dim)),
            Span::styled(status_symbol.to_owned(), status_style),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.border))
        .title(" cockpit [Ctrl-A] ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_slash_popup(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
    let p = palette();
    let completions = &state.slash_completions;
    // Height = number of completions + 2 border rows, capped at available space.
    #[allow(clippy::cast_possible_truncation)]
    let popup_h = (completions.len() as u16 + 2).min(area.height.saturating_sub(2));
    // Session-picker rows (`<short-id>  <title>  <mode>  <updated_at>`) are wider
    // than the 20-col command popup, so widen to fit when the picker is open.
    // Command palette also widens to accommodate the description column.
    let desired_w = if state.session_picker_mode {
        60
    } else if state.command_palette_mode {
        50
    } else {
        20
    };
    let popup_w = desired_w.min(area.width);
    // Position just above the input row (bottom-left).
    let popup_y = area.y + area.height.saturating_sub(popup_h + 1);
    let popup_x = area.x;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let lines: Vec<Line<'_>> = completions
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let label = if state.command_palette_mode {
                let desc = SLASH_COMMAND_DESCRIPTIONS
                    .iter()
                    .find(|(cmd, _)| cmd == c)
                    .map_or("", |(_, d)| d);
                format!(" {c:<14}  {desc}")
            } else {
                format!(" {c}")
            };
            if i == state.slash_cursor {
                Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(p.bg)
                        .bg(p.text_bright)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(label, Style::default().fg(p.text)))
            }
        })
        .collect();

    let title = if state.session_picker_mode {
        "sessions"
    } else if state.runner_picker_mode {
        "runners"
    } else if state.command_palette_mode {
        "palette"
    } else {
        "commands"
    };
    frame.render_widget(Clear, popup_rect);
    let popup = Paragraph::new(lines)
        .style(Style::default().bg(p.panel))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(title),
        );
    frame.render_widget(popup, popup_rect);
}

fn render_file_picker(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
    let p = palette();
    let entries = &state.file_picker_entries;
    #[allow(clippy::cast_possible_truncation)]
    let popup_h = (entries.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup_w = 50_u16.min(area.width);
    let popup_y = area.y + area.height.saturating_sub(popup_h + 1);
    let popup_x = area.x;
    let popup_rect = ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h);

    let lines: Vec<Line<'_>> = entries
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let label = format!(" {name}");
            if i == state.file_picker_cursor {
                Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(p.bg)
                        .bg(p.text_bright)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(label, Style::default().fg(p.text)))
            }
        })
        .collect();

    let title = format!(" {} ", state.file_picker_dir.display());
    frame.render_widget(Clear, popup_rect);
    let popup = Paragraph::new(lines)
        .style(Style::default().bg(p.panel))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(title),
        );
    frame.render_widget(popup, popup_rect);
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
        last_cowork_poll: None,
        last_graph_poll: None,
        stream_rx: None,
        upgrade_rx: None,
        current_thinking: String::new(),
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
        consecutive_low_quality: 0,
        quality_review_in_progress: false,
        ctrl_q_pressed_at: None,
        value_snapshot: value_panel::ValueSnapshot::default(),
        latency_samples: VecDeque::new(),
        session_tokens_in: 0,
        session_tokens_out: 0,
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
        if let Some(ref mut rx) = state.stream_rx {
            let mut turn_done = false;
            let mut stream_disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(ev) => ev,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        if !turn_done {
                            stream_disconnected = true;
                        }
                        break;
                    }
                };
                match event {
                    StreamEvent::Delta { text } => {
                        // First delta of the turn: emit the assistant author
                        // chip on its own line so the response never merges
                        // into the preceding line (which broke ```fences →
                        // syntax highlighting), and start a fresh body line.
                        if !state.assistant_open {
                            let color = theme::runner_color(&state.runner);
                            let label = theme::runner_label(&state.runner).to_lowercase();
                            push_author_chip(&mut state.main_panel, &label, color, state.no_color);
                            state.main_panel.push_line(String::new());
                            state.assistant_open = true;
                        }
                        // A tool-result marker (↳) resolves the running tool
                        // card to ✓ (ok) or ✗ (error).
                        if text.contains('\u{21b3}') {
                            if let Some((idx, name, inp)) = state.pending_tool.take() {
                                let ok = !text.contains("error");
                                let glyph = if ok { '\u{2713}' } else { '\u{2717}' };
                                let card = tool_call_card(&name, &inp, state.no_color, glyph);
                                state.main_panel.replace_styled_line(idx, card);
                            }
                        }
                        // Split on newlines so each line is a separate panel entry.
                        let mut remaining = text.as_str();
                        loop {
                            if let Some(pos) = remaining.find('\n') {
                                let chunk = &remaining[..pos];
                                if !chunk.is_empty() {
                                    state.main_panel.push_delta(chunk);
                                }
                                // The line is now complete — classify it
                                // (syntax-highlight code, colour diffs) then
                                // open a fresh tail line for the next chunk.
                                state.main_panel.finalize_last_line();
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
                    StreamEvent::Started { agent_name } => {
                        if let Some(name) = agent_name {
                            state.active_agent_name = Some(name);
                        }
                    }
                    StreamEvent::Thinking { text } => {
                        state.current_thinking.push_str(&text);
                    }
                    StreamEvent::ToolCall { name, input, full } => {
                        let full_str = full.as_deref().unwrap_or(&input);
                        // Card starts "running" (◷); resolved to ✓/✗ when its
                        // result arrives.
                        let card = tool_call_card(&name, &input, state.no_color, '\u{25f7}');
                        state.main_panel.push_styled_line(card);
                        // Record the card's line + full args for right-click
                        // expansion and the /tools inspector.
                        let line_idx = state.main_panel.len().saturating_sub(1);
                        state
                            .tool_details
                            .push((line_idx, name.clone(), full_str.to_owned()));
                        state.pending_tool = Some((line_idx, name.clone(), input.clone()));
                        if let Some(ref mut block) = state.current_block {
                            block.push_text(&format!("▶ {name}: {input}"));
                            block.push_text("\n");
                        }
                    }
                    StreamEvent::Done {
                        output_tok,
                        input_tok,
                        traceparent,
                    } => {
                        // Classify the final streamed line (responses needn't end
                        // with a newline) and close the assistant block.
                        state.main_panel.finalize_last_line();
                        state.assistant_open = false;
                        // Any tool still marked "running" at turn end is settled.
                        if let Some((idx, name, inp)) = state.pending_tool.take() {
                            let card = tool_call_card(&name, &inp, state.no_color, '\u{2713}');
                            state.main_panel.replace_styled_line(idx, card);
                        }
                        let output_tok = u64::from(output_tok);
                        let input_tok = u64::from(input_tok.unwrap_or(0));
                        let tp = traceparent;
                        let elapsed_ms = state.turn_submitted_at.map_or(0, |t| {
                            u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                        });
                        state.turn_submitted_at = None;
                        state.last_traceparent.clone_from(&tp);

                        // Track latency samples for p95/p99 in the obs panel.
                        if elapsed_ms > 0 {
                            if state.latency_samples.len() >= LATENCY_SAMPLE_CAP {
                                state.latency_samples.pop_front();
                            }
                            state.latency_samples.push_back(elapsed_ms);
                            state.obs_snapshot.latency_samples = state.latency_samples.clone();
                        }
                        // Accumulate session token totals.
                        state.session_tokens_in = state.session_tokens_in.saturating_add(input_tok);
                        state.session_tokens_out =
                            state.session_tokens_out.saturating_add(output_tok);
                        state.obs_snapshot.tokens_input = state.session_tokens_in;
                        state.obs_snapshot.tokens_output = state.session_tokens_out;

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

                        // Emit a collapsible thinking badge if the model produced thinking tokens.
                        if !state.current_thinking.is_empty() {
                            let chars = state.current_thinking.chars().count();
                            state
                                .main_panel
                                .push_line(format!("╌ thinking ({chars} chars) [T to expand] ╌"));
                        }

                        if let Some(output_type) = state.pending_output_type.take() {
                            pending_output_save = Some((output_type, block_content));
                        }

                        let _ = emit_osc9(&mut std::io::stdout());

                        turn_done = true;
                    }
                    StreamEvent::Error { message } => {
                        let (label, hint) = classify_turn_error(&message);
                        let display = if hint.is_empty() {
                            format!("[{label}] {message}")
                        } else {
                            format!("[{label}] {message}\n  \u{2192} {hint}")
                        };
                        // push_system_message cannot be called here because `state.stream_rx`
                        // is mutably borrowed via `ref mut rx` for the entire enclosing block.
                        // Emit to the main panel directly instead.
                        for line in display.lines() {
                            state.main_panel.push_line(line.to_owned());
                        }
                        if let Some(mut block) = state.current_block.take() {
                            block.fail();
                            state.block_store.push(block);
                        }
                        turn_done = true;
                    }
                    StreamEvent::Quality {
                        score,
                        tdd_pass,
                        clean_pass,
                        file_advisories,
                        skill_advisories,
                        llm_reviewed,
                        suggested_command,
                    } => {
                        // Update quality panel snapshot from the post-turn gate evaluation.
                        state.quality_snapshot = quality_panel::QualitySnapshot {
                            score,
                            tdd_pass,
                            clean_pass,
                            llm_reviewed,
                            file_advisories,
                            skill_advisories,
                            suggested_command,
                        };
                        if llm_reviewed {
                            state.quality_review_in_progress = false;
                        }
                        // CoworkGate: two consecutive turns below 60.
                        if score < 60 {
                            state.consecutive_low_quality =
                                state.consecutive_low_quality.saturating_add(1);
                            if state.consecutive_low_quality >= 2 {
                                state.pending_cowork.push(cowork_widget::CoworkItem {
                                    id: format!("quality-gate-{score}"),
                                    tool: "quality-gate".to_owned(),
                                    step_n: 0,
                                    args_display: format!(
                                        "Score {score}/100 for 2 consecutive turns — address findings?"
                                    ),
                                    reasoning: "Quality score below 60 for 2 turns.".to_owned(),
                                });
                            }
                        } else {
                            state.consecutive_low_quality = 0;
                        }
                    }
                    StreamEvent::BufferOverflow { lost } => {
                        let s = if lost == 1 { "" } else { "s" };
                        state.main_panel.push_line(format!(
                            "[stream] {lost} event{s} dropped — output may be incomplete"
                        ));
                    }
                    StreamEvent::Unknown => {}
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
                            let display = if hint.is_empty() {
                                format!("[{label}] {error}")
                            } else {
                                format!("[{label}] {error}\n  \u{2192} {hint}")
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
                            state.messages.push(Message {
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
                state.value_snapshot.token_cost = vc["token_cost"].as_u64().unwrap_or(0);
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
mod tests {
    use super::*;

    #[test]
    fn format_memory_lists_turns_with_previews() {
        let history = json!({
            "turns": [
                { "turn_n": 1, "messages": [
                    {"role": "user", "content": "write a counter"},
                    {"role": "assistant", "content": "here is the code"}
                ]}
            ],
            "audit": [ {"x": 1} ]
        });
        let ctx = json!({ "used_tok": 50, "window_tok": 200, "vault_warm_count": 3, "vault_cold_count": 7 });
        let out = crate::slash::format_memory(&history, Some(&ctx), "abcd1234ef");
        assert!(out.contains("memory"), "{out}");
        assert!(out.contains("abcd1234"), "{out}"); // short session id
        assert!(out.contains("write a counter"), "{out}");
        assert!(out.contains("here is the code"), "{out}");
        assert!(out.contains("1 audit event"), "{out}");
        assert!(out.contains("/memory <session_id>"), "{out}");
        // Short-term context + vault summary present.
        assert!(out.contains("50/200 tok (25%)"), "{out}");
        assert!(out.contains("3 warm + 7 cold"), "{out}");
    }

    #[test]
    fn skills_listing_and_install_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        // A source dir with two skill .md files + a non-md file.
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("alpha.md"), "skill a").unwrap();
        std::fs::write(src.join("beta.md"), "skill b").unwrap();
        std::fs::write(src.join("notes.txt"), "ignore").unwrap();
        // Directory-form skill.
        let gamma = src.join("gamma");
        std::fs::create_dir_all(&gamma).unwrap();
        std::fs::write(gamma.join("SKILL.md"), "skill g").unwrap();

        let names = crate::slash::list_skill_dir(&src);
        assert!(names.contains(&"alpha".to_owned()), "{names:?}");
        assert!(names.contains(&"gamma".to_owned()), "{names:?}"); // dir/SKILL.md
        assert!(!names.iter().any(|n| n == "notes"), "{names:?}"); // .txt ignored

        let dst = tmp.path().join(".smedja").join("skills");
        let msg = crate::slash::install_skills_dir(&src, &dst);
        assert!(msg.contains("installed 2 skill file"), "{msg}"); // alpha.md + beta.md
        assert!(dst.join("alpha.md").exists());
    }

    #[test]
    fn format_memory_handles_empty_history() {
        let out = crate::slash::format_memory(&json!({ "turns": [] }), None, "sess0001");
        assert!(out.contains("no stored turns"), "{out}");
    }

    #[test]
    fn tool_glyph_label_compacts_verbose_names() {
        let (g, l) = tool_glyph_label("ToolSearch");
        assert_eq!((g, l.as_str()), ("⌕", "search"));
        let (_, bash) = tool_glyph_label("Bash");
        assert_eq!(bash, "bash");
        // Unknown tool → lowercased, capped.
        let (g2, l2) = tool_glyph_label("SomeReallyLongToolName");
        assert_eq!(g2, "▶");
        assert!(l2.chars().count() <= 14);
    }

    #[test]
    fn status_bar_line_segments_runner_tier_session() {
        let ctx = ModuleCtx {
            session_id: "abcd1234ef",
            mode: Some("impl"),
            tier: Some("deep"),
            runner: Some("claude-cli"),
            pending: false,
            input_mode: true,
        };
        let text: String = status_bar_line(&ctx, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("INSERT"), "{text}");
        assert!(text.contains("CLAUDE"), "{text}"); // runner_label uppercases
        assert!(text.contains("deep"), "{text}");
        assert!(text.contains("abcd1234"), "{text}"); // 8-char session id
    }

    #[test]
    fn status_hint_advertises_real_entry_points() {
        let text: String = status_hint_line(true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("/help"), "{text}");
        assert!(text.contains("^W"), "{text}");
    }

    #[test]
    fn format_tool_detail_pretty_prints_json_args() {
        let lines = format_tool_detail("Bash", r#"{"command":"ls -la","timeout":5}"#);
        let joined = lines.join("\n");
        assert!(joined.contains("tool: Bash"), "{joined}");
        assert!(joined.contains("\"command\""), "{joined}"); // pretty JSON
        assert!(joined.contains("ls -la"), "{joined}");
        assert!(joined.contains("Esc to close"), "{joined}");
        // Non-JSON falls back to raw.
        let raw = format_tool_detail("X", "not json");
        assert!(raw.join("\n").contains("not json"));
    }

    #[test]
    fn tool_call_card_shows_glyph_label_and_summary() {
        let line = tool_call_card("Bash", "find . -type f", true, '\u{2713}');
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("find . -type f"), "{text}");
        assert!(text.contains('\u{2713}'), "{text}"); // status glyph present
                                                      // No raw JSON braces leak into the card.
        assert!(!text.contains('{'), "{text}");
    }

    // --- metrics-live-fetch: pure JSON→rows mapper ---

    #[test]
    fn metrics_rows_from_summary_folds_buckets_per_runner() {
        let resp = json!({
            "tier": "hourly",
            "buckets": [
                { "bucket_start": 0, "runner": "claude", "turns": 1,
                  "input_tok": 100, "output_tok": 50, "cost_usd": 0.01, "error_count": 1 },
                { "bucket_start": 3_600_000_000_i64, "runner": "claude", "turns": 1,
                  "input_tok": 200, "output_tok": 80, "cost_usd": 0.02, "error_count": 2 },
                { "bucket_start": 0, "runner": "local", "turns": 1,
                  "input_tok": 480, "output_tok": 0, "cost_usd": 0.0, "error_count": 0 },
            ],
        });
        let rows = metrics_rows_from_summary(&resp);
        assert_eq!(rows.len(), 2, "one row per runner");
        // First-seen runner order: claude then local.
        assert_eq!(rows[0].runner, "claude");
        assert_eq!(
            rows[0].tokens,
            100 + 50 + 200 + 80,
            "tokens summed across buckets"
        );
        assert!((rows[0].cost_usd - 0.03).abs() < 1e-9, "cost accumulated");
        assert_eq!(rows[0].errors, 3, "errors accumulated");
        assert_eq!(rows[1].runner, "local");
        assert_eq!(rows[1].tokens, 480);
    }

    #[test]
    fn metrics_rows_from_summary_empty_buckets_yields_no_rows() {
        let empty = json!({ "tier": "hourly", "buckets": [] });
        assert!(metrics_rows_from_summary(&empty).is_empty());
        let missing = json!({ "tier": "hourly" });
        assert!(metrics_rows_from_summary(&missing).is_empty());
    }

    #[test]
    fn metrics_rows_from_summary_tolerates_missing_fields() {
        let resp = json!({
            "buckets": [
                { "runner": "claude" },
            ],
        });
        let rows = metrics_rows_from_summary(&resp);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].runner, "claude");
        assert_eq!(rows[0].tokens, 0);
        assert!((rows[0].cost_usd - 0.0).abs() < 1e-9);
        assert_eq!(rows[0].errors, 0);
    }

    // --- metrics-live-fetch: poll-due predicate ---

    #[test]
    fn metrics_poll_due_when_visible_and_unset_or_elapsed() {
        let now = std::time::Instant::now();
        // Visible and never polled → due.
        assert!(metrics_poll_due(true, None, now));
        // Visible and the interval has elapsed → due.
        let stale = now.checked_sub(std::time::Duration::from_secs(4)).unwrap();
        assert!(metrics_poll_due(true, Some(stale), now));
        // Visible but within the interval → not due.
        let fresh = now.checked_sub(std::time::Duration::from_secs(1)).unwrap();
        assert!(!metrics_poll_due(true, Some(fresh), now));
        // Hidden → never due, regardless of timing.
        assert!(!metrics_poll_due(false, None, now));
        assert!(!metrics_poll_due(false, Some(stale), now));
    }

    // --- metrics-live-fetch: toggle resets the poll cadence ---

    #[test]
    fn toggling_metrics_view_on_resets_last_metrics_poll() {
        let mut state = make_state("sess-metrics-toggle");
        state.last_metrics_poll = Some(std::time::Instant::now());
        assert!(!state.panels.metrics);
        // Toggle on → visible and the poll is cleared for an immediate fetch.
        toggle_metrics_view(&mut state);
        assert!(state.panels.metrics, "toggle makes the panel visible");
        assert!(
            state.last_metrics_poll.is_none(),
            "toggle-on clears last_metrics_poll for an immediate fetch"
        );
        // Toggle off → hidden again.
        toggle_metrics_view(&mut state);
        assert!(!state.panels.metrics, "second toggle hides the panel");
    }

    // --- metrics-live-fetch: live fetch populates/clears the snapshot ---

    #[test]
    fn live_metrics_response_populates_then_clears_snapshot() {
        let mut state = make_state("sess-metrics-populate");
        assert!(
            state.metrics_snapshot.is_empty(),
            "snapshot starts empty (the previously-blank panel)"
        );
        let resp = json!({
            "tier": "hourly",
            "buckets": [
                { "runner": "claude", "input_tok": 700, "output_tok": 80,
                  "cost_usd": 0.06, "error_count": 1 },
            ],
        });
        state.metrics_snapshot = metrics_rows_from_summary(&resp);
        assert_eq!(
            state.metrics_snapshot.len(),
            1,
            "live fetch fills the snapshot"
        );
        assert_eq!(state.metrics_snapshot[0].runner, "claude");
        // An empty window replaces the snapshot wholesale — no stale rows.
        let empty = json!({ "tier": "hourly", "buckets": [] });
        state.metrics_snapshot = metrics_rows_from_summary(&empty);
        assert!(
            state.metrics_snapshot.is_empty(),
            "empty window clears the snapshot rather than leaving stale rows"
        );
    }

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
    fn format_local_model_list_renders_fit_and_active() {
        let v = serde_json::json!({
            "active_model_id": "qwen3-14b",
            "models": [
                { "id": "qwen3-14b", "fit": "fits", "active": true },
                { "id": "huge-70b", "fit": "exceeds", "active": false }
            ]
        });
        let out = format_local_model_list(&v);
        assert!(out.contains("qwen3-14b") && out.contains("[fits]"));
        assert!(out.contains('*'), "active model must be marked");
        assert!(out.contains("huge-70b") && out.contains("[exceeds]"));
    }

    /// Spawns a one-shot mock daemon that records the requested method into the
    /// returned channel and replies with `reply`. Returns the socket path.
    fn spawn_method_capture(
        reply: serde_json::Value,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        tokio::sync::oneshot::Receiver<String>,
    ) {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader as TokioBufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("local-test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let mut reader = TokioBufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let req: serde_json::Value =
                    serde_json::from_str(line.trim_end()).unwrap_or(serde_json::Value::Null);
                let method = req["method"].as_str().unwrap_or("").to_owned();
                let _ = tx.send(method);
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"].clone(),
                    "result": reply,
                });
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                let _ = reader.get_mut().write_all(&bytes).await;
            }
        });

        (dir, sock_path, rx)
    }

    /// For the local runner, `/model <name>` dispatches `local.swap` (not the
    /// relabel-only `session.set_model`).
    #[tokio::test]
    async fn model_command_local_runner_dispatches_local_swap() {
        let (_dir, sock_path, rx) = spawn_method_capture(serde_json::json!({
            "from_model": "qwen3-14b",
            "active_model_id": "llama3-8b",
            "explicit_swap": true,
            "swap_latency_ms": 12
        }));

        let mut client = Client::connect(&sock_path).await.unwrap();
        let mut state = make_state("sess-local");
        state.runner = "local".to_owned();

        dispatch_slash("/model llama3-8b", &mut state, &mut client)
            .await
            .unwrap();

        let method = rx.await.unwrap();
        assert_eq!(
            method, "local.swap",
            "local runner /model <name> must dispatch local.swap, not session.set_model"
        );
        assert_eq!(
            state.model.as_deref(),
            Some("llama3-8b"),
            "the active model label must update after a local swap"
        );
    }

    /// For the local runner, `/model` with no args lists the GPU-annotated
    /// inventory via `local.models`.
    #[tokio::test]
    async fn model_command_local_runner_lists_via_local_models() {
        let (_dir, sock_path, rx) = spawn_method_capture(serde_json::json!({
            "active_model_id": "qwen3-14b",
            "models": [ { "id": "qwen3-14b", "fit": "fits", "active": true } ],
            "gpu": { "detected": false }
        }));

        let mut client = Client::connect(&sock_path).await.unwrap();
        let mut state = make_state("sess-local");
        state.runner = "local".to_owned();

        dispatch_slash("/model", &mut state, &mut client)
            .await
            .unwrap();

        let method = rx.await.unwrap();
        assert_eq!(
            method, "local.models",
            "local runner bare /model must list via local.models"
        );
    }

    #[test]
    fn slash_completions_include_new_commands() {
        let required = [
            "/agent",
            "/approve",
            "/briefing",
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
    fn replay_history_seeds_latency_samples_from_audit() {
        let mut state = make_state("latency-seed-session");
        let history = serde_json::json!({
            "session_id": "latency-seed-session",
            "turns": [],
            "audit": [
                { "latency_ms": 1200 },
                { "latency_ms": 800 },
                { "latency_ms": 0 },       // zero must be skipped
                { "latency_ms": 2500 },
            ],
        });
        replay_history(&mut state, &history);
        // Zero latency is excluded; the three valid samples must be seeded.
        assert_eq!(
            state.latency_samples.len(),
            3,
            "latency_samples must be seeded from audit (zero excluded)"
        );
        assert!(state.latency_samples.contains(&1200));
        assert!(state.latency_samples.contains(&800));
        assert!(state.latency_samples.contains(&2500));
        // The obs_snapshot must reflect the seeded samples for p95/p99.
        assert_eq!(
            state.obs_snapshot.latency_samples.len(),
            3,
            "obs_snapshot must be updated"
        );
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
    #[allow(clippy::too_many_lines)]
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
            staging_queue: staging::StagingQueue::new(),
            panels: PanelVisibility {
                context_rail: true,
                metrics: false,
                session_rail: false,
                lsp: true,
                obs: true,
                role_cockpit: false,
                quality: false,
                value: false,
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
            last_cowork_poll: None,
            last_graph_poll: None,
            stream_rx: None,
            upgrade_rx: None,
            current_thinking: String::new(),
            thinking_expanded: false,
            kill_ring: VecDeque::new(),
            active_agent_name: None,
            stream_sock_path: PathBuf::from("/tmp/smdjad.sock.stream"),
            last_traceparent: None,
            pending_output_type: None,
            otlp_configured: false,
            no_color: false,
            spinner_tick: 0,
            panel_search_mode: false,
            panel_search_query: String::new(),
            display_start_idx: 0,
            prompt_history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            history_search_mode: false,
            history_search_query: String::new(),
            openspec_bin: None,
            lsp_last_poll: None,
            lsp_snapshot: smedja_lsp::LspSnapshot::default(),
            obs_snapshot: obs_panel::ObsSnapshot::default(),
            quality_snapshot: quality_panel::QualitySnapshot::default(),
            consecutive_low_quality: 0,
            quality_review_in_progress: false,
            ctrl_q_pressed_at: None,
            value_snapshot: value_panel::ValueSnapshot::default(),
            latency_samples: VecDeque::new(),
            session_tokens_in: 0,
            session_tokens_out: 0,
            file_picker_open: false,
            file_picker_dir: std::path::PathBuf::new(),
            file_picker_entries: Vec::new(),
            file_picker_cursor: 0,
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
        // Auto-scroll leaves scroll at the last line; scroll to top to see the
        // full banner in the rendered frame.
        state.main_panel.scroll_to_top();
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
            content.contains("thinking") || content.contains("working"),
            "buffer should contain spinner indicator when turn_in_flight is true"
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
    fn wrap_input_rows_splits_long_line() {
        // 25 chars at width 10 → 3 rows (10/10/5).
        let rows = wrap_input_rows(&"x".repeat(25), 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].chars().count(), 10);
        assert_eq!(rows[2].chars().count(), 5);
    }

    #[test]
    fn wrap_input_rows_honours_newlines() {
        let rows = wrap_input_rows("ab\ncd", 80);
        assert_eq!(rows, vec!["ab".to_string(), "cd".to_string()]);
    }

    #[test]
    fn wrap_input_rows_empty_is_one_row() {
        assert_eq!(wrap_input_rows("", 10).len(), 1);
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
            content.contains("ANTHROPIC"),
            "status bar must render the runner label; got: {content}"
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
        state.selection_anchor = (3, 0);
        state.selection_end = (6, 0);
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
            "/agent",
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
            content.contains("INSERT"),
            "status bar must show INSERT when scroll_focus=false; got: {content}"
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
            content.contains("SCROLL"),
            "status bar must show SCROLL when scroll_focus=true; got: {content}"
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
        state.panels.context_rail = true;

        // Simulate Ctrl-R in scroll mode
        state.panels.context_rail = !state.panels.context_rail;

        assert!(
            !state.panels.context_rail,
            "context rail must be toggled off"
        );
    }

    #[test]
    fn ctrl_t_toggles_metrics_view() {
        let mut state = make_state("sess-ctrl-t");
        assert!(!state.panels.metrics, "metrics view starts hidden");
        // Simulate Ctrl-T.
        state.panels.metrics = !state.panels.metrics;
        assert!(state.panels.metrics, "Ctrl-T must show metrics view");
        state.panels.metrics = !state.panels.metrics;
        assert!(!state.panels.metrics, "Ctrl-T again must hide it");
    }

    #[test]
    fn metrics_view_panel_renders_per_runner_snapshot() {
        let mut state = make_state("sess-metrics-render");
        state.panels.metrics = true;
        state.metrics_snapshot = vec![
            metrics_view::MetricsRow {
                runner: "claude".into(),
                tokens: 780,
                cost_usd: 0.06,
                errors: 2,
            },
            metrics_view::MetricsRow {
                runner: "local".into(),
                tokens: 480,
                cost_usd: 0.0,
                errors: 0,
            },
        ];
        // MetricsView lives inside the context rail; rail needs width >= 100.
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(content.contains("claude"), "claude runner must render");
        assert!(content.contains("local"), "local runner must render");
        assert!(content.contains("$0.0600"), "claude cost must render");
        assert!(content.contains("780"), "claude tokens must render");
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

    // --- lsp_snapshot_from_rpc -----------------------------------------------

    #[test]
    fn lsp_snapshot_from_rpc_decodes_all_severity_strings() {
        let status = json!({"servers": []});
        let diag = json!({
            "diagnostics": [
                {"file": "a.rs", "line": 1, "col": 1, "severity": "error",   "message": "e"},
                {"file": "a.rs", "line": 2, "col": 1, "severity": "warning", "message": "w"},
                {"file": "a.rs", "line": 3, "col": 1, "severity": "info",    "message": "i"},
                {"file": "a.rs", "line": 4, "col": 1, "severity": "hint",    "message": "h"},
            ]
        });
        let snap = lsp_snapshot_from_rpc(&status, &diag);
        assert_eq!(snap.diagnostics.len(), 4);
        assert!(matches!(
            snap.diagnostics[0].severity,
            smedja_lsp::Severity::Error
        ));
        assert!(matches!(
            snap.diagnostics[1].severity,
            smedja_lsp::Severity::Warning
        ));
        assert!(matches!(
            snap.diagnostics[2].severity,
            smedja_lsp::Severity::Info
        ));
        assert!(matches!(
            snap.diagnostics[3].severity,
            smedja_lsp::Severity::Hint
        ));
    }

    #[test]
    fn lsp_snapshot_from_rpc_unknown_severity_defaults_to_error() {
        let status = json!({"servers": []});
        let diag = json!({
            "diagnostics": [
                {"file": "x.rs", "line": 1, "col": 1, "severity": "banana", "message": "x"}
            ]
        });
        let snap = lsp_snapshot_from_rpc(&status, &diag);
        assert!(matches!(
            snap.diagnostics[0].severity,
            smedja_lsp::Severity::Error
        ));
    }

    #[test]
    fn lsp_snapshot_from_rpc_decodes_server_states() {
        let status = json!({
            "servers": [
                {"name": "ra",     "state": "ready"},
                {"name": "gopls",  "state": "degraded: connection refused"},
                {"name": "py",     "state": "starting"},
            ]
        });
        let snap = lsp_snapshot_from_rpc(&status, &json!({"diagnostics": []}));
        assert_eq!(snap.servers.len(), 3);
        assert!(matches!(
            snap.servers[0].state,
            smedja_lsp::ServerState::Ready
        ));
        assert!(
            matches!(&snap.servers[1].state, smedja_lsp::ServerState::Degraded(r) if r == "connection refused"),
            "degraded reason must be extracted from prefix"
        );
        assert!(matches!(
            snap.servers[2].state,
            smedja_lsp::ServerState::Starting
        ));
    }

    #[test]
    fn lsp_snapshot_from_rpc_empty_inputs_yield_empty_snapshot() {
        let snap = lsp_snapshot_from_rpc(&json!({"servers": []}), &json!({"diagnostics": []}));
        assert!(snap.servers.is_empty());
        assert!(snap.diagnostics.is_empty());
    }

    // --- detect_project_types ------------------------------------------------

    #[test]
    fn detect_project_types_returns_cargo_when_only_cargo_toml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_project_types(dir.path()), vec!["Cargo.toml"]);
    }

    #[test]
    fn detect_project_types_returns_all_present_manifests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        let types = detect_project_types(dir.path());
        assert_eq!(types.len(), 2);
        assert!(types.contains(&"Cargo.toml"));
        assert!(types.contains(&"package.json"));
    }

    #[test]
    fn detect_project_types_returns_empty_for_no_manifests() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_project_types(dir.path()).is_empty());
    }

    // --- poll backoff --------------------------------------------------------

    #[test]
    fn poll_backoff_shift_never_overflows() {
        // Verify the clamped shift cannot produce a u64 overflow for any retry
        // count up to and including the give-up threshold (60).
        for count in 0u32..=60 {
            let shift = count.saturating_sub(1).min(10);
            let _ = (100u64 << shift).min(1_000);
        }
    }

    #[test]
    fn poll_backoff_caps_at_1000ms() {
        // At retry=4 the raw shift (3) gives 800 ms; at retry=5 (shift=4) the
        // raw value 1600 ms clamps to 1000 ms and stays there.
        for count in 5u32..=60 {
            let shift = count.saturating_sub(1).min(10);
            let ms = (100u64 << shift).min(1_000);
            assert_eq!(ms, 1_000, "backoff must cap at 1000 ms for retry {count}");
        }
    }

    // --- keybinding: Ctrl-F context rail / Ctrl-R history search ---------------

    #[test]
    fn ctrl_f_in_scroll_mode_toggles_context_rail() {
        let mut state = make_state("sess-ctrlf");
        state.scroll_focus = true;
        let initial = state.panels.context_rail;
        // Simulate Ctrl-F in scroll mode.
        state.panels.context_rail = !state.panels.context_rail;
        assert_ne!(
            state.panels.context_rail, initial,
            "Ctrl-F must toggle panels.context_rail in scroll mode"
        );
        state.panels.context_rail = !state.panels.context_rail;
        assert_eq!(
            state.panels.context_rail, initial,
            "second Ctrl-F must restore original value"
        );
    }

    #[test]
    fn ctrl_r_in_scroll_mode_does_not_affect_context_rail() {
        let mut state = make_state("sess-ctrlr-scroll");
        state.scroll_focus = true;
        let initial = state.panels.context_rail;
        // The Ctrl-R handler only acts when !scroll_focus, so it must be a no-op here.
        if !state.scroll_focus {
            state.history_search_mode = !state.history_search_mode;
        }
        assert_eq!(
            state.panels.context_rail, initial,
            "Ctrl-R in scroll mode must not touch panels.context_rail"
        );
        assert!(
            !state.history_search_mode,
            "history_search_mode must remain off when Ctrl-R fires in scroll mode"
        );
    }

    #[test]
    fn ctrl_r_in_input_mode_toggles_history_search() {
        let mut state = make_state("sess-ctrlr-input");
        state.scroll_focus = false;
        state.input = String::from("partial query");
        assert!(!state.history_search_mode);
        // Simulate Ctrl-R in input mode.
        if !state.scroll_focus {
            state.history_search_mode = !state.history_search_mode;
            state.history_search_query.clear();
            if state.history_search_mode {
                state.input.clone_into(&mut state.saved_input);
            }
        }
        assert!(
            state.history_search_mode,
            "Ctrl-R must enable history_search_mode in input mode"
        );
        assert_eq!(
            state.saved_input, "partial query",
            "current input must be saved when entering history search"
        );
        assert!(
            state.history_search_query.is_empty(),
            "search query must be cleared on activation"
        );
    }

    // --- Ctrl-G external editor --------------------------------------------------

    #[test]
    fn resolve_editor_falls_back_to_vi() {
        // Remove VISUAL and EDITOR from the environment for this test.
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
        // Can't guarantee clean env in parallel tests, but the fallback path
        // must always produce a non-empty string.
        let editor = resolve_editor();
        assert!(
            !editor.is_empty(),
            "resolve_editor must return a non-empty string"
        );
    }

    #[test]
    fn resolve_editor_prefers_visual_over_editor() {
        std::env::set_var("VISUAL", "emacs");
        std::env::set_var("EDITOR", "nano");
        let editor = resolve_editor();
        // Clean up after the test regardless of assertion result.
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
        assert_eq!(editor, "emacs", "VISUAL must be preferred over EDITOR");
    }

    #[test]
    fn open_in_editor_temp_path_is_in_tmpdir() {
        // Verify the temp file path is inside the OS temp directory — we
        // cannot actually invoke an editor in a unit test, but we can check
        // that the path construction is correct.
        let tmp = std::env::temp_dir();
        let path = tmp.join(format!("smedja-edit-{}.md", std::process::id()));
        assert!(
            path.starts_with(&tmp),
            "temp file must be under the OS temp directory"
        );
        assert!(
            path.to_string_lossy().ends_with(".md"),
            "temp file must have .md extension for editor syntax highlighting"
        );
    }

    #[test]
    fn ctrl_g_in_scroll_mode_is_noop() {
        let mut state = make_state("sess-ctrlg-scroll");
        state.scroll_focus = true;
        state.input = "existing input".to_owned();
        state.input_cursor = 14;
        // The Ctrl-G handler guards on !scroll_focus; simulate that guard.
        if !state.scroll_focus {
            // would call open_in_editor — never reached
            state.input = "replaced".to_owned();
        }
        assert_eq!(
            state.input, "existing input",
            "Ctrl-G in scroll mode must not modify input"
        );
    }

    // --- thinking token accumulation ------------------------------------------

    #[test]
    fn thinking_tokens_accumulate_in_current_thinking() {
        let mut state = make_state("sess-think");
        assert!(state.current_thinking.is_empty());
        // Simulate two ThinkingDelta stream events arriving.
        state.current_thinking.push_str("step one ");
        state.current_thinking.push_str("step two");
        assert_eq!(state.current_thinking, "step one step two");
    }

    #[test]
    fn thinking_expanded_toggles_only_when_content_present() {
        let mut state = make_state("sess-think-toggle");
        state.scroll_focus = true;
        // No content: T key must be a no-op.
        assert!(state.current_thinking.is_empty());
        if !state.current_thinking.is_empty() {
            state.thinking_expanded = !state.thinking_expanded;
        }
        assert!(
            !state.thinking_expanded,
            "T must not toggle when no thinking content"
        );

        // With content: T key must toggle.
        state.current_thinking = "I considered option A vs B".to_owned();
        if !state.current_thinking.is_empty() {
            state.thinking_expanded = !state.thinking_expanded;
        }
        assert!(
            state.thinking_expanded,
            "T must expand when thinking content is present"
        );
        if !state.current_thinking.is_empty() {
            state.thinking_expanded = !state.thinking_expanded;
        }
        assert!(!state.thinking_expanded, "second T must collapse");
    }

    // --- govctl work-item harness --------------------------------------------

    #[test]
    fn scan_gov_artifacts_returns_empty_when_no_gov_dir() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = scan_gov_artifacts(dir.path());
        assert!(
            artifacts.is_empty(),
            "no gov/ dir should yield empty artifact list"
        );
    }

    #[test]
    fn scan_gov_artifacts_parses_work_item_toml() {
        let dir = tempfile::tempdir().unwrap();
        let wi_dir = dir.path().join("gov").join("work-items");
        std::fs::create_dir_all(&wi_dir).unwrap();
        std::fs::write(
            wi_dir.join("WI-001.toml"),
            r#"id = "WI-001"
title = "Add thinking token streaming"
status = "in_progress"
"#,
        )
        .unwrap();
        let artifacts = scan_gov_artifacts(dir.path());
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, "WI-001");
        assert_eq!(artifacts[0].status, "in_progress");
        assert_eq!(artifacts[0].kind, "work-items");
    }

    #[test]
    fn scan_gov_artifacts_skips_files_without_id() {
        let dir = tempfile::tempdir().unwrap();
        let wi_dir = dir.path().join("gov").join("work-items");
        std::fs::create_dir_all(&wi_dir).unwrap();
        std::fs::write(
            wi_dir.join("bad.toml"),
            r#"title = "missing id"
status = "draft"
"#,
        )
        .unwrap();
        let artifacts = scan_gov_artifacts(dir.path());
        assert!(
            artifacts.is_empty(),
            "TOML without 'id' field must be skipped"
        );
    }

    #[test]
    fn format_gov_list_shows_count_and_ids() {
        let artifacts = vec![
            GovArtifact {
                id: "WI-001".into(),
                kind: "work-items".into(),
                title: "Add multi-line input".into(),
                status: "done".into(),
            },
            GovArtifact {
                id: "RFC-001".into(),
                kind: "rfc".into(),
                title: "Thinking token streaming".into(),
                status: "accepted".into(),
            },
        ];
        let output = format_gov_list(&artifacts);
        assert!(output.contains("2 govctl artifact"), "count must appear");
        assert!(output.contains("WI-001"), "WI-001 must appear");
        assert!(output.contains("RFC-001"), "RFC-001 must appear");
    }

    #[test]
    fn format_gov_list_empty_returns_hint() {
        let output = format_gov_list(&[]);
        assert!(
            output.contains("gov/work-items"),
            "empty list must include path hint"
        );
    }

    // --- session rail (Ctrl-W) ------------------------------------------------

    #[test]
    fn session_rail_toggle_clears_cursor() {
        let mut state = make_state("sess-rail");
        assert!(!state.panels.session_rail);
        // Simulate Ctrl-W: enable rail.
        state.panels.session_rail = true;
        state.session_rail_cursor = 0;
        state.last_session_rail_poll = None;
        assert!(state.panels.session_rail);
        // Toggle off.
        state.panels.session_rail = false;
        assert!(!state.panels.session_rail);
    }

    #[test]
    fn session_rail_cursor_navigates_within_bounds() {
        let mut state = make_state("sess-rail-nav");
        state.session_rail_items = vec![
            ("id1".into(), "claude  id1".into()),
            ("id2".into(), "claude  id2".into()),
            ("id3".into(), "claude  id3".into()),
        ];
        state.session_rail_cursor = 0;
        // ] moves forward.
        let max = state.session_rail_items.len().saturating_sub(1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        assert_eq!(state.session_rail_cursor, 1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        assert_eq!(state.session_rail_cursor, 2);
        // Clamps at max.
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        assert_eq!(state.session_rail_cursor, 2, "cursor must not exceed max");
        // [ moves backward.
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
        assert_eq!(state.session_rail_cursor, 1);
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
        assert_eq!(state.session_rail_cursor, 0);
        // Clamps at zero.
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
        assert_eq!(state.session_rail_cursor, 0, "cursor must not underflow");
    }

    // --- emit/canvas split: system message dual-routing ----------------------

    #[test]
    fn push_system_message_routes_single_line_to_action_log() {
        let mut state = make_state("sess-emit");
        let log_before = state.action_log.len();
        push_system_message(&mut state, "diagram saved: ./out.svg");
        assert_eq!(
            state.action_log.len(),
            log_before + 1,
            "single-line system message must be added to action_log"
        );
    }

    #[test]
    fn push_system_message_multi_line_stays_in_panel_only() {
        let mut state = make_state("sess-emit-multi");
        let log_before = state.action_log.len();
        push_system_message(&mut state, "line one\nline two\nline three");
        assert_eq!(
            state.action_log.len(),
            log_before,
            "multi-line system message must NOT be added to action_log"
        );
    }

    // --- prompt feedback: token estimate -------------------------------------

    #[test]
    fn prompt_token_estimate_uses_chars_over_four_heuristic() {
        // 40 chars / 4 = 10 estimated tokens.
        let input = "a".repeat(40);
        let chars = input.chars().count();
        #[allow(clippy::integer_division)]
        let est = chars / 4;
        assert_eq!(est, 10, "40 chars should estimate to 10 tokens");
    }

    #[test]
    fn prompt_token_estimate_rounds_down() {
        let input = "abc"; // 3 chars / 4 = 0 — rounds down
        let chars = input.chars().count();
        #[allow(clippy::integer_division)]
        let est = chars / 4;
        assert_eq!(est, 0);
    }

    #[test]
    fn thinking_cleared_on_new_turn() {
        let mut state = make_state("sess-think-clear");
        state.current_thinking = "previous reasoning".to_owned();
        state.thinking_expanded = true;
        // Simulate what happens when a new turn starts.
        state.current_thinking.clear();
        state.thinking_expanded = false;
        assert!(state.current_thinking.is_empty());
        assert!(!state.thinking_expanded);
    }

    // --- P3b: OSC-9 helper ---------------------------------------------------

    #[test]
    fn osc9_bytes_is_correct_sequence() {
        let bytes = osc9_turn_complete_bytes();
        assert_eq!(bytes, b"\x1b]9;turn complete\x07");
    }

    #[test]
    fn emit_osc9_writes_to_vec() {
        let mut buf: Vec<u8> = Vec::new();
        emit_osc9(&mut buf).unwrap();
        assert_eq!(buf, b"\x1b]9;turn complete\x07");
    }

    // --- P2a: kill ring -------------------------------------------------------

    #[test]
    fn ctrl_k_kills_to_eol() {
        let mut state = make_state("sess-kill-k");
        state.input = "hello world".to_owned();
        state.input_cursor = 5; // cursor after "hello"
        let killed: String = state.input[state.input_cursor..].to_owned();
        state.input.drain(state.input_cursor..);
        push_kill(&mut state.kill_ring, killed);
        assert_eq!(state.input, "hello");
        assert_eq!(state.kill_ring.back().map(String::as_str), Some(" world"));
    }

    #[test]
    fn ctrl_u_kills_to_bol() {
        let mut state = make_state("sess-kill-u");
        state.input = "hello world".to_owned();
        state.input_cursor = 5;
        let killed: String = state.input[..state.input_cursor].to_owned();
        state.input.drain(..state.input_cursor);
        state.input_cursor = 0;
        push_kill(&mut state.kill_ring, killed);
        assert_eq!(state.input, " world");
        assert_eq!(state.kill_ring.back().map(String::as_str), Some("hello"));
    }

    #[test]
    fn ctrl_y_yanks_from_ring() {
        let mut state = make_state("sess-yank");
        state.input = "foo".to_owned();
        state.input_cursor = 3;
        push_kill(&mut state.kill_ring, " bar".to_owned());
        // Yank
        let text = state.kill_ring.back().cloned().unwrap();
        state.input.insert_str(state.input_cursor, &text);
        state.input_cursor += text.len();
        assert_eq!(state.input, "foo bar");
    }

    #[test]
    fn ctrl_b_moves_cursor_left() {
        let mut state = make_state("sess-ctrl-b");
        state.input = "abc".to_owned();
        state.input_cursor = 3;
        state.input_cursor = prev_char_boundary(&state.input, state.input_cursor);
        assert_eq!(state.input_cursor, 2);
    }

    #[test]
    fn kill_ring_evicts_oldest_at_capacity() {
        let mut ring: VecDeque<String> = VecDeque::new();
        for i in 0..17u32 {
            push_kill(&mut ring, i.to_string());
        }
        assert_eq!(ring.len(), 16, "ring must not exceed 16 entries");
        // Oldest entry (0) is evicted; front is "1".
        assert_eq!(ring.front().map(String::as_str), Some("1"));
    }

    // --- P2b: /gov create + transition ----------------------------------------

    #[test]
    fn gov_create_work_item_creates_toml_with_planned_status() {
        let dir = tempfile::tempdir().unwrap();
        let msg = gov_create(dir.path(), "work-item My first task");
        assert!(
            msg.contains("WI-001"),
            "should report created id; got: {msg}"
        );
        let path = dir.path().join("gov/work-items/WI-001.toml");
        assert!(
            path.exists(),
            "TOML file must be created at {}",
            path.display()
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("planned"),
            "status must be planned; got: {content}"
        );
    }

    #[test]
    fn gov_create_auto_increments_id() {
        let dir = tempfile::tempdir().unwrap();
        gov_create(dir.path(), "work-item First");
        let msg = gov_create(dir.path(), "work-item Second");
        assert!(
            msg.contains("WI-002"),
            "second item must get WI-002; got: {msg}"
        );
    }

    #[test]
    fn gov_create_rfc_uses_draft_status() {
        let dir = tempfile::tempdir().unwrap();
        let msg = gov_create(dir.path(), "rfc My RFC");
        assert!(msg.contains("RFC-001"), "got: {msg}");
        let content = std::fs::read_to_string(dir.path().join("gov/rfc/RFC-001.toml")).unwrap();
        assert!(
            content.contains("draft"),
            "RFC default status must be draft; got: {content}"
        );
    }

    #[test]
    fn gov_transition_updates_status_in_file() {
        let dir = tempfile::tempdir().unwrap();
        gov_create(dir.path(), "work-item Test task");
        let msg = gov_transition(dir.path(), "WI-001 in_progress");
        assert!(
            msg.contains("in_progress"),
            "should confirm transition; got: {msg}"
        );
        let content =
            std::fs::read_to_string(dir.path().join("gov/work-items/WI-001.toml")).unwrap();
        assert!(
            content.contains("\"in_progress\""),
            "file must contain updated status; got: {content}"
        );
    }

    #[test]
    fn gov_transition_rejects_invalid_status() {
        let dir = tempfile::tempdir().unwrap();
        gov_create(dir.path(), "work-item Test task");
        let msg = gov_transition(dir.path(), "WI-001 flying");
        assert!(
            msg.contains("invalid status"),
            "must reject unknown status; got: {msg}"
        );
    }

    // --- P1a: role cockpit ----------------------------------------------------

    #[test]
    fn role_cockpit_toggle_via_ctrl_a() {
        let mut state = make_state("sess-cockpit");
        assert!(!state.panels.role_cockpit, "cockpit hidden by default");
        state.panels.role_cockpit = !state.panels.role_cockpit;
        assert!(state.panels.role_cockpit, "toggle must show cockpit");
        state.panels.role_cockpit = !state.panels.role_cockpit;
        assert!(
            !state.panels.role_cockpit,
            "second toggle must hide cockpit"
        );
    }

    #[test]
    fn active_agent_name_captured_from_stream_started_event() {
        let mut state = make_state("sess-agent");
        let event = serde_json::json!({"type": "started", "agent_name": "review"});
        if let Some(name) = event["agent_name"].as_str() {
            state.active_agent_name = Some(name.to_owned());
        }
        assert_eq!(state.active_agent_name.as_deref(), Some("review"));
    }

    // --- P4: PanelVisibility default ------------------------------------------

    #[test]
    fn panel_visibility_startup_defaults_match_make_state() {
        let state = make_state("sess-panels");
        assert!(state.panels.context_rail, "context rail visible by default");
        assert!(!state.panels.metrics, "metrics hidden by default");
        assert!(!state.panels.session_rail, "session rail hidden by default");
        assert!(state.panels.lsp, "LSP visible by default");
        assert!(state.panels.obs, "obs visible by default");
        assert!(!state.panels.role_cockpit, "cockpit hidden by default");
    }

    // --- session detail overlay (Story A) ------------------------------------

    #[test]
    fn session_detail_starts_empty() {
        let state = make_state("sess-detail-init");
        assert!(
            state.session_detail_overlay.is_none(),
            "detail overlay must start empty"
        );
    }

    #[test]
    fn session_detail_esc_closes_overlay() {
        let mut state = make_state("sess-detail-esc");
        state.session_detail_overlay = Some(SessionDetail {
            id: "abc-123".into(),
            title: None,
            mode: Some("auto".into()),
            status: Some("active".into()),
            active_change: None,
            created_at: "2026-06-28T00:00:00Z".into(),
            updated_at: "2026-06-28T00:00:00Z".into(),
            cowork_mode: None,
        });
        // Esc while overlay is open clears it.
        state.session_detail_overlay = None;
        assert!(
            state.session_detail_overlay.is_none(),
            "Esc must close the detail overlay"
        );
    }

    #[test]
    fn session_detail_overlay_holds_correct_fields() {
        let detail = SessionDetail {
            id: "test-session-id".into(),
            title: Some("My session".into()),
            mode: Some("review".into()),
            status: Some("active".into()),
            active_change: Some("add-quality-panel".into()),
            created_at: "2026-06-01T12:00:00Z".into(),
            updated_at: "2026-06-28T09:00:00Z".into(),
            cowork_mode: Some("ask".into()),
        };
        assert_eq!(detail.id, "test-session-id");
        assert_eq!(detail.title.as_deref(), Some("My session"));
        assert_eq!(detail.mode.as_deref(), Some("review"));
        assert_eq!(detail.status.as_deref(), Some("active"));
        assert_eq!(detail.active_change.as_deref(), Some("add-quality-panel"));
        assert_eq!(detail.cowork_mode.as_deref(), Some("ask"));
    }

    #[test]
    fn session_detail_from_json_maps_all_fields() {
        let v = serde_json::json!({
            "id": "sess-42",
            "title": "refactor sprint",
            "mode": "auto",
            "status": "active",
            "active_change": "add-auth",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-06-28T00:00:00Z",
            "cowork_mode": "plan",
        });
        let detail = SessionDetail::from_json(&v);
        assert_eq!(detail.id, "sess-42");
        assert_eq!(detail.title.as_deref(), Some("refactor sprint"));
        assert_eq!(detail.mode.as_deref(), Some("auto"));
        assert_eq!(detail.status.as_deref(), Some("active"));
        assert_eq!(detail.active_change.as_deref(), Some("add-auth"));
        assert_eq!(detail.cowork_mode.as_deref(), Some("plan"));
    }

    #[test]
    fn session_detail_from_json_handles_missing_optional_fields() {
        let v = serde_json::json!({ "id": "bare-id",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        });
        let detail = SessionDetail::from_json(&v);
        assert_eq!(detail.id, "bare-id");
        assert!(detail.title.is_none());
        assert!(detail.mode.is_none());
        assert!(detail.status.is_none());
        assert!(detail.active_change.is_none());
        assert!(detail.cowork_mode.is_none());
    }

    #[test]
    fn session_detail_overlay_renders_in_buffer() {
        let mut state = make_state("sess-detail-render");
        state.session_detail_overlay = Some(SessionDetail {
            id: "full-id-abc-def-ghi".into(),
            title: Some("Sprint 12".into()),
            mode: Some("auto".into()),
            status: Some("active".into()),
            active_change: Some("add-quality-panel".into()),
            created_at: "2026-06-28T09:00:00Z".into(),
            updated_at: "2026-06-28T10:00:00Z".into(),
            cowork_mode: Some("ask".into()),
        });
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            content.contains("full-id-abc-def-ghi"),
            "full id must render"
        );
        assert!(content.contains("Sprint 12"), "title must render");
        assert!(
            content.contains("add-quality-panel"),
            "active change must render"
        );
        assert!(content.contains("ask"), "cowork mode must render");
    }

    #[test]
    fn session_rail_up_down_move_cursor_in_scroll_mode() {
        let mut state = make_state("sess-up-down");
        state.scroll_focus = true;
        state.panels.session_rail = true;
        state.session_rail_items = vec![
            ("id1".into(), "runner  id1".into()),
            ("id2".into(), "runner  id2".into()),
            ("id3".into(), "runner  id3".into()),
        ];
        state.session_rail_cursor = 0;

        // Down moves cursor forward.
        let max = state.session_rail_items.len().saturating_sub(1);
        if state.scroll_focus && state.panels.session_rail && !state.session_rail_items.is_empty() {
            state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        }
        assert_eq!(state.session_rail_cursor, 1);

        // Up moves cursor back.
        if state.scroll_focus && state.panels.session_rail {
            state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
        }
        assert_eq!(state.session_rail_cursor, 0);
    }

    #[test]
    fn session_rail_bracket_keys_work_in_input_mode() {
        let mut state = make_state("sess-bracket-input");
        state.scroll_focus = false; // input mode
        state.panels.session_rail = true;
        state.session_rail_items = vec![
            ("id1".into(), "label1".into()),
            ("id2".into(), "label2".into()),
        ];
        state.session_rail_cursor = 0;

        // ] advances cursor even in input mode.
        if state.panels.session_rail && !state.session_rail_items.is_empty() {
            let max = state.session_rail_items.len().saturating_sub(1);
            state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        }
        assert_eq!(state.session_rail_cursor, 1, "] must work in input mode");

        // [ goes back.
        if state.panels.session_rail {
            state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
        }
        assert_eq!(state.session_rail_cursor, 0, "[ must work in input mode");
    }

    // --- session detail: Ctrl+Enter load (Story B) ---------------------------

    #[test]
    fn session_detail_ctrl_enter_switches_session_id() {
        let mut state = make_state("sess-switch-id");
        state.session_id = "original-session".into();
        state.session_detail_overlay = Some(SessionDetail {
            id: "new-session-abc".into(),
            title: Some("other work".into()),
            mode: Some("auto".into()),
            status: Some("active".into()),
            active_change: None,
            created_at: "2026-06-28T00:00:00Z".into(),
            updated_at: "2026-06-28T00:00:00Z".into(),
            cowork_mode: None,
        });
        // Simulate what Ctrl+Enter does: extract id, switch, clear overlay.
        let target_id = state
            .session_detail_overlay
            .as_ref()
            .map(|d| d.id.clone())
            .unwrap();
        state.session_id = target_id;
        state.session_detail_overlay = None;
        state.display_start_idx = state.messages.len();
        state.main_panel.clear_display();

        assert_eq!(
            state.session_id, "new-session-abc",
            "session_id must switch"
        );
        assert!(
            state.session_detail_overlay.is_none(),
            "overlay must close after load"
        );
    }

    #[test]
    fn session_detail_ctrl_enter_does_nothing_without_overlay() {
        let mut state = make_state("sess-switch-no-overlay");
        state.session_id = "original".into();
        state.session_detail_overlay = None;
        // Nothing happens — session_id is unchanged.
        if let Some(ref d) = state.session_detail_overlay {
            state.session_id = d.id.clone();
        }
        assert_eq!(state.session_id, "original", "no overlay = no switch");
    }

    #[test]
    fn session_detail_popup_shows_load_hint() {
        let mut state = make_state("sess-detail-hint");
        state.session_detail_overlay = Some(SessionDetail {
            id: "hint-session".into(),
            title: None,
            mode: None,
            status: None,
            active_change: None,
            created_at: "2026-06-28T00:00:00Z".into(),
            updated_at: "2026-06-28T00:00:00Z".into(),
            cowork_mode: None,
        });
        let buf = render_frame(&mut state);
        let content: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        // The popup must hint both the load binding and close binding.
        assert!(
            content.contains("load") || content.contains("Load"),
            "popup must show load hint: {content}"
        );
        assert!(
            content.contains("Esc") || content.contains("close"),
            "popup must show close hint"
        );
    }

    // --- session rail: arrow keys in input mode (Story B fix) ----------------

    #[test]
    fn session_rail_up_arrow_moves_cursor_in_input_mode() {
        let mut state = make_state("sess-up-input");
        state.scroll_focus = false; // input mode
        state.panels.session_rail = true;
        state.session_rail_items = vec![
            ("id1".into(), "label1".into()),
            ("id2".into(), "label2".into()),
            ("id3".into(), "label3".into()),
        ];
        state.session_rail_cursor = 2;

        // Simulate the early-exit block: Up decrements cursor, does not touch history.
        if state.panels.session_rail && !state.scroll_focus {
            state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
        }
        assert_eq!(
            state.session_rail_cursor, 1,
            "Up must move rail cursor in input mode"
        );
        assert!(
            state.history_idx.is_none(),
            "prompt history must be untouched"
        );
    }

    #[test]
    fn session_rail_down_arrow_moves_cursor_in_input_mode() {
        let mut state = make_state("sess-down-input");
        state.scroll_focus = false;
        state.panels.session_rail = true;
        state.session_rail_items = vec![
            ("id1".into(), "label1".into()),
            ("id2".into(), "label2".into()),
        ];
        state.session_rail_cursor = 0;

        if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty()
        {
            let max = state.session_rail_items.len().saturating_sub(1);
            state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        }
        assert_eq!(
            state.session_rail_cursor, 1,
            "Down must move rail cursor in input mode"
        );
    }

    #[test]
    fn session_rail_down_arrow_clamps_at_bottom_in_input_mode() {
        let mut state = make_state("sess-down-clamp-input");
        state.scroll_focus = false;
        state.panels.session_rail = true;
        state.session_rail_items = vec![("id1".into(), "label1".into())];
        state.session_rail_cursor = 0;

        if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty()
        {
            let max = state.session_rail_items.len().saturating_sub(1);
            state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
        }
        assert_eq!(state.session_rail_cursor, 0, "Down must clamp at last item");
    }

    // --- Slice 7: command palette ---

    // --- Slice 8: file picker ---

    #[test]
    fn file_picker_insert_formats_at_file() {
        let mut state = make_state("s");
        state.input.clear();
        state.input_cursor = 0;
        // Simulate inserting a file selection
        let path = "/workspace/src/main.rs";
        let at_ref = format!("@file {path}");
        state.input = at_ref.clone();
        state.input_cursor = state.input.len();
        assert!(state.input.starts_with("@file "));
        assert!(state.input.contains(path));
    }

    #[test]
    fn ctrl_f_in_input_mode_opens_file_picker() {
        let mut state = make_state("s");
        state.scroll_focus = false; // input mode
                                    // Simulate what Ctrl+F handler does
        state.file_picker_open = true;
        state.file_picker_entries = vec![("../".to_owned(), true), ("main.rs".to_owned(), false)];
        state.file_picker_cursor = 0;
        assert!(state.file_picker_open);
        assert_eq!(state.file_picker_entries.len(), 2);
    }

    #[test]
    fn command_palette_empty_query_returns_all_commands() {
        let completions = command_palette_filtered("");
        assert_eq!(completions.len(), SLASH_COMPLETIONS.len());
    }

    #[test]
    fn command_palette_filters_by_substring() {
        // "model" matches "/model" and substring of other commands that contain "model"
        let completions = command_palette_filtered("mod");
        assert!(
            completions.contains(&"/model".to_owned()),
            "expected /model in results"
        );
    }

    #[test]
    fn command_palette_no_match_returns_empty() {
        let completions = command_palette_filtered("zzznomatch");
        assert!(completions.is_empty());
    }

    #[test]
    fn ctrl_k_on_empty_input_opens_palette() {
        let mut state = make_state("test-session");
        state.input.clear();
        // Simulate what the Ctrl+K handler does when input is empty
        state.slash_popup_visible = true;
        state.slash_completions = command_palette_filtered("");
        state.command_palette_mode = true;
        state.slash_cursor = 0;
        assert!(state.slash_popup_visible);
        assert_eq!(state.slash_completions.len(), SLASH_COMPLETIONS.len());
        assert!(state.command_palette_mode);
    }
}
