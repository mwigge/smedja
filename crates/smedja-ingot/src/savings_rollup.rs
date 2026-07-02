//! Time-tiered rollups over the tokens-saved ledger.
//!
//! Mirrors [`crate::metrics_rollup`] but keyed on `source` rather than `runner`:
//! aggregates `tokens_saved` grouped by `(tier, bucket_start, source)` over the
//! same five fixed [`RollupTier`] tiers, reusing [`RollupTier::bucket_start`] so
//! savings buckets align exactly with the billed buckets in `metrics_rollup`.
//!
//! Alongside per-source sums, the rollup computes an efficiency ratio
//! `saved / (saved + billed_input)` over a tier window, where `billed_input` is
//! the `cost_ledger.input_tok` sum over the same range. Cache savings
//! (`source = 'cache'`) are provider-reported "input not re-paid" and are kept
//! categorically distinct from compression savings (`filter` + `crusher` +
//! `cold-context`); the [`SavingsSummary`] headline never folds them together.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use smedja_types::Timestamp;

use crate::error::IngotError;
use crate::metrics_rollup::RollupTier;

/// The savings source recorded for provider prompt-cache reads.
///
/// Kept distinct from compression sources because a cache read is input not
/// re-paid, not content compressed away.
pub const CACHE_SOURCE: &str = "cache";

/// One aggregated `(tier, bucket_start, source)` savings row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavingsBucket {
    /// The tier this bucket belongs to.
    pub tier: RollupTier,
    /// Bucket start (microseconds since the Unix epoch), per [`RollupTier::bucket_start`].
    pub bucket_start: Timestamp,
    /// The saver that produced the rows in this bucket.
    pub source: String,
    /// Sum of `tokens_saved` for this `(bucket, source)`.
    pub tokens_saved: i64,
}

/// The headline split for a savings window: compression and cache totals kept
/// as separate figures, plus the efficiency ratio.
///
/// `compression_saved` sums only the compression sources (`filter`, `crusher`,
/// `cold-context`); `cache_saved` is the [`CACHE_SOURCE`] total. They are never
/// summed into one number — folding a caching win into the compression story
/// would misrepresent both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavingsSummary {
    /// Per-`(bucket, source)` rows over the window, ordered by `bucket_start` then `source`.
    pub buckets: Vec<SavingsBucket>,
    /// Sum of compression savings (`filter` + `crusher` + `cold-context`).
    pub compression_saved: i64,
    /// Sum of cache savings (`source = 'cache'`).
    pub cache_saved: i64,
    /// Efficiency ratio `saved / (saved + billed_input)` over the window.
    pub efficiency_ratio: f64,
}

/// Returns `true` when `source` is a compression saver (everything except cache).
#[must_use]
pub fn is_compression_source(source: &str) -> bool {
    source != CACHE_SOURCE
}

/// Computes savings buckets for `tier` over `[since, until)` from the ledger.
///
/// Sums `tokens_saved` from `tokens_saved_ledger` per `(bucket, source)`,
/// bucketing `created_at` with [`RollupTier::bucket_start`]. The `until` bound is
/// exclusive. Results are ordered by `bucket_start` then `source`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the source query fails.
pub(crate) fn compute(
    conn: &rusqlite::Connection,
    tier: RollupTier,
    since: Timestamp,
    until: Timestamp,
) -> Result<Vec<SavingsBucket>, IngotError> {
    let mut acc: BTreeMap<(i64, String), i64> = BTreeMap::new();

    let mut stmt = conn.prepare(
        "SELECT source, tokens_saved, created_at \
         FROM tokens_saved_ledger \
         WHERE created_at >= ?1 AND created_at < ?2",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![since.as_micros(), until.as_micros()],
        |row| {
            let source: String = row.get(0)?;
            let tokens_saved: i64 = row.get(1)?;
            let created_at = crate::read_micros(row, 2)?;
            Ok((source, tokens_saved, created_at))
        },
    )?;
    for row in rows {
        let (source, tokens_saved, created_at) = row?;
        let bucket = tier.bucket_start(created_at);
        let entry = acc.entry((bucket, source)).or_default();
        *entry = entry.saturating_add(tokens_saved);
    }

    // BTreeMap iteration is ordered by (bucket_start, source) — the required order.
    let buckets = acc
        .into_iter()
        .map(|((bucket_start, source), tokens_saved)| SavingsBucket {
            tier,
            bucket_start: Timestamp::from_micros(bucket_start),
            source,
            tokens_saved,
        })
        .collect();
    Ok(buckets)
}

/// Returns the sum of `cost_ledger.input_tok` over `[since, until)`.
///
/// This is the efficiency ratio's denominator term `billed_input`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn billed_input(
    conn: &rusqlite::Connection,
    since: Timestamp,
    until: Timestamp,
) -> Result<i64, IngotError> {
    let total = conn.query_row(
        "SELECT COALESCE(SUM(input_tok), 0) FROM cost_ledger \
         WHERE created_at >= ?1 AND created_at < ?2",
        rusqlite::params![since.as_micros(), until.as_micros()],
        |row| row.get(0),
    )?;
    Ok(total)
}

/// Computes the efficiency ratio `saved / (saved + billed_input)` over `[since, until)`.
///
/// `saved` is the sum of all `tokens_saved` rows in the window (every source);
/// `billed_input` is [`billed_input`]. When `saved + billed_input` is zero, the
/// ratio is `0.0` (an empty window is neither efficient nor inefficient).
///
/// # Errors
///
/// Returns [`IngotError::Db`] if either query fails.
#[allow(clippy::cast_precision_loss)] // token counts stay well below 2^53
pub(crate) fn efficiency_ratio(
    conn: &rusqlite::Connection,
    since: Timestamp,
    until: Timestamp,
) -> Result<f64, IngotError> {
    let saved: i64 = conn.query_row(
        "SELECT COALESCE(SUM(tokens_saved), 0) FROM tokens_saved_ledger \
         WHERE created_at >= ?1 AND created_at < ?2",
        rusqlite::params![since.as_micros(), until.as_micros()],
        |row| row.get(0),
    )?;
    let billed = billed_input(conn, since, until)?;
    let denom = saved.saturating_add(billed);
    if denom == 0 {
        return Ok(0.0);
    }
    Ok(saved as f64 / denom as f64)
}

/// Computes the full [`SavingsSummary`] for `tier` over `[since, until)`.
///
/// Carries the per-`(bucket, source)` rows plus the headline split: the
/// compression total (`filter` + `crusher` + `cold-context`) and the cache total
/// kept as separate figures, never summed, and the efficiency ratio.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if any source query fails.
pub(crate) fn summary(
    conn: &rusqlite::Connection,
    tier: RollupTier,
    since: Timestamp,
    until: Timestamp,
) -> Result<SavingsSummary, IngotError> {
    let buckets = compute(conn, tier, since, until)?;
    let mut compression_saved = 0i64;
    let mut cache_saved = 0i64;
    for b in &buckets {
        if is_compression_source(&b.source) {
            compression_saved = compression_saved.saturating_add(b.tokens_saved);
        } else {
            cache_saved = cache_saved.saturating_add(b.tokens_saved);
        }
    }
    let efficiency_ratio = efficiency_ratio(conn, since, until)?;
    Ok(SavingsSummary {
        buckets,
        compression_saved,
        cache_saved,
        efficiency_ratio,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CostEntry, Ingot, TokensSavedEntry};
    use chrono::{TimeZone as _, Utc};
    use smedja_types::Microdollars;
    use uuid::Uuid;

    const MICROS_PER_SEC: i64 = 1_000_000;

    fn micros(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("valid instant")
            .timestamp()
            * MICROS_PER_SEC
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
            change_name: None,
        }
    }

    #[test]
    fn savings_rollup_groups_by_day_and_source() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        // filter + cache on the same UTC day, in two rows each.
        ingot
            .insert_tokens_saved(&saved("filter", 100, day))
            .unwrap();
        ingot
            .insert_tokens_saved(&saved("filter", 40, day + 1_000_000))
            .unwrap();
        ingot
            .insert_tokens_saved(&saved("cache", 500, day))
            .unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .savings_rollup(RollupTier::Daily, since, until)
            .unwrap();

        // (day, cache), (day, filter) — ordered by bucket then source.
        assert_eq!(buckets.len(), 2);
        let day_start = micros(2026, 1, 1, 0, 0, 0);
        assert_eq!(buckets[0].source, "cache");
        assert_eq!(buckets[0].tokens_saved, 500);
        assert_eq!(buckets[0].bucket_start.as_micros(), day_start);
        assert_eq!(buckets[1].source, "filter");
        assert_eq!(buckets[1].tokens_saved, 140);
        assert_eq!(buckets[1].bucket_start.as_micros(), day_start);
        // Both share the same bucket_start (start of the UTC day).
        assert_eq!(buckets[0].bucket_start, buckets[1].bucket_start);
    }

    #[test]
    fn efficiency_ratio_is_saved_over_saved_plus_billed_input() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        // saved = 200, billed_input = 800 → 0.2.
        ingot
            .insert_tokens_saved(&saved("filter", 200, day))
            .unwrap();
        ingot.insert_cost(&cost(800, day)).unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let ratio = ingot
            .efficiency_ratio(RollupTier::Daily, since, until)
            .unwrap();
        assert!((ratio - 0.2).abs() < 1e-9, "expected 0.2, got {ratio}");
    }

    #[test]
    fn efficiency_ratio_empty_window_is_zero() {
        let ingot = Ingot::open_in_memory().unwrap();
        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let ratio = ingot
            .efficiency_ratio(RollupTier::Daily, since, until)
            .unwrap();
        assert!((ratio - 0.0).abs() < 1e-12);
    }

    #[test]
    fn headline_keeps_cache_separate_from_compression() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        // Compression: filter 100 + crusher 30 + cold-context 20 = 150.
        ingot
            .insert_tokens_saved(&saved("filter", 100, day))
            .unwrap();
        ingot
            .insert_tokens_saved(&saved("crusher", 30, day))
            .unwrap();
        ingot
            .insert_tokens_saved(&saved("cold-context", 20, day))
            .unwrap();
        // Cache is a separate figure.
        ingot
            .insert_tokens_saved(&saved("cache", 9000, day))
            .unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let s = ingot
            .savings_summary(RollupTier::Daily, since, until)
            .unwrap();

        assert_eq!(
            s.compression_saved, 150,
            "compression total must include only filter + crusher + cold-context"
        );
        assert_eq!(s.cache_saved, 9000, "cache must be a separate figure");
        // The two are never folded into one number.
        assert_ne!(s.compression_saved, s.compression_saved + s.cache_saved);
    }
}
