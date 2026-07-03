//! `smj metrics` and `smj savings` — time-tiered rollups and token-economy savings.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::json;

use crate::util::connect;

/// Dispatches the `smj metrics` command.
pub(crate) async fn run_metrics(
    sock: &Path,
    tier: String,
    since: String,
    until: Option<String>,
    runner: Option<String>,
    json: bool,
) -> Result<()> {
    let mut client = connect(sock).await?;
    let now_micros = chrono::Utc::now().timestamp_micros();
    let since_micros = since_to_micros(&since, now_micros)?;
    let until_micros = match until {
        Some(spec) => Some(since_to_micros(&spec, now_micros)?),
        None => None,
    };
    let params = build_metrics_params(&tier, since_micros, until_micros);
    let resp = client
        .call("metrics.summary", params)
        .await
        .context("metrics.summary failed")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        for line in format_metrics_rows(&resp, runner.as_deref()) {
            println!("{line}");
        }
    }
    Ok(())
}

/// Dispatches the `smj savings` command.
pub(crate) async fn run_savings(
    sock: &Path,
    tier: String,
    since: String,
    until: Option<String>,
    json: bool,
) -> Result<()> {
    let mut client = connect(sock).await?;
    let now_micros = chrono::Utc::now().timestamp_micros();
    let since_micros = since_to_micros(&since, now_micros)?;
    let until_micros = match until {
        Some(spec) => Some(since_to_micros(&spec, now_micros)?),
        None => None,
    };
    let params = build_metrics_params(&tier, since_micros, until_micros);
    let resp = client
        .call("savings.summary", params)
        .await
        .context("savings.summary failed")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        for line in format_savings_rows(&resp) {
            println!("{line}");
        }
    }
    Ok(())
}

/// Parses a duration string (`7d`, `24h`, `30m`, `90s`, or bare seconds) into a
/// micros-since-epoch lower bound relative to `now_micros`.
///
/// A bare integer is interpreted as seconds. The result is `now_micros` minus
/// the parsed span, clamped at zero.
///
/// # Errors
///
/// Returns an error when the string is not a recognised duration.
fn since_to_micros(spec: &str, now_micros: i64) -> Result<i64> {
    let span_micros = duration_to_micros(spec)?;
    Ok(now_micros.saturating_sub(span_micros).max(0))
}

/// Parses a duration string into a span in microseconds.
///
/// Accepts a trailing unit suffix `d`/`h`/`m`/`s`; a bare integer is seconds.
fn duration_to_micros(spec: &str) -> Result<i64> {
    let spec = spec.trim();
    let (value_str, unit_secs) = match spec.chars().last() {
        Some('d') => (&spec[..spec.len() - 1], 86_400),
        Some('h') => (&spec[..spec.len() - 1], 3_600),
        Some('m') => (&spec[..spec.len() - 1], 60),
        Some('s') => (&spec[..spec.len() - 1], 1),
        _ => (spec, 1),
    };
    let value: i64 = value_str
        .parse()
        .with_context(|| format!("invalid duration: {spec}"))?;
    Ok(value.saturating_mul(unit_secs).saturating_mul(1_000_000))
}

/// Builds the `metrics.summary` request params from CLI flags.
///
/// `until` is omitted from the params when `None`, letting the daemon default it
/// to the current time.
fn build_metrics_params(
    tier: &str,
    since_micros: i64,
    until_micros: Option<i64>,
) -> serde_json::Value {
    let mut params = json!({ "tier": tier, "since": since_micros });
    if let Some(until) = until_micros {
        params["until"] = json!(until);
    }
    params
}

/// Renders the `metrics.summary` response into table rows, one per bucket,
/// optionally filtered to a single `runner`.
///
/// Returns the header line followed by one formatted line per bucket. The cost
/// column is the USD `f64` the daemon already converted at its display boundary.
fn format_metrics_rows(resp: &serde_json::Value, runner_filter: Option<&str>) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<22}  {:<12}  {:>5}  {:>9}  {:>9}  {:>11}  {:>6}",
        "BUCKET", "RUNNER", "TURNS", "INPUT", "OUTPUT", "COST", "ERRORS"
    )];
    let Some(buckets) = resp["buckets"].as_array() else {
        return lines;
    };
    for b in buckets {
        let runner = b["runner"].as_str().unwrap_or("-");
        if let Some(filter) = runner_filter {
            if runner != filter {
                continue;
            }
        }
        let bucket = format_bucket_start(b["bucket_start"].as_i64().unwrap_or(0));
        let turns = b["turns"].as_i64().unwrap_or(0);
        let input = b["input_tok"].as_i64().unwrap_or(0);
        let output = b["output_tok"].as_i64().unwrap_or(0);
        let cost = b["cost_usd"].as_f64().unwrap_or(0.0);
        let errors = b["error_count"].as_i64().unwrap_or(0);
        lines.push(format!(
            "{bucket:<22}  {runner:<12}  {turns:>5}  {input:>9}  {output:>9}  ${cost:>10.6}  {errors:>6}"
        ));
    }
    lines
}

/// Formats a micros-since-epoch bucket start as a UTC `YYYY-MM-DD HH:MM` label.
fn format_bucket_start(micros: i64) -> String {
    chrono::DateTime::from_timestamp_micros(micros).map_or_else(
        || micros.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// Renders the `savings.summary` response into report lines.
///
/// Leads with the efficiency-ratio headline and the compression / cache totals
/// kept as separate figures (cache savings are "input not re-paid" and are never
/// folded into the compression total), then lists one row per `(bucket, source)`.
fn format_savings_rows(resp: &serde_json::Value) -> Vec<String> {
    let ratio = resp["efficiency_ratio"].as_f64().unwrap_or(0.0);
    let compression = resp["compression_saved"].as_i64().unwrap_or(0);
    let cache = resp["cache_saved"].as_i64().unwrap_or(0);
    let mut lines = vec![
        format!("EFFICIENCY  {:.1}%", ratio * 100.0),
        format!("COMPRESSION {compression} tok  (filter + crusher + cold-context)"),
        format!("CACHE       {cache} tok  (input not re-paid)"),
        String::new(),
        format!("{:<22}  {:<14}  {:>12}", "BUCKET", "SOURCE", "TOKENS_SAVED"),
    ];
    let Some(buckets) = resp["buckets"].as_array() else {
        return lines;
    };
    for b in buckets {
        let bucket = format_bucket_start(b["bucket_start"].as_i64().unwrap_or(0));
        let source = b["source"].as_str().unwrap_or("-");
        let saved = b["tokens_saved"].as_i64().unwrap_or(0);
        lines.push(format!("{bucket:<22}  {source:<14}  {saved:>12}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Cmd};
    use clap::Parser as _;

    // --- smj metrics ---

    #[test]
    fn metrics_parses_subcommand_with_flags() {
        let cli = Cli::try_parse_from([
            "smj", "metrics", "--tier", "hourly", "--since", "24h", "--runner", "claude", "--json",
        ])
        .expect("metrics must parse");
        match cli.command {
            Cmd::Metrics {
                tier,
                since,
                until,
                runner,
                json,
            } => {
                assert_eq!(tier, "hourly");
                assert_eq!(since, "24h");
                assert_eq!(until, None);
                assert_eq!(runner.as_deref(), Some("claude"));
                assert!(json);
            }
            _ => panic!("expected Cmd::Metrics"),
        }
    }

    // --- smj savings ---

    #[test]
    fn savings_parses_subcommand_with_flags() {
        let cli = Cli::try_parse_from([
            "smj", "savings", "--tier", "weekly", "--since", "30d", "--json",
        ])
        .expect("savings must parse");
        match cli.command {
            Cmd::Savings {
                tier,
                since,
                until,
                json,
            } => {
                assert_eq!(tier, "weekly");
                assert_eq!(since, "30d");
                assert_eq!(until, None);
                assert!(json);
            }
            _ => panic!("expected Cmd::Savings"),
        }
    }

    #[test]
    fn savings_tier_defaults_to_daily() {
        let cli = Cli::try_parse_from(["smj", "savings"]).expect("savings must parse");
        match cli.command {
            Cmd::Savings { tier, since, .. } => {
                assert_eq!(tier, "daily");
                assert_eq!(since, "7d");
            }
            _ => panic!("expected Cmd::Savings"),
        }
    }

    #[test]
    fn format_savings_rows_keeps_cache_separate_from_compression() {
        let resp = json!({
            "tier": "daily",
            "compression_saved": 150,
            "cache_saved": 9000,
            "efficiency_ratio": 0.2,
            "buckets": [
                { "bucket_start": 1_767_225_600_000_000_i64, "source": "cache", "tokens_saved": 9000 },
                { "bucket_start": 1_767_225_600_000_000_i64, "source": "filter", "tokens_saved": 150 },
            ],
        });
        let lines = format_savings_rows(&resp);
        let joined = lines.join("\n");
        assert!(
            joined.contains("EFFICIENCY  20.0%"),
            "headline ratio: {joined}"
        );
        assert!(
            joined.contains("COMPRESSION 150 tok"),
            "compression total: {joined}"
        );
        assert!(
            joined.contains("CACHE       9000 tok"),
            "cache total: {joined}"
        );
        // The compression and cache totals are never summed into one figure.
        assert!(
            !joined.contains("9150"),
            "must not fold cache into compression"
        );
        assert!(joined.contains("cache"));
        assert!(joined.contains("filter"));
    }

    #[test]
    fn metrics_tier_defaults_to_daily() {
        let cli = Cli::try_parse_from(["smj", "metrics"]).expect("metrics must parse");
        match cli.command {
            Cmd::Metrics { tier, since, .. } => {
                assert_eq!(tier, "daily");
                assert_eq!(since, "7d");
            }
            _ => panic!("expected Cmd::Metrics"),
        }
    }

    #[test]
    fn since_to_micros_subtracts_duration() {
        // now = 1_000_000s in micros; 1d back = 86_400s earlier.
        let now = 1_000_000_000_000;
        assert_eq!(since_to_micros("1d", now).unwrap(), now - 86_400_000_000);
        assert_eq!(since_to_micros("2h", now).unwrap(), now - 7_200_000_000);
        assert_eq!(since_to_micros("30m", now).unwrap(), now - 1_800_000_000);
        assert_eq!(since_to_micros("45s", now).unwrap(), now - 45_000_000);
        // Bare integer is seconds.
        assert_eq!(since_to_micros("10", now).unwrap(), now - 10_000_000);
    }

    #[test]
    fn since_to_micros_clamps_at_zero() {
        assert_eq!(since_to_micros("100d", 0).unwrap(), 0);
    }

    #[test]
    fn since_to_micros_rejects_garbage() {
        assert!(since_to_micros("notaduration", 0).is_err());
    }

    #[test]
    fn build_metrics_params_omits_until_when_absent() {
        let p = build_metrics_params("daily", 123, None);
        assert_eq!(p["tier"], "daily");
        assert_eq!(p["since"], 123);
        assert!(p.get("until").is_none());
        let p2 = build_metrics_params("hourly", 1, Some(999));
        assert_eq!(p2["until"], 999);
    }

    #[test]
    fn format_metrics_rows_renders_known_response() {
        // 2026-01-01 00:00:00 UTC = 1_767_225_600_000_000 micros.
        let resp = json!({
            "tier": "daily",
            "buckets": [
                {
                    "bucket_start": 1_767_225_600_000_000_i64,
                    "runner": "claude",
                    "turns": 3,
                    "input_tok": 600,
                    "output_tok": 180,
                    "cost_usd": 0.06,
                    "error_count": 1
                },
                {
                    "bucket_start": 1_767_225_600_000_000_i64,
                    "runner": "local",
                    "turns": 1,
                    "input_tok": 400,
                    "output_tok": 80,
                    "cost_usd": 0.0,
                    "error_count": 0
                }
            ]
        });
        let rows = format_metrics_rows(&resp, None);
        assert_eq!(rows.len(), 3, "header + two buckets");
        assert!(rows[0].contains("BUCKET") && rows[0].contains("ERRORS"));
        assert!(rows[1].contains("2026-01-01 00:00"));
        assert!(rows[1].contains("claude"));
        assert!(rows[1].contains("$  0.060000"));
        assert!(rows[2].contains("local"));

        // Runner filter keeps only the matching bucket.
        let filtered = format_metrics_rows(&resp, Some("claude"));
        assert_eq!(filtered.len(), 2, "header + claude only");
        assert!(filtered[1].contains("claude"));
    }
}
