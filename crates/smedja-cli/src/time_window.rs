use anyhow::{Context as _, Result};
use serde_json::json;
pub(crate) fn since_to_micros(spec: &str, now_micros: i64) -> Result<i64> {
    let span_micros = duration_to_micros(spec)?;
    Ok(now_micros.saturating_sub(span_micros).max(0))
}

pub(crate) fn duration_to_micros(spec: &str) -> Result<i64> {
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

pub(crate) fn build_metrics_params(
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
