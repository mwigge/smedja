//! Session bootstrap and terminal setup for the TUI.
//!
//! Everything the event loop needs to start — tracing/palette init, CLI
//! parsing, daemon connection, session resolution, the initial [`AppState`],
//! the connect banner + optional history replay, and terminal/raw-mode setup —
//! is assembled here and handed to `run_loop::run` as a [`Session`]. Moved
//! verbatim from `main.rs`; behavior is unchanged.

use super::*;

/// Everything constructed during bootstrap that the event loop then drives.
pub(crate) struct Session {
    pub(crate) state: AppState,
    pub(crate) client: Client,
    pub(crate) editor: rustyline::DefaultEditor,
    pub(crate) history_path: PathBuf,
    pub(crate) sock: PathBuf,
    pub(crate) terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    pub(crate) sigterm_rx: tokio::sync::watch::Receiver<bool>,
    pub(crate) guard: TerminalGuard,
}

/// Runs the full startup sequence and returns the assembled [`Session`].
#[allow(clippy::too_many_lines)] // sequential startup steps + one large AppState literal
pub(crate) async fn bootstrap() -> Result<Session> {
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
        turn_tokens_in: 0,
        turn_tokens_out: 0,
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

    // Connect banner — dim chrome so it never out-shouts the conversation.
    let banner_sock = sock.display().to_string();
    crate::push_chrome_line(&mut state.main_panel, format!("connected to {banner_sock}"));
    crate::push_chrome_line(
        &mut state.main_panel,
        format!("session {}", state.session_id),
    );
    crate::push_chrome_line(&mut state.main_panel, format!("provider: {}", state.runner));
    if let Some(ref m) = state.model {
        crate::push_chrome_line(&mut state.main_panel, format!("model: {m}"));
    }
    let tier_str = state.tier.as_deref().unwrap_or("fast");
    crate::push_chrome_line(&mut state.main_panel, format!("tier: {tier_str}"));
    crate::push_chrome_line(
        &mut state.main_panel,
        "type a message or /help for commands",
    );

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
    let terminal = Terminal::new(backend).context("create terminal")?;

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

    Ok(Session {
        state,
        client,
        editor,
        history_path,
        sock,
        terminal,
        sigterm_rx,
        guard: _guard,
    })
}
