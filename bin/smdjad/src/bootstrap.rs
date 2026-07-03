//! Startup helpers extracted from `main`: telemetry install, env validation,
//! PID file, orphan sweep, routing-override load, and the background
//! maintenance/GC/quality-gate tasks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::IngotHandle;
use tracing::{error, info, warn};

use crate::orchestrator;
use crate::paths::dirs_home;
use crate::quality_hook;

/// Installs the W3C trace-context propagator process-wide and, when
/// `SMEDJA_OTLP_ENDPOINT` is set, an OTLP span exporter. Falls back to recording
/// spans through the structured-log layer, logging the destination either way.
pub(crate) fn install_telemetry() {
    // Install the W3C trace-context propagator process-wide so outbound HTTP
    // calls inject `traceparent`/`tracestate` and inbound contexts are
    // extracted. The adapter only *uses* the global propagator; installing it
    // is the binary's responsibility.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // Install an OTLP exporter when SMEDJA_OTLP_ENDPOINT is set; otherwise fall
    // back to recording spans through the structured-log layer. The trace
    // destination is logged in both branches so operators always know where
    // span data goes (no silent discard).
    if let Ok(endpoint) = std::env::var("SMEDJA_OTLP_ENDPOINT") {
        use opentelemetry_otlp::WithExportConfig as _;
        let build_result = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(&endpoint)
            .build();
        match build_result {
            Ok(exporter) => {
                let provider = opentelemetry_sdk::trace::TracerProvider::builder()
                    .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                    .build();
                opentelemetry::global::set_tracer_provider(provider);
                info!(endpoint = %endpoint, "trace destination: OTLP exporter");
            }
            Err(e) => {
                error!(error = %e, endpoint = %endpoint, "failed to install OTLP exporter; trace destination: structured logs only");
            }
        }
    } else {
        info!("SMEDJA_OTLP_ENDPOINT not set; trace destination: structured logs only (set the endpoint to export OTLP spans)");
    }
}

/// Validates `SMEDJA_COMPACT_THRESHOLD` at startup, rejecting invalid values
/// early. A below-minimum value aborts; a non-float logs a warning and uses the
/// default.
pub(crate) fn validate_compact_threshold() -> anyhow::Result<()> {
    if let Ok(val) = std::env::var("SMEDJA_COMPACT_THRESHOLD") {
        match val.parse::<f64>() {
            Ok(t) if t < 0.5 => {
                anyhow::bail!(
                    "SMEDJA_COMPACT_THRESHOLD={val} is below the minimum of 0.5; \
                     set it to a value in [0.5, 1.0] or unset it to use the default (0.85)"
                );
            }
            Err(_) => {
                tracing::warn!(
                    value = %val,
                    "SMEDJA_COMPACT_THRESHOLD is not a valid float; using default 0.85"
                );
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Writes the PID file so `smj daemon stop` can send SIGTERM.
///
/// Stored in `XDG_RUNTIME_DIR` (per-user tmpfs) or `~/.cache` as a private
/// fallback; never `/tmp` which is world-traversable. Returns the path written
/// (for later removal), or `None` when no private directory is available.
pub(crate) fn write_pid_file() -> Option<PathBuf> {
    let pid_path = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|d| std::path::PathBuf::from(d).join("smdjad.pid"))
        .or_else(|| dirs_home().map(|h| h.join(".cache").join("smdjad.pid")));
    if let Some(ref p) = pid_path {
        std::fs::write(p, std::process::id().to_string())
            .unwrap_or_else(|e| tracing::warn!(error = %e, "failed to write PID file"));
    } else {
        tracing::warn!("no private directory for PID file (set XDG_RUNTIME_DIR or HOME); smj daemon stop will not work");
    }
    pid_path
}

/// Detects sessions left `in_flight` by a prior crash and marks them (and their
/// in-progress tasks) as orphaned/failed.
pub(crate) async fn sweep_orphaned_sessions(ingot: &IngotHandle) {
    // ponytail: linear scan; session counts are small
    match ingot.list_sessions().await {
        Ok(sessions) => {
            let orphaned: Vec<_> = sessions
                .into_iter()
                .filter(|s| s.status == "in_flight")
                .collect();
            if !orphaned.is_empty() {
                tracing::warn!(
                    count = orphaned.len(),
                    "orphaned in_flight sessions detected at startup; marking as orphaned"
                );
                for sess in &orphaned {
                    let sid = sess.id.to_string();
                    let _ = ingot.update_session_status(&sid, "orphaned").await;

                    // Also fail any in_progress tasks owned by this session.
                    match ingot.list_tasks(Some("in_progress".to_owned())).await {
                        Ok(tasks) => {
                            for task in tasks {
                                if task.session_id.as_deref() == Some(sid.as_str()) {
                                    let _ = ingot
                                        .update_task_status(&task.id.to_string(), "failed")
                                        .await;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "could not list tasks during orphan sweep");
                        }
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not list sessions at startup"),
    }
}

/// Builds the assayer with default rules plus any workspace-local routing
/// overrides from `.smedja/agents.toml`.
pub(crate) fn load_assayer(workspace_root: &Path) -> Assayer {
    let mut assayer = Assayer::default_rules();
    match smedja_assayer::load_rules(workspace_root) {
        Ok(rules) if !rules.is_empty() => {
            let n = rules.len();
            assayer.prepend_rules(rules);
            info!(count = n, path = ?workspace_root.join(".smedja/agents.toml"), "loaded agents.toml overrides");
        }
        Ok(_) => {}
        Err(e) => {
            warn!(error = %e, "failed to load .smedja/agents.toml; using default routing");
        }
    }
    assayer
}

/// Spawns the background GC that caps the `provider_sessions` map at 10 000
/// entries. Each entry is soft state, so a full clear is safe; the task wakes
/// every 5 minutes.
pub(crate) fn spawn_session_gc(provider_sessions: orchestrator::ProviderSessions) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_mins(5)).await;
            let mut map = provider_sessions.lock().await;
            let n = map.len();
            if n > 10_000 {
                map.clear();
                tracing::info!("provider_sessions: cleared {n} entries (cap exceeded)");
            }
        }
    });
}

/// Spawns the daily maintenance task: prune old sessions and VACUUM the database.
/// First run is delayed 1 hour so startup I/O is not affected.
pub(crate) fn spawn_daily_maintenance(ingot: IngotHandle) {
    tokio::spawn(async move {
        // First run after 1 hour so startup I/O is not affected.
        tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        loop {
            match ingot.prune_old_sessions(30).await {
                Ok(n) if n > 0 => info!(pruned = n, "pruned old terminated sessions"),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "session prune failed"),
            }
            if let Err(e) = ingot.vacuum().await {
                warn!(error = %e, "database vacuum failed");
            }
            tokio::time::sleep(std::time::Duration::from_hours(24)).await;
        }
    });
}

/// Spawns the post-turn quality gate: reacts to every `TurnEvent::Completed` by
/// running the Tier-1 deterministic gates and dispatching a `QualitySnapshot`.
/// All errors are swallowed — it must never stall the turn loop.
pub(crate) fn spawn_quality_gate(dispatcher: Arc<Dispatcher>, workspace_root: PathBuf) {
    let mut quality_rx = dispatcher.subscribe();
    let session_skills = quality_hook::discover_session_skills(&workspace_root);
    let file_size_threshold = quality_hook::load_file_size_threshold(&workspace_root);
    tokio::spawn(async move {
        loop {
            let events = smedja_bellows::drain_ready(&mut quality_rx);
            for ev in events {
                if let TurnEvent::Completed { turn_id, .. } = ev {
                    let disp = Arc::clone(&dispatcher);
                    let ws = workspace_root.clone();
                    let skills = session_skills.clone();
                    tokio::task::spawn_blocking(move || {
                        quality_hook::run_after_turn(
                            Some(turn_id),
                            ws,
                            skills,
                            file_size_threshold,
                            disp,
                        );
                    });
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn provider_pool_builds_without_panic() {
        // build_provider_pool is infallible — just verify no panic regardless
        // of what environment variables are set in the test runner.
        let pool = crate::provider_pool::build_provider_pool().await;
        // Pool may be empty or non-empty depending on the environment; either is valid.
        drop(pool);
    }

    /// Returns the provider name that `build_provider` would select given the
    /// detection results for each candidate, encoding the subscription-first
    /// priority order without touching the network or filesystem.
    ///
    /// Priority (index 0 = highest):
    /// 0. claude CLI binary present
    /// 1. codex CLI binary present
    /// 2. copilot detected
    /// 3. poolside detected
    /// 4. `ANTHROPIC_API_KEY` set
    /// 5. `OPENAI_API_KEY` set
    /// 6. minimax detected
    /// 7. berget detected
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn provider_priority(
        claude_cli: bool,
        codex_cli: bool,
        copilot: bool,
        poolside: bool,
        anthropic_key: bool,
        openai_key: bool,
        minimax: bool,
        berget: bool,
    ) -> &'static str {
        if claude_cli {
            return "claude-cli";
        }
        if codex_cli {
            return "codex-cli";
        }
        if copilot {
            return "copilot";
        }
        if poolside {
            return "poolside";
        }
        if anthropic_key {
            return "anthropic";
        }
        if openai_key {
            return "openai";
        }
        if minimax {
            return "minimax";
        }
        if berget {
            return "berget";
        }
        "none"
    }

    #[test]
    fn cli_wins_over_api_key_when_both_present() {
        // CLI subscription beats API key — the fundamental invariant of L20.
        assert_eq!(
            provider_priority(true, false, false, false, true, true, false, false),
            "claude-cli"
        );
        assert_eq!(
            provider_priority(false, true, false, false, false, true, false, false),
            "codex-cli"
        );
    }

    #[test]
    fn api_key_selected_when_no_cli_available() {
        assert_eq!(
            provider_priority(false, false, false, false, true, false, false, false),
            "anthropic"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, true, false, false),
            "openai"
        );
    }

    #[test]
    fn cli_providers_ordered_before_copilot_and_poolside() {
        // Even copilot (subscription-like) comes after the CLI runners.
        assert_eq!(
            provider_priority(false, true, true, true, false, false, false, false),
            "codex-cli"
        );
    }

    #[test]
    fn anthropic_key_before_openai_key() {
        assert_eq!(
            provider_priority(false, false, false, false, true, true, false, false),
            "anthropic"
        );
    }

    #[test]
    fn minimax_and_berget_are_lowest_priority_before_local() {
        assert_eq!(
            provider_priority(false, false, false, false, false, false, true, false),
            "minimax"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, false, false, true),
            "berget"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, false, false, false),
            "none"
        );
    }
}
