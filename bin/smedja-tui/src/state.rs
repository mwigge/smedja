use crate::generators::OutputType;
use crate::session::SessionDetail;
use crate::{
    action_log, blocks, cowork_widget, main_panel, metrics_view, obs_panel, quality_panel, staging,
    thoughts_panel, value_panel,
};
use smedja_bellows::StreamEvent;
use std::collections::VecDeque;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) enum Role {
    User,
    System,
}

#[derive(Debug, Clone)]
pub(crate) struct Message {
    #[allow(dead_code)]
    // role field drives future rendering distinction; suppressed until render is split
    pub(crate) role: Role,
    pub(crate) text: String,
}

///
/// Grouped here so new panels only require adding one field instead of
/// threading a top-level boolean through `AppState` and every test helper.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct PanelVisibility {
    /// Context rail (right, Ctrl-F).
    pub(crate) context_rail: bool,
    /// Metrics view overlay (Ctrl-T).
    pub(crate) metrics: bool,
    /// Session browser left-rail (Ctrl-W).
    pub(crate) session_rail: bool,
    /// LSP diagnostic panel (right rail, Ctrl-L).
    pub(crate) lsp: bool,
    /// Observability panel (right rail, Ctrl-O).
    pub(crate) obs: bool,
    /// Role cockpit panel (right rail, Ctrl-A).
    pub(crate) role_cockpit: bool,
    /// Quality gate panel (right rail, Ctrl-Q).
    pub(crate) quality: bool,
    /// Value / ROI panel (right rail, Ctrl-V).
    pub(crate) value: bool,
}

/// Prompt input editing mode — Emacs-style (Insert) or Vim Normal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputMode {
    /// Normal text editing mode (default); keys type characters.
    Insert,
    /// Vim normal mode; motion and editing keys are active.
    Normal,
}

#[allow(clippy::struct_excessive_bools)] // AppState is a TUI dispatch table; enum-splitting would add indirection without clarity
#[derive(Debug)]
pub(crate) struct AppState {
    pub(crate) session_id: String,
    pub(crate) mode: Option<String>,
    pub(crate) tier: Option<String>,
    pub(crate) runner: String,
    pub(crate) model: Option<String>,
    pub(crate) messages: Vec<Message>,
    pub(crate) input: String,
    pub(crate) quit: bool,
    /// True after one Ctrl-C with an empty input — a second consecutive Ctrl-C
    /// confirms quit. Reset by any other key so quitting is always deliberate.
    pub(crate) quit_armed: bool,
    /// Current permission mode (`ask`/`accept_edits`/`plan`/`auto`), cycled with
    /// Shift+Tab via `cowork.set_mode` and shown in the status bar.
    pub(crate) permission_mode: String,
    /// Workspace whose code-graph status the right-bar reflects — the last
    /// `/index <path>`, falling back to the TUI's cwd.
    pub(crate) graph_workspace: Option<String>,
    /// Symbol count from the last `/index` this session (`None` = not indexed
    /// here yet). Surfaced as a code-graph status under the LSP panel.
    pub(crate) graph_symbols: Option<usize>,
    /// Tool-call detail log: `(card_line_index, tool_name, full_input)`. Backs
    /// right-click expansion of a tool card and the `/tools` inspector.
    pub(crate) tool_details: Vec<(usize, String, String)>,
    /// The currently-running tool card awaiting its result: `(line, name,
    /// input_summary)`. Resolved to ✓/✗ when the result arrives.
    pub(crate) pending_tool: Option<(usize, String, String)>,
    /// When `Some(env_var)`, the input bar is in masked secret-entry mode (e.g.
    /// pasting an API key during login). Input renders as dots and Enter saves
    /// the value to the secrets file under this env-var name instead of sending
    /// a turn. `Esc` cancels.
    pub(crate) secret_var: Option<String>,
    /// Task ID of an in-flight turn being polled for a response.
    pub(crate) pending_task_id: Option<String>,
    /// Timestamp of the last poll attempt.
    pub(crate) last_poll: Option<std::time::Instant>,
    /// Monotonically increasing turn counter.
    pub(crate) turn_n: u32,
    /// Timestamp when the current turn was submitted (used to compute `turn_ms`).
    pub(crate) turn_submitted_at: Option<std::time::Instant>,
    /// The turn block being assembled for the current in-flight turn.
    pub(crate) current_block: Option<blocks::TurnBlock>,
    /// Completed turn block history.
    pub(crate) block_store: blocks::BlockStore,
    /// Whether the block browser overlay is open.
    pub(crate) block_browser_open: bool,
    /// Cursor position within the block browser.
    pub(crate) block_browser_cursor: usize,
    /// In-memory clipboard (no system clipboard).
    pub(crate) clipboard: Option<String>,
    /// Full diff overlay: (`tool_entry_idx`, `diff_lines`).
    pub(crate) diff_overlay: Option<(usize, Vec<String>)>,
    /// Scroll offset within the diff overlay.
    pub(crate) diff_scroll: usize,
    /// When true, the diff overlay renders in side-by-side split mode.
    pub(crate) diff_split_view: bool,
    /// Staging queue for batched tool dispatch.
    pub(crate) staging_queue: staging::StagingQueue,
    /// Visibility state for all toggleable rail and overlay panels.
    pub(crate) panels: PanelVisibility,
    /// Cached per-runner metrics snapshot for the latest rollup window.
    pub(crate) metrics_snapshot: Vec<metrics_view::MetricsRow>,
    /// Cached token-economy savings snapshot for the latest rollup window.
    pub(crate) savings_snapshot: metrics_view::SavingsSnapshot,
    /// Timestamp of the last metrics panel poll (drives both the `metrics.summary`
    /// per-runner fetch and the `savings.summary` token-economy fetch on one
    /// cadence). `None` forces an immediate fetch on the next tick.
    pub(crate) last_metrics_poll: Option<std::time::Instant>,
    /// Timestamp of the last obs-panel poll (session.cost + daily token total).
    /// Independent of `panels.metrics` so the obs panel is always current.
    pub(crate) last_obs_poll: Option<std::time::Instant>,
    /// Cumulative tokens used so far in this session (input + output).
    pub(crate) context_used: u64,
    /// Context window size in tokens for the active model.
    pub(crate) context_window: u64,
    /// Main message display panel.
    pub(crate) main_panel: main_panel::MainPanel,
    /// Audit action log widget.
    pub(crate) action_log: action_log::ActionLog,
    /// Available slash-command completions (filtered subset of `SLASH_COMPLETIONS`, or dynamic runner list).
    pub(crate) slash_completions: Vec<String>,
    /// Whether the slash-command completion popup is visible.
    pub(crate) slash_popup_visible: bool,
    /// Cursor index within the filtered completion list.
    pub(crate) slash_cursor: usize,
    /// True when the popup is showing a runner picker (Enter confirms runner switch).
    pub(crate) runner_picker_mode: bool,
    /// True when the popup is showing a session picker (Enter resumes the highlighted session).
    pub(crate) session_picker_mode: bool,
    /// True when the popup is the Ctrl+K command palette (fuzzy filter, wider, shows descriptions).
    pub(crate) command_palette_mode: bool,
    /// True while the Ctrl+F file picker overlay is open.
    pub(crate) file_picker_open: bool,
    /// Current directory being browsed in the file picker.
    pub(crate) file_picker_dir: std::path::PathBuf,
    /// Entries in the current directory: (display-name, `is_dir`).
    pub(crate) file_picker_entries: Vec<(String, bool)>,
    /// Cursor index within `file_picker_entries`.
    pub(crate) file_picker_cursor: usize,
    /// Session ids parallel to `slash_completions` while the session picker is open.
    pub(crate) session_picker_ids: Vec<String>,
    /// Sessions shown in the left rail: (id, label) pairs.
    pub(crate) session_rail_items: Vec<(String, String)>,
    /// Cursor row within the session rail.
    pub(crate) session_rail_cursor: usize,
    /// Timestamp of the last session rail refresh.
    pub(crate) last_session_rail_poll: Option<std::time::Instant>,
    /// Detail overlay opened by pressing Enter on a session rail item.
    pub(crate) session_detail_overlay: Option<SessionDetail>,
    /// True while a turn is awaiting a streaming response.
    pub(crate) turn_in_flight: bool,
    /// True once the assistant author chip + fresh line for the current turn have
    /// been emitted, so streamed deltas land on their own line (not merged into
    /// the preceding "queued"/user line) and the chip is shown exactly once.
    pub(crate) assistant_open: bool,
    /// Number of consecutive unexpected (non-done) poll responses received.
    ///
    /// Used to rate-limit the "waiting for turn…" status message so it does not
    /// flood the panel on rapid retries.
    pub(crate) poll_retry_count: u32,
    /// Whether the messages panel has scroll focus (input bar is inactive).
    pub(crate) scroll_focus: bool,
    /// Whether selection mode is active within the messages panel.
    pub(crate) selection_mode: bool,
    /// Anchor `(line, char_col)` of the current selection.
    pub(crate) selection_anchor: (usize, usize),
    /// Moving end `(line, char_col)` of the current selection.
    pub(crate) selection_end: (usize, usize),
    /// First `g` press received; waiting for a second `g` to jump to top.
    pub(crate) g_pending: bool,
    /// Byte offset of the insertion cursor within `input`.
    /// Invariant: always on a UTF-8 char boundary, 0 ≤ cursor ≤ `input.len()`.
    pub(crate) input_cursor: usize,
    /// Pending cowork approvals waiting for a decision.
    pub(crate) pending_cowork: Vec<cowork_widget::CoworkItem>,
    /// True when the user pressed `m` to enter modify-instruction mode.
    pub(crate) cowork_modify_mode: bool,
    /// Current content of the modify instruction input.
    pub(crate) cowork_modify_input: String,
    /// Timestamp of the last `graph.status` poll (refreshes the right-bar count).
    pub(crate) last_graph_poll: Option<std::time::Instant>,
    /// NDJSON stream receiver for the current in-flight turn.
    pub(crate) stream_rx: Option<tokio::sync::mpsc::UnboundedReceiver<StreamEvent>>,
    /// Oneshot receiver for a background /upgrade operation.
    pub(crate) upgrade_rx: Option<tokio::sync::oneshot::Receiver<String>>,
    /// Accumulated thinking-token text for the current in-flight turn.
    ///
    /// Reset to empty at the start of each new turn. Rendered as a dim
    /// collapsible block while the turn is in flight; summarised as a
    /// single-line badge once the turn completes.
    pub(crate) current_thinking: String,
    /// Ordered steps accumulated during the current turn for the timeline overlay.
    pub(crate) thinking_steps: Vec<thoughts_panel::ThinkingStep>,
    /// Whether the completed thinking block is expanded in the panel.
    pub(crate) thinking_expanded: bool,
    /// Kill ring for Ctrl-K / Ctrl-U / Ctrl-Y input editing (max 16 entries).
    pub(crate) kill_ring: VecDeque<String>,
    /// Name of the agent/role active in the current in-flight turn (from `CorrelationCtx`).
    pub(crate) active_agent_name: Option<String>,
    /// Path of the smdjad stream socket (`<rpc_sock>.stream`).
    pub(crate) stream_sock_path: PathBuf,
    /// W3C traceparent from the most recently completed turn.
    pub(crate) last_traceparent: Option<String>,
    /// Pending structured output type for generator commands (/drawio, /pptx).
    pub(crate) pending_output_type: Option<OutputType>,
    /// True when `SMEDJA_OTLP_ENDPOINT` is set in the environment at startup.
    pub(crate) otlp_configured: bool,
    /// Disable all colours when `NO_COLOR` is set in the environment.
    pub(crate) no_color: bool,
    /// Braille spinner frame counter; advances each render tick while a turn is in flight.
    pub(crate) spinner_tick: u8,
    /// Whether '/' panel search is active (intercepts keys to refine the query).
    pub(crate) panel_search_mode: bool,
    /// Current search query string (highlights matching panel lines while non-empty).
    pub(crate) panel_search_query: String,
    /// Watermark index into `messages`; messages before this index are not
    /// re-displayed after a `/clear`.
    pub(crate) display_start_idx: usize,
    /// Ordered list of submitted prompts for Up/Down history browsing.
    pub(crate) prompt_history: Vec<String>,
    /// Current browse position within `prompt_history` (`None` = live input).
    pub(crate) history_idx: Option<usize>,
    /// Input saved before history browsing started; restored when browsing past the end.
    pub(crate) saved_input: String,
    /// True while reverse history search (Ctrl-R in input mode) is active.
    pub(crate) history_search_mode: bool,
    /// Query string for the active reverse history search.
    pub(crate) history_search_query: String,
    /// Resolved path to the `openspec` binary, or `None` if not installed.
    pub(crate) openspec_bin: Option<PathBuf>,
    /// Instant of last `lsp.status` / `lsp.diagnostics` RPC poll; `None` before first poll.
    pub(crate) lsp_last_poll: Option<std::time::Instant>,
    /// Most recent LSP snapshot (updated from RPC polls every 5 s).
    pub(crate) lsp_snapshot: smedja_lsp::LspSnapshot,
    /// Observability snapshot — updated from turn events + metrics polls.
    pub(crate) obs_snapshot: obs_panel::ObsSnapshot,
    /// Quality gate snapshot — updated on each `TurnEvent::QualitySnapshot`.
    pub(crate) quality_snapshot: quality_panel::QualitySnapshot,
    /// Plan steps extracted from the current turn's streaming response.
    /// Reset at the start of each new turn.
    pub(crate) plan_steps: Vec<String>,
    /// Consecutive turns with quality score < 60 (resets on score ≥ 60).
    pub(crate) consecutive_low_quality: u8,
    /// Value / ROI snapshot — updated on the obs poll cadence.
    pub(crate) value_snapshot: value_panel::ValueSnapshot,
    /// Whether a Tier-2 LLM quality review is in flight.
    pub(crate) quality_review_in_progress: bool,
    /// When Ctrl-Q was first pressed; used to detect hold ≥ 500ms.
    pub(crate) ctrl_q_pressed_at: Option<std::time::Instant>,
    /// Last 50 turn round-trip latencies in ms, used for p95/p99 computation.
    pub(crate) latency_samples: VecDeque<u64>,
    /// Cumulative input tokens for this session (updated on each turn done event).
    pub(crate) session_tokens_in: u64,
    /// Cumulative output tokens for this session.
    pub(crate) session_tokens_out: u64,
    /// True while the session config peek overlay is visible (toggled by Ctrl+P in scroll mode).
    pub(crate) show_session_peek: bool,
    /// Whether the Ctrl-S session browser overlay is open.
    pub(crate) session_browser_open: bool,
    /// Cursor row within the session browser overlay.
    pub(crate) session_browser_cursor: usize,
    /// Vim-style prompt editing mode (Insert vs Normal).
    pub(crate) vim_input_mode: InputMode,
    /// First key of a two-key vim sequence (e.g. `d` in `dd`, `g` in `gg`).
    pub(crate) pending_vim_key: Option<char>,
}

#[cfg(test)]
mod tests {

    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

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
    fn input_cursor_defaults_to_zero_in_make_state() {
        let state = make_state("s");
        assert_eq!(state.input_cursor, 0);
    }

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
}
