//! Savings RPC handlers: `savings.summary`.
//!
//! Exposes the ingot-backed time-tiered savings rollup (tokens saved per source
//! over the five fixed tiers) plus the headline split: the compression total
//! (`filter` + `crusher` + `cold-context`) and the cache total kept as separate
//! figures, and the efficiency ratio `saved / (saved + billed_input)`. This is
//! the single shared backend for the `st-statusbar` segment, the TUI metrics
//! panel, and the `smj savings` CLI companion.
//!
//! Cache savings are provider-reported "input not re-paid", categorically
//! distinct from compression savings; the response never folds them into one
//! number.

use serde_json::{json, Value};
use smedja_ingot::{IngotHandle, RollupTier, SavingsBucket};
use smedja_rpc::RpcError;
use smedja_types::Timestamp;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

/// Handles `savings.summary`: returns per-source savings buckets, the
/// compression/cache split, and the efficiency ratio.
///
/// Parameters:
/// - `tier` (required): one of `raw` / `hourly` / `daily` / `weekly` / `monthly`.
/// - `since` (required): inclusive lower bound, microseconds since the Unix epoch.
/// - `until` (optional): exclusive upper bound, microseconds since the epoch;
///   defaults to the current time when omitted.
///
/// # Errors
///
/// Returns `missing_param` when `tier` is missing or unrecognised, or when
/// `since` is missing; returns an internal error when the ingot query fails.
pub(crate) async fn summary(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    summary_with(&state.ingot, &params).await
}

/// Core of `savings.summary`, parameterised on the ingot handle alone so it is
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

    let since_ts = Timestamp::from_micros(since);
    let until_ts = Timestamp::from_micros(until);
    let summary = ig
        .savings_summary(tier, since_ts, until_ts)
        .await
        .map_err(|e| ingot_err(&e))?;

    let buckets_json: Vec<Value> = summary.buckets.iter().map(bucket_to_json).collect();
    Ok(json!({
        "tier": tier.as_str(),
        "buckets": buckets_json,
        "compression_saved": summary.compression_saved,
        "cache_saved": summary.cache_saved,
        "efficiency_ratio": summary.efficiency_ratio,
    }))
}

/// Serialises a [`SavingsBucket`].
fn bucket_to_json(b: &SavingsBucket) -> Value {
    json!({
        "bucket_start": b.bucket_start.as_micros(),
        "source": b.source,
        "tokens_saved": b.tokens_saved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{CostEntry, Ingot, TokensSavedEntry};
    use smedja_types::Microdollars;
    use uuid::Uuid;

    fn handle() -> IngotHandle {
        IngotHandle::new(Ingot::open_in_memory().unwrap())
    }

    fn saved(source: &str, tokens: i64, at: i64) -> TokensSavedEntry {
        TokensSavedEntry {
            id: Uuid::new_v4(),
            session_id: "s1".to_owned(),
            turn_n: 0,
            command: "x".to_owned(),
            tokens_saved: tokens,
            source: source.to_owned(),
            created_at: Timestamp::from_micros(at),
        }
    }

    fn cost(input_tok: i64, at: i64) -> CostEntry {
        CostEntry {
            id: Uuid::new_v4(),
            session_id: "s1".to_owned(),
            turn_n: 0,
            runner: "claude".to_owned(),
            model: "m".to_owned(),
            input_tok,
            output_tok: 0,
            cost_usd: Microdollars::from_micros(0),
            created_at: Timestamp::from_micros(at),
        }
    }

    #[tokio::test]
    async fn summary_returns_per_source_buckets_and_split() {
        let ig = handle();
        // 2026-01-01 09:00:00 UTC in micros.
        let at = 1_767_258_000_000_000;
        ig.insert_tokens_saved(saved("filter", 100, at))
            .await
            .unwrap();
        ig.insert_tokens_saved(saved("cache", 700, at))
            .await
            .unwrap();
        ig.insert_cost(cost(300, at)).await.unwrap();

        let params = json!({
            "tier": "daily",
            "since": 0,
            "until": at + 86_400_000_000_i64,
        });
        let resp = summary_with(&ig, &params).await.unwrap();
        assert_eq!(resp["tier"], "daily");
        let buckets = resp["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2, "one bucket per source");
        // Compression and cache are reported as separate figures.
        assert_eq!(resp["compression_saved"], 100);
        assert_eq!(resp["cache_saved"], 700);
        // saved=800, billed_input=300 → 800 / 1100.
        let ratio = resp["efficiency_ratio"].as_f64().unwrap();
        assert!((ratio - 800.0 / 1100.0).abs() < 1e-9, "got {ratio}");
    }

    #[tokio::test]
    async fn summary_missing_tier_is_missing_param() {
        let ig = handle();
        let params = json!({ "since": 0 });
        let err = summary_with(&ig, &params).await.unwrap_err();
        assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
    }
}
