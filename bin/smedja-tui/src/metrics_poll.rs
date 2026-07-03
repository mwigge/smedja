use crate::metrics_view::{self};
use crate::state::AppState;

pub(crate) const METRICS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Window covered by the metrics fetch: the last 24h, in microseconds.
pub(crate) const METRICS_SINCE_WINDOW_MICROS: i64 = 24 * 3_600 * 1_000_000;

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

pub(crate) fn lsp_snapshot_from_rpc(
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
pub(crate) fn metrics_rows_from_summary(resp: &serde_json::Value) -> Vec<metrics_view::MetricsRow> {
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
pub(crate) fn metrics_poll_due(
    visible: bool,
    last: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    visible && last.is_none_or(|t| now.saturating_duration_since(t) >= METRICS_POLL_INTERVAL)
}

/// Toggles the metrics panel visibility. When the toggle makes the panel
/// visible, clears `last_metrics_poll` so the next event-loop tick fetches
/// immediately rather than waiting a full interval for the first paint.
pub(crate) fn toggle_metrics_view(state: &mut AppState) {
    state.panels.metrics = !state.panels.metrics;
    if state.panels.metrics {
        state.last_metrics_poll = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

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
}
