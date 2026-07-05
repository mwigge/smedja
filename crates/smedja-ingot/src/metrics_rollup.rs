//! Time-tiered metrics rollups over the cost ledger and audit log.
//!
//! Aggregates tokens, cost, turns, and error counts **per (runner, model)** into
//! one of five fixed time tiers (`raw` / `hourly` / `daily` / `weekly` /
//! `monthly`). The `model` dimension surfaces per-tier usage/cost (fable-plan vs
//! sonnet/haiku-implement vs opus-review) — the cost ledger records `model` per
//! turn, and the rollup carries it through instead of dropping it.
//!
//! Tokens, cost, and turns come from `cost_ledger`; error counts come from
//! `audit_events` rows with `status = 'error'`. The audit log has no model
//! dimension, so error rows land under the empty-string model (`""`). The two
//! grouped result sets are merged in Rust on `(bucket_start, runner, model)` so a
//! runner that errored without a cost row — or spent without erroring — still
//! appears.
//!
//! Aggregation is computed on read from source rows by default; an optional
//! idempotent [`materialise`] upserts the same computed buckets into the
//! `metrics_rollups` table for callers that want pre-aggregated reads. The table
//! is a derived cache, never a second source of truth.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, TimeZone as _, Timelike, Utc};
use serde::{Deserialize, Serialize};
use smedja_types::{Microdollars, Timestamp};

use crate::error::IngotError;
use crate::{Ingot, IngotHandle};

/// Microseconds in one second.
const MICROS_PER_SEC: i64 = 1_000_000;

/// A fixed time tier the rollup aggregates over.
///
/// Each tier truncates a microsecond timestamp to the start of its grid cell in
/// UTC via [`RollupTier::bucket_start`]; `Raw` keeps per-source granularity (no
/// truncation). Callers pass a tier, never raw SQL or arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RollupTier {
    /// Per-source granularity — the timestamp is its own bucket.
    Raw,
    /// Start of the hour (UTC).
    Hourly,
    /// Start of the day (00:00 UTC).
    Daily,
    /// ISO-week start (Monday 00:00 UTC).
    Weekly,
    /// First of the month (00:00 UTC).
    Monthly,
}

impl RollupTier {
    /// Returns the tier's wire string (`"raw"`, `"hourly"`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    /// Parses a tier from its wire string, returning `None` for any other input.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "raw" => Some(Self::Raw),
            "hourly" => Some(Self::Hourly),
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }

    /// Truncates `ts_micros` (microseconds since the Unix epoch) to the start of
    /// this tier's grid cell, in UTC, returning the bucket start in microseconds.
    ///
    /// `Raw` returns the input unchanged. `Hourly`/`Daily` floor to the start of
    /// the hour/day; `Weekly` floors to the ISO-week start (Monday 00:00 UTC);
    /// `Monthly` floors to the first of the month (00:00 UTC). An instant that
    /// already sits exactly on a boundary maps to itself.
    #[must_use]
    pub fn bucket_start(self, ts_micros: i64) -> i64 {
        if self == Self::Raw {
            return ts_micros;
        }
        // Split into whole seconds and the sub-second microsecond remainder so
        // calendar math runs on a chrono DateTime, then re-attach zero micros.
        let secs = ts_micros.div_euclid(MICROS_PER_SEC);
        let dt: DateTime<Utc> = Utc
            .timestamp_opt(secs, 0)
            .single()
            .unwrap_or_else(|| Utc.timestamp_nanos(0));
        let truncated = match self {
            // Raw handled above; listing it keeps the match exhaustive.
            Self::Raw => dt,
            Self::Hourly => dt
                .with_minute(0)
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))
                .unwrap_or(dt),
            Self::Daily => start_of_day(dt),
            Self::Weekly => {
                let days_since_monday = i64::from(dt.weekday().num_days_from_monday());
                start_of_day(dt) - Duration::days(days_since_monday)
            }
            Self::Monthly => Utc
                .with_ymd_and_hms(dt.year(), dt.month(), 1, 0, 0, 0)
                .single()
                .unwrap_or_else(|| start_of_day(dt)),
        };
        truncated.timestamp() * MICROS_PER_SEC
    }
}

/// Returns `dt` floored to 00:00:00.000000 of the same UTC day.
fn start_of_day(dt: DateTime<Utc>) -> DateTime<Utc> {
    dt.with_hour(0)
        .and_then(|d| d.with_minute(0))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(dt)
}

/// One aggregated `(tier, bucket_start, runner, model)` row.
///
/// Cost is kept as exact integer [`Microdollars`]; conversion to USD happens
/// only at the display boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsBucket {
    /// The tier this bucket belongs to.
    pub tier: RollupTier,
    /// Bucket start (microseconds since the Unix epoch), per [`RollupTier::bucket_start`].
    pub bucket_start: Timestamp,
    /// Runner name (e.g. `"claude"`, `"local"`).
    pub runner: String,
    /// Model id (e.g. `"claude-opus-4-8"`). Empty for error-only rows, which come
    /// from the audit log and carry no model dimension.
    pub model: String,
    /// Number of turns (cost-ledger rows) in this bucket.
    pub turns: i64,
    /// Sum of input tokens.
    pub input_tok: i64,
    /// Sum of output tokens.
    pub output_tok: i64,
    /// Exact total cost (microdollars).
    pub cost_usd: Microdollars,
    /// Number of `audit_events` rows with `status = 'error'` in this bucket.
    pub error_count: i64,
}

/// A mutable per-`(bucket_start, runner, model)` accumulator used while merging
/// the two grouped result sets.
#[derive(Default)]
struct Accumulator {
    turns: i64,
    input_tok: i64,
    output_tok: i64,
    cost_micros: i64,
    error_count: i64,
}

/// Computes rollup buckets for `tier` over `[since, until)` from source rows.
///
/// Tokens, cost, and turns are summed from `cost_ledger`; error counts are
/// counted from `audit_events` with `status = 'error'`. Both source timestamps
/// (`cost_ledger.created_at`, `audit_events.ts`) are bucketed with the same
/// [`RollupTier::bucket_start`], so identical instants land in identical
/// buckets. The merge is keyed on `(bucket_start, runner, model)`; results are
/// ordered by `bucket_start`, then `runner`, then `model`. Audit errors carry no
/// model dimension and land under the empty-string model (`""`).
///
/// The `until` bound is exclusive. The `runner` dimension for audit errors is
/// taken from `agent_name` when present, falling back to `actor`, mirroring how
/// the conversation rollups attribute agents.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if either source query fails.
pub(crate) fn compute(
    conn: &rusqlite::Connection,
    tier: RollupTier,
    since: Timestamp,
    until: Timestamp,
) -> Result<Vec<MetricsBucket>, IngotError> {
    let mut acc: BTreeMap<(i64, String, String), Accumulator> = BTreeMap::new();

    // ── cost_ledger: tokens / cost / turns per (bucket, runner, model) ───────
    let mut cost_stmt = conn.prepare(
        "SELECT runner, model, input_tok, output_tok, cost_usd, created_at \
         FROM cost_ledger \
         WHERE created_at >= ?1 AND created_at < ?2",
    )?;
    let cost_rows = cost_stmt.query_map(
        rusqlite::params![since.as_micros(), until.as_micros()],
        |row| {
            let runner: String = row.get(0)?;
            let model: String = row.get(1)?;
            let input_tok: i64 = row.get(2)?;
            let output_tok: i64 = row.get(3)?;
            let cost_micros = crate::read_micros(row, 4)?;
            let created_at = crate::read_micros(row, 5)?;
            Ok((
                runner,
                model,
                input_tok,
                output_tok,
                cost_micros,
                created_at,
            ))
        },
    )?;
    for row in cost_rows {
        let (runner, model, input_tok, output_tok, cost_micros, created_at) = row?;
        let bucket = tier.bucket_start(created_at);
        let entry = acc.entry((bucket, runner, model)).or_default();
        entry.turns += 1;
        entry.input_tok += input_tok;
        entry.output_tok += output_tok;
        entry.cost_micros = entry.cost_micros.saturating_add(cost_micros);
    }

    // ── audit_events: error counts per (bucket, runner) — no model dimension ─
    let mut err_stmt = conn.prepare(
        "SELECT COALESCE(agent_name, actor) AS runner, ts \
         FROM audit_events \
         WHERE status = 'error' AND ts >= ?1 AND ts < ?2",
    )?;
    let err_rows = err_stmt.query_map(
        rusqlite::params![since.as_micros(), until.as_micros()],
        |row| {
            let runner: String = row.get(0)?;
            let ts = crate::read_micros(row, 1)?;
            Ok((runner, ts))
        },
    )?;
    for row in err_rows {
        let (runner, ts) = row?;
        let bucket = tier.bucket_start(ts);
        // Errors have no model — key them under the empty-string model.
        let entry = acc.entry((bucket, runner, String::new())).or_default();
        entry.error_count += 1;
    }

    // BTreeMap iteration is ordered by (bucket_start, runner, model) — the order.
    let buckets = acc
        .into_iter()
        .map(|((bucket_start, runner, model), a)| MetricsBucket {
            tier,
            bucket_start: Timestamp::from_micros(bucket_start),
            runner,
            model,
            turns: a.turns,
            input_tok: a.input_tok,
            output_tok: a.output_tok,
            cost_usd: Microdollars::from_micros(a.cost_micros),
            error_count: a.error_count,
        })
        .collect();
    Ok(buckets)
}

/// Upserts the computed buckets for `tier` over `[since, until)` into
/// `metrics_rollups`, keyed on `(tier, bucket_start, runner, model)`.
///
/// Re-running with the same inputs reproduces identical rows (idempotent): the
/// `ON CONFLICT … DO UPDATE` overwrites every aggregate column with the freshly
/// computed value rather than accumulating.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the source queries or the upsert fail.
pub(crate) fn materialise(
    conn: &rusqlite::Connection,
    tier: RollupTier,
    since: Timestamp,
    until: Timestamp,
) -> Result<Vec<MetricsBucket>, IngotError> {
    let buckets = compute(conn, tier, since, until)?;
    for b in &buckets {
        conn.execute(
            "INSERT INTO metrics_rollups \
             (tier, bucket_start, runner, model, turns, input_tok, output_tok, cost_usd, error_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(tier, bucket_start, runner, model) DO UPDATE SET \
               turns       = excluded.turns, \
               input_tok   = excluded.input_tok, \
               output_tok  = excluded.output_tok, \
               cost_usd    = excluded.cost_usd, \
               error_count = excluded.error_count",
            rusqlite::params![
                b.tier.as_str(),
                b.bucket_start.as_micros(),
                b.runner,
                b.model,
                b.turns,
                b.input_tok,
                b.output_tok,
                b.cost_usd.as_micros(),
                b.error_count,
            ],
        )?;
    }
    Ok(buckets)
}

impl Ingot {
    /// Computes time-tiered metrics buckets for `tier` over `[since, until)`.
    ///
    /// Aggregates tokens, cost, and turns from `cost_ledger` and error counts
    /// from `audit_events` (`status = 'error'`) per `(bucket, runner)`, merging
    /// the two on `(bucket_start, runner)`. Buckets are computed on read from the
    /// source rows — there is no staleness and no background writer. Results are
    /// ordered by `bucket_start` then `runner`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if either source query fails.
    #[must_use = "check the Result and inspect the returned buckets"]
    pub fn metrics_rollup(
        &self,
        tier: RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        compute(&self.conn, tier, since, until)
    }

    /// Upserts the computed buckets for `tier` over `[epoch, until)` into the
    /// `metrics_rollups` cache, keyed on `(tier, bucket_start, runner)`.
    ///
    /// Materialises every bucket up to (but not including) `until`. Idempotent:
    /// re-running with the same `until` reproduces identical rows, and the stored
    /// rows equal `metrics_rollup(tier, epoch, until)`. The returned buckets are
    /// exactly what was stored.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the source queries or the upsert fail.
    #[must_use = "check the Result to confirm the rollups were materialised"]
    pub fn materialise_rollups(
        &self,
        tier: RollupTier,
        until: Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        materialise(
            &self.conn,
            tier,
            smedja_types::Timestamp::from_micros(0),
            until,
        )
    }
}

impl IngotHandle {
    /// Computes time-tiered metrics buckets for `tier` over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn metrics_rollup(
        &self,
        tier: RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.metrics_rollup(tier, since, until))
            .await
    }

    /// Upserts the computed buckets for `tier` over `[epoch, until)` into the
    /// `metrics_rollups` cache. Idempotent.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries or upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn materialise_rollups(
        &self,
        tier: RollupTier,
        until: Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.materialise_rollups(tier, until))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the micros for a UTC `Y-M-D h:m:s` instant.
    fn micros(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("valid instant")
            .timestamp()
            * MICROS_PER_SEC
    }

    #[test]
    fn raw_tier_is_identity() {
        let ts = micros(2026, 6, 24, 12, 34, 56);
        assert_eq!(RollupTier::Raw.bucket_start(ts), ts);
        // Including sub-second micros.
        assert_eq!(RollupTier::Raw.bucket_start(ts + 789), ts + 789);
    }

    #[test]
    fn hourly_floors_to_start_of_hour() {
        let ts = micros(2026, 6, 24, 12, 34, 56) + 789;
        assert_eq!(
            RollupTier::Hourly.bucket_start(ts),
            micros(2026, 6, 24, 12, 0, 0)
        );
        // Boundary instant maps to itself.
        let boundary = micros(2026, 6, 24, 12, 0, 0);
        assert_eq!(RollupTier::Hourly.bucket_start(boundary), boundary);
    }

    #[test]
    fn daily_floors_to_midnight() {
        let ts = micros(2026, 6, 24, 23, 59, 59);
        assert_eq!(
            RollupTier::Daily.bucket_start(ts),
            micros(2026, 6, 24, 0, 0, 0)
        );
        let boundary = micros(2026, 6, 24, 0, 0, 0);
        assert_eq!(RollupTier::Daily.bucket_start(boundary), boundary);
    }

    #[test]
    fn weekly_floors_to_iso_monday() {
        // 2026-06-24 is a Wednesday; its ISO week starts Monday 2026-06-22.
        let wednesday = micros(2026, 6, 24, 12, 0, 0);
        assert_eq!(
            RollupTier::Weekly.bucket_start(wednesday),
            micros(2026, 6, 22, 0, 0, 0)
        );
        // A Monday 00:00 boundary maps to itself (no jump to the previous week).
        let monday = micros(2026, 6, 22, 0, 0, 0);
        assert_eq!(RollupTier::Weekly.bucket_start(monday), monday);
        // Sunday is the last day of the same ISO week.
        let sunday = micros(2026, 6, 28, 23, 0, 0);
        assert_eq!(
            RollupTier::Weekly.bucket_start(sunday),
            micros(2026, 6, 22, 0, 0, 0)
        );
    }

    #[test]
    fn monthly_floors_to_first_of_month() {
        let ts = micros(2026, 6, 24, 12, 0, 0);
        assert_eq!(
            RollupTier::Monthly.bucket_start(ts),
            micros(2026, 6, 1, 0, 0, 0)
        );
        let boundary = micros(2026, 6, 1, 0, 0, 0);
        assert_eq!(RollupTier::Monthly.bucket_start(boundary), boundary);
        // Month length edge: 2026-02-28 floors to 2026-02-01.
        let feb = micros(2026, 2, 28, 23, 59, 59);
        assert_eq!(
            RollupTier::Monthly.bucket_start(feb),
            micros(2026, 2, 1, 0, 0, 0)
        );
    }

    #[test]
    fn tier_str_round_trips() {
        for tier in [
            RollupTier::Raw,
            RollupTier::Hourly,
            RollupTier::Daily,
            RollupTier::Weekly,
            RollupTier::Monthly,
        ] {
            assert_eq!(RollupTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(RollupTier::parse("yearly"), None);
    }

    // ── ingot-backed tests ───────────────────────────────────────────────────

    use crate::{AuditEvent, CostEntry, Ingot};
    use uuid::Uuid;

    fn cost_entry(runner: &str, cost_usd: f64, in_tok: i64, out_tok: i64, at: i64) -> CostEntry {
        CostEntry {
            id: Uuid::new_v4(),
            session_id: "s1".to_owned(),
            turn_n: 0,
            runner: runner.to_owned(),
            model: "m".to_owned(),
            input_tok: in_tok,
            output_tok: out_tok,
            cost_usd: Microdollars::from_usd_f64(cost_usd),
            created_at: Timestamp::from_micros(at),
        }
    }

    fn error_event(runner: &str, at: i64) -> AuditEvent {
        AuditEvent {
            id: Uuid::new_v4(),
            ts: Timestamp::from_micros(at),
            session_id: "s1".to_owned(),
            action_type: "llm".to_owned(),
            actor: runner.to_owned(),
            status: Some("error".to_owned()),
            ..AuditEvent::default()
        }
    }

    #[test]
    fn migration_22_creates_metrics_rollups_table() {
        let ingot = Ingot::open_in_memory().unwrap();
        let count: i64 = ingot
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'metrics_rollups'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "metrics_rollups table must exist after migration");
    }

    #[test]
    fn daily_rollup_groups_by_day_and_runner() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day1 = micros(2026, 1, 1, 9, 0, 0);
        let day2 = micros(2026, 1, 2, 9, 0, 0);
        // claude: two entries on day1, one on day2; local: one on day1.
        ingot
            .insert_cost(&cost_entry("claude", 0.01, 100, 50, day1))
            .unwrap();
        ingot
            .insert_cost(&cost_entry("claude", 0.02, 200, 60, day1 + 1_000_000))
            .unwrap();
        ingot
            .insert_cost(&cost_entry("claude", 0.03, 300, 70, day2))
            .unwrap();
        ingot
            .insert_cost(&cost_entry("local", 0.04, 400, 80, day1))
            .unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 3, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();

        // (day1, claude), (day1, local), (day2, claude) — ordered by bucket then runner.
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].runner, "claude");
        assert_eq!(
            buckets[0].bucket_start.as_micros(),
            micros(2026, 1, 1, 0, 0, 0)
        );
        assert_eq!(buckets[0].turns, 2);
        assert_eq!(buckets[0].input_tok, 300);
        assert_eq!(buckets[0].output_tok, 110);
        assert_eq!(buckets[0].cost_usd, Microdollars::from_micros(30_000));
        assert_eq!(buckets[1].runner, "local");
        assert_eq!(
            buckets[1].bucket_start.as_micros(),
            micros(2026, 1, 1, 0, 0, 0)
        );
        assert_eq!(buckets[2].runner, "claude");
        assert_eq!(
            buckets[2].bucket_start.as_micros(),
            micros(2026, 1, 2, 0, 0, 0)
        );
        assert_eq!(buckets[2].turns, 1);
    }

    #[test]
    fn error_events_populate_error_count() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_audit_event(&error_event("claude", day))
            .unwrap();
        ingot
            .insert_audit_event(&error_event("claude", day + 1_000_000))
            .unwrap();
        // A non-error event must not count.
        let mut ok = error_event("claude", day);
        ok.status = Some("ok".to_owned());
        ingot.insert_audit_event(&ok).unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].runner, "claude");
        assert_eq!(buckets[0].error_count, 2);
        assert_eq!(buckets[0].turns, 0, "no cost rows means zero turns");
    }

    #[test]
    fn cost_and_error_split_by_model_dimension() {
        // With `model` in the key, a cost row (model "m") and an error row (no
        // model → "") no longer merge: the cost lands under its model, the error
        // under the empty-string model. Ordered by model, "" sorts before "m".
        let ingot = Ingot::open_in_memory().unwrap();
        let at = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_cost(&cost_entry("claude", 0.05, 100, 50, at))
            .unwrap();
        ingot
            .insert_audit_event(&error_event("claude", at))
            .unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();
        assert_eq!(buckets.len(), 2, "cost and error split by model dimension");
        // (claude, "") — the error-only row.
        assert_eq!(buckets[0].runner, "claude");
        assert_eq!(buckets[0].model, "");
        assert_eq!(buckets[0].error_count, 1);
        assert_eq!(buckets[0].turns, 0);
        // (claude, "m") — the cost row.
        assert_eq!(buckets[1].model, "m");
        assert_eq!(buckets[1].turns, 1);
        assert_eq!(buckets[1].cost_usd, Microdollars::from_micros(50_000));
        assert_eq!(buckets[1].error_count, 0);
    }

    #[test]
    fn buckets_split_per_model_for_the_same_runner() {
        // Per-tier accounting: two models on the same runner produce two rows so
        // fable-plan / sonnet-implement / opus-review are separable.
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        let entry = |model: &str, cost: f64| CostEntry {
            id: Uuid::new_v4(),
            session_id: "s1".to_owned(),
            turn_n: 0,
            runner: "claude".to_owned(),
            model: model.to_owned(),
            input_tok: 100,
            output_tok: 50,
            cost_usd: Microdollars::from_usd_f64(cost),
            created_at: Timestamp::from_micros(day),
        };
        ingot.insert_cost(&entry("claude-fable-5", 0.01)).unwrap();
        ingot.insert_cost(&entry("claude-opus-4-8", 0.02)).unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();
        assert_eq!(buckets.len(), 2, "one row per model");
        // Ordered by model: "claude-fable-5" < "claude-opus-4-8".
        assert_eq!(buckets[0].model, "claude-fable-5");
        assert_eq!(buckets[0].cost_usd, Microdollars::from_micros(10_000));
        assert_eq!(buckets[1].model, "claude-opus-4-8");
        assert_eq!(buckets[1].cost_usd, Microdollars::from_micros(20_000));
    }

    #[test]
    fn agent_name_is_preferred_over_actor_for_error_runner() {
        let ingot = Ingot::open_in_memory().unwrap();
        let at = micros(2026, 1, 1, 9, 0, 0);
        let mut ev = error_event("actor-fallback", at);
        ev.agent_name = Some("coder-rust".to_owned());
        ingot.insert_audit_event(&ev).unwrap();

        let since = Timestamp::from_micros(micros(2026, 1, 1, 0, 0, 0));
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let buckets = ingot
            .metrics_rollup(RollupTier::Daily, since, until)
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].runner, "coder-rust");
    }

    #[test]
    fn materialise_then_read_equals_on_read() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_cost(&cost_entry("claude", 0.01, 100, 50, day))
            .unwrap();
        ingot
            .insert_audit_event(&error_event("claude", day))
            .unwrap();

        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));
        let on_read = ingot
            .metrics_rollup(RollupTier::Daily, Timestamp::from_micros(0), until)
            .unwrap();
        ingot.materialise_rollups(RollupTier::Daily, until).unwrap();

        // Read the persisted rows directly and compare to on-read.
        let mut stmt = ingot
            .conn
            .prepare(
                "SELECT bucket_start, runner, model, turns, input_tok, output_tok, cost_usd, error_count \
                 FROM metrics_rollups WHERE tier = 'daily' \
                 ORDER BY bucket_start, runner, model",
            )
            .unwrap();
        let stored: Vec<MetricsBucket> = stmt
            .query_map([], |row| {
                Ok(MetricsBucket {
                    tier: RollupTier::Daily,
                    bucket_start: Timestamp::from_micros(crate::read_micros(row, 0)?),
                    runner: row.get(1)?,
                    model: row.get(2)?,
                    turns: row.get(3)?,
                    input_tok: row.get(4)?,
                    output_tok: row.get(5)?,
                    cost_usd: Microdollars::from_micros(crate::read_micros(row, 6)?),
                    error_count: row.get(7)?,
                })
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(stored, on_read);
    }

    #[test]
    fn materialise_is_idempotent() {
        let ingot = Ingot::open_in_memory().unwrap();
        let day = micros(2026, 1, 1, 9, 0, 0);
        ingot
            .insert_cost(&cost_entry("claude", 0.01, 100, 50, day))
            .unwrap();
        let until = Timestamp::from_micros(micros(2026, 1, 2, 0, 0, 0));

        ingot.materialise_rollups(RollupTier::Daily, until).unwrap();
        let count_after_first: i64 = ingot
            .conn
            .query_row("SELECT COUNT(*) FROM metrics_rollups", [], |row| row.get(0))
            .unwrap();
        let cost_after_first: i64 = ingot
            .conn
            .query_row(
                "SELECT cost_usd FROM metrics_rollups WHERE runner = 'claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        ingot.materialise_rollups(RollupTier::Daily, until).unwrap();
        let count_after_second: i64 = ingot
            .conn
            .query_row("SELECT COUNT(*) FROM metrics_rollups", [], |row| row.get(0))
            .unwrap();
        let cost_after_second: i64 = ingot
            .conn
            .query_row(
                "SELECT cost_usd FROM metrics_rollups WHERE runner = 'claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count_after_first, count_after_second, "row count unchanged");
        assert_eq!(cost_after_first, cost_after_second, "values unchanged");
    }
}
