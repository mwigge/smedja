//! Metrics RPC handlers: `metrics.summary`.
//!
//! Exposes the local, ingot-backed time-tiered rollups (tokens, cost, turns,
//! error counts per runner) computed over the cost ledger and audit log. This is
//! the always-available, zero-dependency path; the external-OTel
//! `smedja-sre::metric_query` path (Prometheus range queries) is complementary
//! and only available when a metrics backend is deployed.

use serde_json::{json, Value};
use smedja_ingot::{IngotHandle, MetricsBucket, RollupTier};
use smedja_rpc::RpcError;
use smedja_types::Timestamp;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

/// Handles `metrics.summary`: returns time-tiered rollup buckets.
///
/// Parameters:
/// - `tier` (required): one of `raw` / `hourly` / `daily` / `weekly` / `monthly`.
/// - `since` (required): inclusive lower bound, microseconds since the Unix epoch.
/// - `until` (optional): exclusive upper bound, microseconds since the epoch;
///   defaults to the current time when omitted.
///
/// Cost is returned as USD `f64` at this display boundary; every other field is
/// an integer.
///
/// # Errors
///
/// Returns `missing_param` when `tier` is missing or unrecognised, or when
/// `since` is missing; returns an internal error when the ingot query fails.
pub(crate) async fn summary(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    summary_with(&state.ingot, &params).await
}

/// Core of `metrics.summary`, parameterised on the ingot handle alone so it is
/// testable without constructing a full [`HandlerState`].
///
/// # Errors
///
/// See [`summary`].
async fn summary_with(ig: &IngotHandle, params: &Value) -> Result<Value, RpcError> {
    let tier = params
        .get("tier")
        .and_then(Value::as_str)
        .and_then(RollupTier::parse)
        .ok_or_else(|| missing_param("tier"))?;
    let since = params
        .get("since")
        .and_then(Value::as_i64)
        .ok_or_else(|| missing_param("since"))?;
    let until = params
        .get("until")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| Timestamp::now().as_micros());

    let buckets = ig
        .metrics_rollup(
            tier,
            Timestamp::from_micros(since),
            Timestamp::from_micros(until),
        )
        .await
        .map_err(|e| ingot_err(&e))?;

    let buckets_json: Vec<Value> = buckets.iter().map(bucket_to_json).collect();
    Ok(json!({
        "tier": tier.as_str(),
        "buckets": buckets_json,
    }))
}

/// Serialises a [`MetricsBucket`], converting cost to USD `f64` at the boundary.
fn bucket_to_json(b: &MetricsBucket) -> Value {
    json!({
        "bucket_start": b.bucket_start.as_micros(),
        "runner": b.runner,
        "model": b.model,
        "turns": b.turns,
        "input_tok": b.input_tok,
        "output_tok": b.output_tok,
        "cost_usd": b.cost_usd.as_usd_f64(),
        "error_count": b.error_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{CostEntry, Ingot};
    use smedja_types::Microdollars;
    use uuid::Uuid;

    fn handle() -> IngotHandle {
        IngotHandle::new(Ingot::open_in_memory().unwrap())
    }

    fn cost_entry(runner: &str, at: i64) -> CostEntry {
        CostEntry {
            id: Uuid::new_v4(),
            session_id: "s1".to_owned(),
            turn_n: 0,
            runner: runner.to_owned(),
            model: "m".to_owned(),
            input_tok: 100,
            output_tok: 50,
            cost_usd: Microdollars::from_usd_f64(0.01),
            created_at: Timestamp::from_micros(at),
        }
    }

    #[tokio::test]
    async fn summary_returns_buckets_for_populated_ingot() {
        let ig = handle();
        // 2026-01-01 09:00:00 UTC in micros.
        let at = 1_767_258_000_000_000;
        ig.insert_cost(cost_entry("claude", at)).await.unwrap();

        let params = json!({
            "tier": "daily",
            "since": 0,
            "until": at + 86_400_000_000_i64,
        });
        let resp = summary_with(&ig, &params).await.unwrap();
        assert_eq!(resp["tier"], "daily");
        let buckets = resp["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0]["runner"], "claude");
        assert_eq!(buckets[0]["turns"], 1);
        assert_eq!(buckets[0]["input_tok"], 100);
        assert!((buckets[0]["cost_usd"].as_f64().unwrap() - 0.01).abs() < 1e-9);
    }

    #[tokio::test]
    async fn summary_missing_tier_is_missing_param() {
        let ig = handle();
        let params = json!({ "since": 0 });
        let err = summary_with(&ig, &params).await.unwrap_err();
        assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn summary_missing_since_is_missing_param() {
        let ig = handle();
        let params = json!({ "tier": "daily" });
        let err = summary_with(&ig, &params).await.unwrap_err();
        assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
    }
}
