//! Shared test fixtures and builders for the TUI crate's unit tests.
//!
//! Centralising `make_state` (and the `render_frame` buffer helper) here lets
//! every split test file pull setup from one place instead of leaning on
//! crate-root re-exports via `use super::*`.

use std::collections::VecDeque;
use std::path::PathBuf;

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use crate::render::render;
use crate::state::{AppState, PanelVisibility};
use crate::{
    action_log, blocks, fleet_panel, main_panel, metrics_view, obs_panel, quality_panel, staging,
    trace_waterfall, value_panel, viz,
};

/// Constructs a minimal `AppState` for testing without a daemon connection.
#[allow(clippy::too_many_lines)]
pub(crate) fn make_state(session_id: &str) -> AppState {
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
        diff_split_view: false,
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
            fleet: false,
        },
        metrics_snapshot: Vec::new(),
        tier_snapshot: Vec::new(),
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
        last_graph_poll: None,
        stream_rx: None,
        upgrade_rx: None,
        current_thinking: String::new(),
        thinking_steps: Vec::new(),
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
        turn_tokens_in: 0,
        turn_tokens_out: 0,
        quality_score_sum: 0,
        quality_score_count: 0,
        file_picker_open: false,
        file_picker_dir: std::path::PathBuf::new(),
        file_picker_entries: Vec::new(),
        file_picker_cursor: 0,
        show_session_peek: false,
        render_mode: viz::RenderMode::Block,
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
    }
}

/// Renders `state` to an 80×24 `TestBackend` and returns the buffer.
pub(crate) fn render_frame(state: &mut AppState) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal.backend().buffer().clone()
}
