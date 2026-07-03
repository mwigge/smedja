pub mod action_log;
mod blocks;
mod capabilities;
mod clipboard;
pub mod code_widget;
mod commands;
mod completion;
mod context_rail;
mod cowork;
mod cowork_widget;
mod diff_viewer;
mod editor;
mod formatting;
mod generators;
mod governance;
mod history;
mod input;
mod lsp_panel;
pub mod main_panel;
mod messages;
mod metrics_poll;
mod metrics_view;
mod obs_panel;
mod plan_panel;
mod quality_panel;
mod render;
mod secrets;
mod session;
pub(crate) mod slash;
mod slash_fmt;
mod staging;
mod state;
mod statusbar;
mod statusline;
mod submit;
mod terminal_guard;
#[cfg(test)]
mod testutil;
pub mod theme;
mod thoughts_panel;
mod tool_call;
mod upgrade;
mod value_panel;

// Re-export extracted-module items at the crate root so `main` and the retained
// test module (`use super::*`) continue to resolve the moved names unchanged.
#[allow(unused_imports)]
pub(crate) use capabilities::{
    format_capabilities_table, runner_is_subprocess, runner_supports_thinking,
};
#[allow(unused_imports)]
pub(crate) use commands::{
    command_palette_filtered, filtered_completions, parse_review_scope, render_findings_summary,
    HELP_TEXT, SLASH_COMMAND_DESCRIPTIONS, SLASH_COMPLETIONS,
};
#[allow(unused_imports)]
pub(crate) use completion::{
    accept_slash_completion, clear_slash_popup, list_dir_entries, open_file_picker,
};
#[allow(unused_imports)]
pub(crate) use cowork::{apply_cowork_decision, cowork_resolved, resolve_cowork};
#[allow(unused_imports)]
pub(crate) use generators::{save_generator_output, slugify, OutputType};
#[allow(unused_imports)]
pub(crate) use history::{
    dirs_home, dirs_tui_history_path, large_paste_token, load_history, save_history,
    HISTORY_FILE_CAP, LARGE_PASTE_THRESHOLD, PROMPT_HISTORY_CAP,
};
#[allow(unused_imports)]
pub(crate) use input::handle_key;
#[allow(unused_imports)]
pub(crate) use messages::{
    author_chip, format_tool_detail, push_action_log, push_author_chip, push_system_message,
};
#[allow(unused_imports)]
pub(crate) use metrics_poll::{
    format_token_count, lsp_snapshot_from_rpc, metrics_poll_due, metrics_rows_from_summary,
    toggle_metrics_view, METRICS_POLL_INTERVAL, METRICS_SINCE_WINDOW_MICROS,
};
#[allow(unused_imports)]
pub(crate) use render::{
    render, render_file_picker, render_role_cockpit, render_session_detail, render_session_peek,
    render_slash_popup, INPUT_MAX_ROWS,
};
#[allow(unused_imports)]
pub(crate) use session::{
    format_resume_rows, parse_resume_args, replay_history, resolve_session,
    resume_blocked_by_pending_turn, resume_into_view, resume_plan, session_start_decision,
    socket_path, start_stream_reader, stream_socket_path, ResolvedSession, ResumePlan,
    SessionDetail, SessionStart, LATENCY_SAMPLE_CAP,
};
#[allow(unused_imports)]
pub(crate) use state::{AppState, InputMode, Message, PanelVisibility, Role};
#[allow(unused_imports)]
pub(crate) use statusline::{status_bar_line, status_hint_line};
#[allow(unused_imports)]
pub(crate) use submit::submit;

// Re-export slash module items so that `use super::*` in the test module
// continues to find them without change.  The `#[allow(unused_imports)]` is
// needed because the compiler does not see the indirect usage via `use super::*`
// in the test module.
#[allow(unused_imports)]
pub(crate) use slash::{apply_agent, apply_tier, dispatch_slash};
#[allow(unused_imports)]
pub(crate) use slash_fmt::{
    format_agents_table, format_approvals_list, format_local_model_list, format_metrics,
    format_model_list,
};

// Re-export extracted module items so callers (slash.rs, tests) see them
// at the crate root unchanged.
#[allow(unused_imports)]
pub(crate) use clipboard::{
    emit_turn_notifications, osc9_turn_complete_bytes, paste_from_clipboard, push_kill,
    set_terminal_title, yank_to_clipboard,
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
pub(crate) use tool_call::{tool_call_card, tool_glyph_label};
#[allow(unused_imports)]
pub(crate) use upgrade::{
    fetch_latest_version, format_openspec_list, format_openspec_status, is_newer, run_openspec,
    run_upgrade, VERSION,
};

use std::collections::VecDeque;
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    EnableBracketedPaste, EnableMouseCapture, Event, KeyboardEnhancementFlags, MouseEventKind,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use crossterm::{event, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::{json, Value};
use smedja_bellows::StreamEvent;
use smedja_rpc::client::Client;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "smedja-tui", version, about = "smedja agent dashboard (TUI)")]
pub(crate) struct Cli {
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

// ---------------------------------------------------------------------------
// Socket path resolution
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Streaming turn reader
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Session bootstrap (create vs. resume)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Submit a user turn to the daemon
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Slash completion helpers
// ---------------------------------------------------------------------------

/// Classifies an LLM turn error message into a short label and optional hint.
///
/// Returns `(label, hint)` where `hint` is empty when there is nothing useful
/// to suggest.  The label is used to prefix the displayed error line.
use formatting::{classify_turn_error, format_turn_error};

// dispatch_slash, apply_tier, apply_agent, and their exclusive format helpers
// (format_model_list, format_local_model_list, format_agents_table,
// format_metrics, format_approvals_list) have been extracted to src/slash.rs.
// They are re-exported at the top of this file via `pub(crate) use slash::...`
// so callers and the test module (which uses `use super::*`) see them unchanged.

// ---------------------------------------------------------------------------
// Cowork resolver
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Key handler
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Resolves the TUI log file path: `$XDG_STATE_HOME/smedja/smedja-tui.log`
/// (falling back to `~/.local/state/smedja/`). Creates the directory.
pub(crate) fn tui_log_path() -> Option<PathBuf> {
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
pub(crate) fn init_tracing() {
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
pub(crate) fn load_tui_colors() -> Option<crate::theme::TuiColorConfig> {
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
    // Load the persisted TUI prompt history from disk.
    let tui_history_path = dirs_tui_history_path();
    let loaded_prompt_history = load_history(&tui_history_path);
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
        prompt_history: loaded_prompt_history,
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
        show_session_peek: false,
        session_browser_open: false,
        session_browser_cursor: 0,
        vim_input_mode: InputMode::Insert,
        pending_vim_key: None,
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

    // Seed context window so the obs panel shows a real % on the first frame.
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
                        // Large pastes are saved to a temp file to avoid
                        // flooding the input bar; a short token is substituted.
                        let insert_text = if let Some((token, msg)) = large_paste_token(&text) {
                            push_system_message(&mut state, msg);
                            token
                        } else {
                            text
                        };
                        // Insert the whole paste as a single edit at the cursor.
                        // Because we don't process it key-by-key, embedded
                        // newlines stay literal (no accidental submit) — pasting
                        // a multi-line URL/snippet just lands in the input.
                        let cur = state.input_cursor.min(state.input.len());
                        state.input.insert_str(cur, &insert_text);
                        state.input_cursor = cur + insert_text.len();
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
        let _ = set_terminal_title(&mut stdout(), state.turn_in_flight, state.spinner_tick);

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
                        // Scan accumulated turn text for new numbered plan steps.
                        if let Some(ref block) = state.current_block {
                            let new = plan_panel::extract_new_steps(
                                &block.content,
                                state.plan_steps.len(),
                            );
                            state.plan_steps.extend(new);
                        }
                    }
                    StreamEvent::Started { agent_name } => {
                        if let Some(name) = agent_name {
                            state.active_agent_name = Some(name);
                        }
                    }
                    StreamEvent::Thinking { text } => {
                        state.current_thinking.push_str(&text);
                        let elapsed_s = state
                            .turn_submitted_at
                            .map_or(0.0, |t| t.elapsed().as_secs_f32());
                        // Merge consecutive reasoning into the last step.
                        if let Some(thoughts_panel::ThinkingStep::Reasoning {
                            text: ref mut t,
                            ..
                        }) = state.thinking_steps.last_mut()
                        {
                            t.push_str(&text);
                        } else {
                            state
                                .thinking_steps
                                .push(thoughts_panel::ThinkingStep::Reasoning {
                                    text: text.clone(),
                                    elapsed_s,
                                });
                        }
                    }
                    StreamEvent::ToolCall { name, input, full } => {
                        let full_str = full.as_deref().unwrap_or(&input);
                        let elapsed_s = state
                            .turn_submitted_at
                            .map_or(0.0, |t| t.elapsed().as_secs_f32());
                        state
                            .thinking_steps
                            .push(thoughts_panel::ThinkingStep::Tool {
                                name: name.clone(),
                                preview: input.chars().take(60).collect(),
                                elapsed_s,
                            });
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
                        let elapsed_s = state
                            .turn_submitted_at
                            .map_or(0.0, |start| start.elapsed().as_secs_f32());
                        state
                            .thinking_steps
                            .push(thoughts_panel::ThinkingStep::Answer { elapsed_s });
                        // Any tool still marked "running" at turn end is settled.
                        if let Some((idx, name, inp)) = state.pending_tool.take() {
                            let card = tool_call_card(&name, &inp, state.no_color, '\u{2713}');
                            state.main_panel.replace_styled_line(idx, card);
                        }
                        let output_tok = u64::from(output_tok);
                        let input_tok = u64::from(input_tok.unwrap_or(0));
                        let turn_ms = state.turn_submitted_at.map_or(0, |inst| {
                            u64::try_from(inst.elapsed().as_millis()).unwrap_or(u64::MAX)
                        });
                        state.turn_submitted_at = None;
                        state.last_traceparent.clone_from(&traceparent);

                        // Track latency samples for p95/p99 in the obs panel.
                        if turn_ms > 0 {
                            if state.latency_samples.len() >= LATENCY_SAMPLE_CAP {
                                state.latency_samples.pop_front();
                            }
                            state.latency_samples.push_back(turn_ms);
                            state.obs_snapshot.latency_samples = state.latency_samples.clone();
                        }
                        // Accumulate session token totals.
                        state.session_tokens_in = state.session_tokens_in.saturating_add(input_tok);
                        state.session_tokens_out =
                            state.session_tokens_out.saturating_add(output_tok);
                        state.obs_snapshot.tokens_input = state.session_tokens_in;
                        state.obs_snapshot.tokens_output = state.session_tokens_out;

                        let block_content = if let Some(mut block) = state.current_block.take() {
                            block.complete(turn_ms);
                            let content = block.content.clone();
                            state.block_store.push(block);
                            content
                        } else {
                            String::new()
                        };

                        let footer = if let Some(ref tp_str) = traceparent {
                            if state.otlp_configured {
                                format!("↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str}")
                            } else {
                                format!(
                                    "↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str} · traces not exported (set SMEDJA_OTLP_ENDPOINT)"
                                )
                            }
                        } else {
                            format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms")
                        };
                        state.main_panel.push_line(footer);

                        // Emit a collapsible trace badge when thinking steps or tool
                        // calls were recorded — meaningful for all providers.
                        let n_steps = state.thinking_steps.len();
                        if n_steps > 1 {
                            let label = if state.thinking_steps.iter().any(|s| {
                                matches!(s, thoughts_panel::ThinkingStep::Reasoning { .. })
                            }) {
                                "thinking"
                            } else {
                                "trace"
                            };
                            state.main_panel.push_line(format!(
                                "\u{254c} {label} ({n_steps} steps) [T to expand] \u{254c}"
                            ));
                        }

                        if let Some(output_type) = state.pending_output_type.take() {
                            pending_output_save = Some((output_type, block_content));
                        }

                        let _ = emit_turn_notifications(&mut std::io::stdout());

                        turn_done = true;
                    }
                    StreamEvent::Error { message } => {
                        let (label, hint) = classify_turn_error(&message);
                        let header = format_turn_error(&state.runner, label, &message);
                        let display = if hint.is_empty() {
                            header
                        } else {
                            format!("{header}\n  \u{2192} {hint}")
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
                    StreamEvent::CoworkRequest {
                        approval_id,
                        tool,
                        step_n,
                        args_display,
                        reasoning,
                    } => {
                        let already_known =
                            state.pending_cowork.iter().any(|i| i.id == approval_id);
                        if !already_known {
                            state.pending_cowork.push(cowork_widget::CoworkItem {
                                id: approval_id,
                                tool,
                                step_n,
                                args_display,
                                reasoning,
                            });
                        }
                    }
                    StreamEvent::HistoryReplaced { summary_tokens } => {
                        state.main_panel.push_seam(summary_tokens);
                    }
                    StreamEvent::Unknown => {
                        tracing::debug!(
                            "received unknown stream event type — daemon may be newer than TUI"
                        );
                    }
                    StreamEvent::Usage { .. } | StreamEvent::ToolCallChunk { .. } => {}
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
                                format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms");
                            state.main_panel.push_line(footer);
                            state.last_traceparent = None;
                            let in_u64 = u64::try_from(input_tok.max(0)).unwrap_or(0);
                            let out_u64 = u64::try_from(output_tok.max(0)).unwrap_or(0);
                            state.session_tokens_in =
                                state.session_tokens_in.saturating_add(in_u64);
                            state.session_tokens_out =
                                state.session_tokens_out.saturating_add(out_u64);
                            state.obs_snapshot.tokens_input = state.session_tokens_in;
                            state.obs_snapshot.tokens_output = state.session_tokens_out;
                        }
                        state.pending_task_id = None;
                        state.last_poll = None;
                        state.turn_in_flight = false;
                        state.poll_retry_count = 0;
                        let _ = emit_turn_notifications(&mut std::io::stdout());

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
                // Fallback: if no persisted buckets yet, show current-session totals.
                if state.metrics_snapshot.is_empty()
                    && (state.session_tokens_in > 0 || state.session_tokens_out > 0)
                {
                    let total = state
                        .session_tokens_in
                        .saturating_add(state.session_tokens_out);
                    state.metrics_snapshot = vec![metrics_view::MetricsRow {
                        runner: state.runner.clone(),
                        tokens: i64::try_from(total).unwrap_or(i64::MAX),
                        cost_usd: 0.0,
                        errors: 0,
                    }];
                }
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
                state.value_snapshot.cost_usd_micros = vc["cost_usd_micros"].as_u64().unwrap_or(0);
                state.value_snapshot.quality_avg = state.quality_snapshot.score;
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
    // M5a: persist TUI prompt history to ~/.config/smedja/tui-history.jsonl.
    let _ = save_history(&state.prompt_history, &tui_history_path);

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
pub(crate) async fn try_reconnect(sock: &std::path::Path) -> Option<Client> {
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

// ---------------------------------------------------------------------------
// M5 — Prompt history persistence + large-paste protection
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// M5a — Prompt history persistence
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// M5b — Large-paste SHA-8 placeholder
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests (L128, L129)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};

    // --- metrics-live-fetch: pure JSON→rows mapper ---

    // --- metrics-live-fetch: poll-due predicate ---

    // --- metrics-live-fetch: toggle resets the poll cadence ---

    // --- metrics-live-fetch: live fetch populates/clears the snapshot ---

    // --- /review scope-flag parsing ---

    // --- /review findings summary rendering ---

    // L128: trailing backslash appends newline continuation, does not submit.

    // L128: continuation display prefix uses "..." for multi-line input.

    // L128: normal input display uses "> " prefix.

    // L129: filtered_completions returns only matching entries.

    // L129: typing "/" returns all completions.

    // L129: unknown prefix returns empty.

    // -----------------------------------------------------------------------
    // Session resume — startup routing, replay, picker, rollback
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Layer 4 TUI functional tests — TestBackend (no network)
    // -----------------------------------------------------------------------

    // push_delta accumulated via the panel renders into the frame buffer.

    // --- connect banner tests ---

    // --- thinking indicator tests ---

    // --- layout regression tests ---

    // handle_key with "/health" + Enter calls session.get and writes latency to panel.

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

    // ── provider-display: session.create response parsing ───────────────────

    // ── tui-message-selection: T6 tests ─────────────────────────────────────

    // --- OTel footer guidance tests ---

    /// Build the footer string the same way the streaming `done` handler does,
    /// so the unit test does not depend on a live event loop.
    fn build_turn_footer(
        input_tok: u64,
        output_tok: u64,
        turn_ms: u64,
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
            format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms")
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

    // --- tui-input-modes tests ---

    // --- tui-prompt-history tests ---

    // --- tui-spec-command tests ---

    // --- cowork resolver helper ---

    // --- cowork decision application (approve / deny) ---

    // --- cowork modify flow ---

    // --- lsp_snapshot_from_rpc -----------------------------------------------

    // --- detect_project_types ------------------------------------------------

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

    // --- Ctrl-G external editor --------------------------------------------------

    // --- thinking token accumulation ------------------------------------------

    // --- thinking step timeline ----------------------------------------------

    // --- govctl work-item harness --------------------------------------------

    // --- session rail (Ctrl-W) ------------------------------------------------

    // --- emit/canvas split: system message dual-routing ----------------------

    // --- prompt feedback: token estimate -------------------------------------

    // --- M1: notification helpers -------------------------------------------

    // --- P2a: kill ring -------------------------------------------------------

    // --- P2b: /gov create + transition ----------------------------------------

    // --- P1a: role cockpit ----------------------------------------------------

    // --- P4: PanelVisibility default ------------------------------------------

    // --- session detail overlay (Story A) ------------------------------------

    // --- session detail: Ctrl+Enter load (Story B) ---------------------------

    // --- session rail: arrow keys in input mode (Story B fix) ----------------

    // --- Slice 7: command palette ---

    // --- Slice 8: file picker ---

    // -------------------------------------------------------------------------
    // M5 — History persistence and large-paste protection
    // -------------------------------------------------------------------------

    // -------------------------------------------------------------------------
    // M10 — Vim normal mode for prompt input
    // -------------------------------------------------------------------------

    // -------------------------------------------------------------------------
    // M7 — Session browser overlay
    // -------------------------------------------------------------------------

    // -------------------------------------------------------------------------
    // M5 — History persistence and large-paste protection
    // -------------------------------------------------------------------------
}
